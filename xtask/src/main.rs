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
//!   필수 locale key completeness, fallback, CJK path, layout smoke tests를 실행하고,
//!   `crates/` 전역에서 코드가 참조하는 리터럴 i18n 키가 5개 로케일 전부에 있는지도
//!   대조한다(로케일 "간" 짝맞춤만으로는 5개 로케일 모두에 없는 키를 못 잡는다 —
//!   docs/superpowers/specs/2026-08-19-i18n-key-guard.md 참고).
//!
//! `cargo run -p xtask -- bg01-deterministic-gate`
//!   하드웨어 실측, trusted-signing 실행, 실제 외부 계정 smoke를 제외한 BG01 production
//!   gate를 한 번에 실행한다.
//!
//! Cargo metadata가 해석한 모든 workspace-local 의존을 본다(외부 crate는 무관). crate 식별은
//! 디렉터리명 기준(예: crates/core의 패키지명은 deppy-core지만 여기선 "core").

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use quote::ToTokens as _;

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
        "bg01-deterministic-gate" => bg01_deterministic_gate(),
        other => bail!(
            "알 수 없는 명령 '{other}' — 사용법: cargo run -p xtask -- check-deps|check-boundary|smoke-db-migrations|security-scan|perf-smoke|od01-failure-matrix|i18n-check|bg01-deterministic-gate"
        ),
    }
}

fn bg01_deterministic_gate() -> anyhow::Result<()> {
    // Keep this fail-fast and deterministic. Wall-clock soak, hardware frame/RSS measurements,
    // trusted signing, and real external-account smoke are explicit release gates outside CI.
    check_boundary()?;
    check_deps()?;
    check_package_gate_source()?;
    run_process("sh", &["-n", "scripts/package-macos.sh"])?;
    run_process("sh", &["-n", "scripts/verify-macos-package.sh"])?;
    run_cargo(&["fmt", "--all", "--", "--check"])?;
    run_cargo(&["check", "--workspace", "--all-targets", "--locked"])?;
    run_cargo(&[
        "clippy",
        "--workspace",
        "--all-targets",
        "--locked",
        "--",
        "-D",
        "warnings",
    ])?;
    smoke_db_migrations()?;
    security_scan()?;
    perf_smoke()?;
    od01_failure_matrix()?;
    i18n_check()?;
    run_cargo(&[
        "test",
        "--workspace",
        "--no-fail-fast",
        "--locked",
        "--",
        "--test-threads=1",
    ])?;
    println!(
        "bg01-deterministic-gate OK — structural, security, failure, performance smoke, and workspace regressions"
    );
    Ok(())
}

fn check_package_gate_source() -> anyhow::Result<()> {
    let root = workspace_root()?;
    let package = std::fs::read_to_string(root.join("scripts/package-macos.sh"))
        .context("package-macos.sh read failed")?;
    let verifier = std::fs::read_to_string(root.join("scripts/verify-macos-package.sh"))
        .context("verify-macos-package.sh read failed")?;
    let workflow = std::fs::read_to_string(root.join("docs/build/bg01-deterministic-workflow.yml"))
        .context("BG01 workflow template read failed")?;
    for required in [
        "REQUIRE_TRUSTED=${DEPPY_REQUIRE_TRUSTED_SIGNING:-1}",
        "ALLOW_UNTRUSTED=${DEPPY_ALLOW_UNTRUSTED_SIGNING:-0}",
        "cargo build --release -p deppy-sijo -p mcp-proxy",
        "Developer ID Application:",
        "codesign --force --options runtime --timestamp",
        "DEPPY_NOTARY_KEYCHAIN_PROFILE",
        "DEPPY_NOTARY_KEY_ID",
        "xcrun notarytool submit",
        "--wait --timeout",
        "NOTARY_STATUS",
        "Accepted",
        "xcrun stapler staple",
        "ditto -c -k --sequesterRsrc --keepParent",
        "verify-macos-package.sh",
    ] {
        anyhow::ensure!(
            package.contains(required),
            "macOS package gate missing required step: {required}"
        );
    }
    for required in [
        "REQUIRE_TRUSTED=${DEPPY_REQUIRE_TRUSTED_SIGNING:-1}",
        "ALLOW_UNTRUSTED=${DEPPY_ALLOW_UNTRUSTED_SIGNING:-0}",
        "codesign --verify --deep --strict",
        "xcrun stapler validate",
        "spctl --assess --type execute",
        "source=Notarized Developer ID",
        "lipo -archs",
        "CFBundleIdentifier",
        "TeamIdentifier",
        "Signature=adhoc",
        "anchor apple generic",
        "certificate leaf[field.1.2.840.113635.100.6.1.13] exists",
        "flags=.*runtime",
        "Timestamp=",
        "verify_trusted_code \"$candidate\" \"$team_id\"",
        "verify_trusted_code \"$candidate/Contents/MacOS/$BIN_NAME\" \"$team_id\"",
        "verify_trusted_code \"$candidate/Contents/MacOS/$PROXY_NAME\" \"$team_id\"",
        "shasum -a 256",
    ] {
        anyhow::ensure!(
            verifier.contains(required),
            "macOS package verifier missing required check: {required}"
        );
    }
    let first_archive = package
        .find("ditto -c -k --sequesterRsrc --keepParent")
        .context("macOS package gate missing upload archive")?;
    let submit = package
        .find("xcrun notarytool submit")
        .context("macOS package gate missing notarization submit")?;
    let staple = package
        .find("xcrun stapler staple")
        .context("macOS package gate missing ticket staple")?;
    let final_archive = package
        .rfind("ditto -c -k --sequesterRsrc --keepParent")
        .context("macOS package gate missing final archive")?;
    let verify = package
        .rfind("verify-macos-package.sh")
        .context("macOS package gate missing final verification")?;
    anyhow::ensure!(
        first_archive < submit
            && submit < staple
            && staple < final_archive
            && final_archive < verify,
        "macOS package must archive, submit, staple, rebuild the archive, then verify"
    );
    anyhow::ensure!(
        workflow.contains("actions/checkout@11bd71901bbe5b1630ceea73d27597364c9af683"),
        "BG01 workflow template must pin checkout to the reviewed commit"
    );
    anyhow::ensure!(
        !workflow.contains("uses: actions/checkout@v")
            && !workflow.contains("uses: Swatinem/rust-cache@"),
        "BG01 workflow template contains mutable action code"
    );
    Ok(())
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
        point: "StatusFeedLazyConstruction",
        package: "deppy-sijo",
        exact_test: "status_feed::tests::construction은_thread_network_timer_repaint를_시작하지_않는다",
    },
    FailureMatrixCase {
        point: "StatusFeedIdleReap",
        package: "deppy-sijo",
        exact_test: "status_feed::tests::active_intent만_lazy_start하고_hidden_idle_ttl뒤_join_reap한다",
    },
    FailureMatrixCase {
        point: "SettingsAuxLazyConstruction",
        package: "deppy-sijo",
        exact_test: "app::tests::env_aux_workers는_constructor에서_thread나_io를_시작하지_않는다",
    },
    FailureMatrixCase {
        point: "SettingsAuxRenderBoundary",
        package: "deppy-sijo",
        exact_test: "app::tests::app_ui는_aux_worker를_직접_admit하지_않는다",
    },
    FailureMatrixCase {
        point: "WorkspaceShutdownBound",
        package: "deppy-sijo",
        exact_test: "app::tests::workspace_shutdown_registry는_two_slot을넘지않고_join한다",
    },
    FailureMatrixCase {
        point: "WorkspaceIdleOneShot",
        package: "deppy-sijo",
        exact_test: "app::tests::warm_idle_deadline은_변경될때만_one_shot_repaint를_예약한다",
    },
    FailureMatrixCase {
        point: "SanitizedPanicHook",
        package: "deppy-sijo",
        exact_test: "production_panic_hook_drops_payload_before_diagnostics",
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
    // `cargo test -p i18n`의 `required_locales_have_complete_keys`는 로케일 "간" 키
    // 짝맞춤만 본다 — 5개 로케일 모두에 똑같이 없는 키(예: 코드가 쓰는데 어느 로케일
    // 파일에도 안 채워진 키)는 "일치"라서 통과해 버린다. 그 결과 화면에 키 문자열이
    // 그대로 노출되는 사고가 났다(runtime.spawn_failed.invalid_command 등). 아래 검사가
    // "코드가 실제로 참조하는 키가 로케일에 있는지"를 직접 대조해 그 구멍을 메운다.
    check_i18n_key_coverage()?;
    println!("i18n-check OK");
    Ok(())
}

/// `crates/` 전역에서 `catalog.t("key", ...)` / `MessagePayload::new("key")`로 참조하는
/// **리터럴** 키가 5개 로케일(`crates/i18n/locales/*`) 전부에 실제로 있는지 대조한다.
///
/// 정적으로 못 잡는 범위(`extract_i18n_key_calls` 문서 참고): `format!()`로 조립한 키,
/// `xxx_key()` 헬퍼가 반환하는 키, match 팔에서 고른 상수 키. 그런 호출은 "dynamic"으로
/// 개수만 센다 — 못 잡는다는 사실을 숨기지 않는다. 실측치와 상세는
/// `docs/superpowers/specs/2026-08-19-i18n-key-guard.md` 참고.
///
/// 테스트 코드의 가짜 키(예: 워크스페이스 spawn 재현용 `"shell.failed"`, `format!("agent.failed.{idx}")`)가
/// 오탐을 만들지 않도록, `check_leaf_semantic_boundary`와 같은 규칙으로 최상위 아이템의
/// `#[cfg(test)]`를 그대로 제외한다.
fn check_i18n_key_coverage() -> anyhow::Result<()> {
    let root = workspace_root()?;
    let locales = locale_key_sets(&root)?;
    anyhow::ensure!(
        !locales.is_empty(),
        "crates/i18n/locales 아래에 로케일이 없습니다"
    );

    let mut violations = Vec::new();
    let mut literal_total = 0usize;
    let mut dynamic_total = 0usize;

    for path in rust_files_under(&root.join("crates"))? {
        let rel = rel_path(&root, &path)?;
        // `crates/*/tests/` 아래는 통합 테스트 바이너리다 — 관례상 `#[cfg(test)]` 없이
        // 맨 `#[test]`를 쓰므로 `top_level_item_is_test_only` 제외에 걸리지 않는다.
        // 지금은 i18n 키를 쓰는 파일이 없지만, 가짜 키를 쓰는 통합 테스트가 하나라도
        // 생기면 그 즉시 오탐으로 게이트가 무너진다(2026-08-19 코드 리뷰).
        if rel.contains("/tests/") {
            continue;
        }
        let source = std::fs::read_to_string(&path).with_context(|| format!("{rel} 읽기 실패"))?;
        let (literal, dynamic) =
            check_i18n_key_coverage_source(&rel, &source, &locales, &mut violations)?;
        literal_total += literal;
        dynamic_total += dynamic;
    }

    if violations.is_empty() {
        println!(
            "i18n key coverage OK — 리터럴 키 호출 {literal_total}건을 로케일 {}개와 대조; \
             정적으로 못 잡는 동적 키 호출 {dynamic_total}건(docs/superpowers/specs/2026-08-19-i18n-key-guard.md 참고)",
            locales.len()
        );
        Ok(())
    } else {
        violations.sort();
        violations.dedup();
        for violation in &violations {
            eprintln!("VIOLATION: {violation}");
        }
        bail!("i18n key coverage 실패: {}건", violations.len());
    }
}

/// 소스 하나의 production 영역(`#[cfg(test)]` 최상위 아이템 제외)에서 i18n 키 호출을
/// 추출해 로케일과 대조하고, 못 찾은 (파일, 키) 조합을 `violations`에 남긴다.
/// 반환값은 (리터럴 키 호출 수, 정적으로 못 잡은 동적 키 호출 수).
fn check_i18n_key_coverage_source(
    rel: &str,
    source: &str,
    locales: &[(String, std::collections::BTreeSet<String>)],
    violations: &mut Vec<String>,
) -> anyhow::Result<(usize, usize)> {
    let syntax = syn::parse_file(source).with_context(|| format!("{rel} Rust syntax 파싱 실패"))?;
    let mut literal_count = 0usize;
    let mut dynamic_count = 0usize;
    for item in syntax.items {
        let compact = item
            .into_token_stream()
            .to_string()
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>();
        if top_level_item_is_test_only(&compact) {
            continue;
        }
        let extracted = extract_i18n_key_calls(&compact);
        dynamic_count += extracted.dynamic;
        for key in extracted.literal {
            literal_count += 1;
            let missing_locales: Vec<&str> = locales
                .iter()
                .filter(|(_, keys)| !keys.contains(&key))
                .map(|(locale, _)| locale.as_str())
                .collect();
            if !missing_locales.is_empty() {
                violations.push(format!(
                    "{rel}: key '{key}' missing from locale(s): {}",
                    missing_locales.join(", ")
                ));
            }
        }
    }
    Ok((literal_count, dynamic_count))
}

struct ExtractedI18nKeys {
    literal: Vec<String>,
    dynamic: usize,
}

/// 공백을 다 지운 토큰 문자열에서 `catalog.t("key"` / `MessagePayload::new("key"` 바로
/// 뒤의 **문자열 리터럴**만 키로 추출한다. 인자가 문자열 리터럴이 아니면(변수, `format!()`,
/// 헬퍼 함수 호출 등) 컴파일타임에 값을 알 수 없어 정적으로 못 잡는다 — dynamic 카운트로만
/// 집계하고 넘어간다. 이 정직한 한계는 의도된 것이다(거짓 안심을 주는 게이트가 없는
/// 게이트보다 나쁘다).
fn extract_i18n_key_calls(compact: &str) -> ExtractedI18nKeys {
    let mut literal = Vec::new();
    let mut dynamic = 0usize;
    // 키를 **인자로 받는** 헬퍼도 여기 등록해야 한다. 등록하지 않으면 그 키는 리터럴로도
    // 동적으로도 세지 않아 **가드에서 통째로 보이지 않는다** — "OK"라고 말하면서 놓친다
    // (2026-08-19 코드 리뷰: `sanitized_spawn_failure`가 정확히 그랬다). 새 헬퍼를 만들면
    // 여기 추가하거나, 키를 `MessagePayload::new(` 옆에 그대로 두어라.
    for prefix in [".t(", "MessagePayload::new(", "sanitized_spawn_failure("] {
        let mut rest = compact;
        while let Some(idx) = rest.find(prefix) {
            let after = &rest[idx + prefix.len()..];
            match after.strip_prefix('"') {
                Some(stripped) => {
                    if let Some(end) = stripped.find('"') {
                        literal.push(stripped[..end].to_owned());
                    }
                }
                None if !after.is_empty() => dynamic += 1,
                None => {}
            }
            rest = after;
        }
    }
    ExtractedI18nKeys { literal, dynamic }
}

/// `crates/i18n/locales/*` 아래 로케일 디렉터리마다 `messages.txt`를 파싱해 키 집합을
/// 만든다. `crates/i18n::parse_locale_file`은 private이라 재사용할 수 없어 같은 포맷
/// (`key = value`, `#` 주석/빈 줄 무시)을 최소 형태로 다시 파싱한다 — 포맷이 바뀌면
/// `cargo test -p i18n`이 먼저 깨지므로 drift는 그쪽에서 드러난다.
fn locale_key_sets(
    root: &Path,
) -> anyhow::Result<Vec<(String, std::collections::BTreeSet<String>)>> {
    let locales_dir = root.join("crates/i18n/locales");
    let mut names = Vec::new();
    for entry in std::fs::read_dir(&locales_dir)
        .with_context(|| format!("{} 읽기 실패", locales_dir.display()))?
    {
        let entry = entry?;
        if entry.path().is_dir() {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    names.sort();
    names
        .into_iter()
        .map(|locale| {
            let keys = locale_key_set(root, &locale)?;
            Ok((locale, keys))
        })
        .collect()
}

fn locale_key_set(root: &Path, locale: &str) -> anyhow::Result<std::collections::BTreeSet<String>> {
    let path = root.join(format!("crates/i18n/locales/{locale}/messages.txt"));
    let content =
        std::fs::read_to_string(&path).with_context(|| format!("{} 읽기 실패", path.display()))?;
    let mut keys = std::collections::BTreeSet::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, _)) = line.split_once('=') {
            keys.insert(key.trim().to_owned());
        }
    }
    Ok(keys)
}

fn run_cargo(args: &[&str]) -> anyhow::Result<()> {
    run_process("cargo", args)
}

fn run_process(program: &str, args: &[&str]) -> anyhow::Result<()> {
    let root = workspace_root()?;
    let status = std::process::Command::new(program)
        .args(args)
        .current_dir(root)
        .status()
        .with_context(|| format!("{program} {} 실행 실패", args.join(" ")))?;
    if status.success() {
        Ok(())
    } else {
        bail!("{program} {} 실패: {status}", args.join(" "));
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
        pattern: "RuntimeClient",
        label: "leaf UI must return protocol intents instead of owning a runtime client",
    },
    BoundaryRule {
        pattern: "send_command(",
        label: "leaf UI must not execute runtime protocol commands directly",
    },
];

const LEAF_SEMANTIC_RULES: &[BoundaryRule] = &[
    BoundaryRule {
        pattern: "storage::",
        label: "leaf UI must not access the storage crate",
    },
    BoundaryRule {
        pattern: "mcp::",
        label: "leaf UI must not access the MCP transport crate",
    },
    BoundaryRule {
        pattern: "audit::",
        label: "leaf UI must not access the audit crate",
    },
    BoundaryRule {
        pattern: "secret::",
        label: "leaf UI must not access the secret crate",
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
        check_leaf_semantic_boundary(&rel, &content, &mut violations)?;
    }

    check_session_secret_boundary(&root, &mut violations)?;
    check_authorization_capability_boundary(&root, &mut violations)?;
    check_app_render_source_boundary(&root, &mut violations)?;
    check_app_composition_root_boundary(&root, &mut violations)?;

    if violations.is_empty() {
        println!(
            "check-boundary OK — UI leaves have zero native/runtime capability; bounded composition-root terminal tail passed"
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

fn check_leaf_semantic_boundary(
    rel: &str,
    source: &str,
    violations: &mut Vec<String>,
) -> anyhow::Result<()> {
    let syntax = syn::parse_file(source).with_context(|| format!("{rel} Rust syntax 파싱 실패"))?;
    for item in syntax.items {
        let compact = item
            .into_token_stream()
            .to_string()
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>();
        if top_level_item_is_test_only(&compact) {
            continue;
        }
        for rule in LEAF_SEMANTIC_RULES {
            if compact.contains(rule.pattern) {
                violations.push(format!(
                    "{rel}: production syntax boundary violation: '{}' ({})",
                    rule.pattern, rule.label
                ));
            }
        }
    }
    Ok(())
}

fn top_level_item_is_test_only(compact_tokens: &str) -> bool {
    let mut remaining = compact_tokens;
    while let Some(attributes) = remaining.strip_prefix("#[") {
        let mut depth = 1usize;
        let mut end = None;
        for (index, character) in attributes.char_indices() {
            match character {
                '[' => depth += 1,
                ']' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(index);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(end) = end else {
            return false;
        };
        let attribute = &attributes[..end];
        if attribute == "cfg(test)" {
            return true;
        }
        remaining = &attributes[end + 1..];
    }
    false
}

/// 구체 저장소를 소유해도 되는 파일 목록. **정확히 이 둘뿐이다.**
///
/// - `app.rs`: loopback/Tailscale 경로의 합성 루트.
/// - `relay_repository.rs`: Relay 영속 어댑터. 계획
///   `docs/superpowers/plans/2026-08-28-production-relay.md` Task 2가 요구하는 분리다 —
///   `web-remote`가 SQLite를 열지 않게 하려면 앱이 소유해야 하고, 동시에 Relay 의존성을
///   `app.rs`에 밀어 넣지 않아야 두 전송 경로가 독립적으로 유지된다. 이 모듈은 앱의 다른
///   모듈을 하나도 import하지 않는다(해당 크레이트의 소스 법칙 테스트가 고정한다).
const APP_COMPOSITION_ROOTS: &[&str] = &["app.rs", "relay_repository.rs"];

fn check_app_composition_root_boundary(
    root: &Path,
    violations: &mut Vec<String>,
) -> anyhow::Result<()> {
    for path in rust_files_under(&root.join("crates/app/src"))? {
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                path.parent() == Some(root.join("crates/app/src").as_path())
                    && APP_COMPOSITION_ROOTS.contains(&name)
            })
        {
            continue;
        }
        let rel = rel_path(root, &path)?;
        let source = std::fs::read_to_string(&path).with_context(|| format!("{rel} 읽기 실패"))?;
        check_app_composition_source(&rel, &source, violations)?;
    }
    if root.join("crates/app/src/storage.rs").exists() {
        violations.push(
            "crates/app/src/storage.rs: app-local concrete storage re-export shim is forbidden"
                .to_owned(),
        );
    }
    Ok(())
}

fn check_app_composition_source(
    rel: &str,
    source: &str,
    violations: &mut Vec<String>,
) -> anyhow::Result<()> {
    const FORBIDDEN: &[(&str, &str)] = &[
        (
            "Db::open(",
            "production concrete database construction belongs in app.rs",
        ),
        (
            "use storage::Db",
            "production concrete database ownership belongs in app.rs",
        ),
        (
            "storage::Db",
            "production concrete database ownership belongs in app.rs",
        ),
        (
            "crate::storage",
            "app-local concrete storage re-export shims are forbidden",
        ),
        (
            "KeyringSecretStore",
            "production concrete keyring ownership belongs in app.rs",
        ),
    ];
    let syntax = syn::parse_file(source)
        .with_context(|| format!("{rel} composition-root syntax parsing failed"))?;
    for item in syntax.items {
        let compact = item
            .into_token_stream()
            .to_string()
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>();
        if top_level_item_is_test_only(&compact) {
            continue;
        }
        for (pattern, reason) in FORBIDDEN {
            let compact_pattern = pattern
                .chars()
                .filter(|character| !character.is_whitespace())
                .collect::<String>();
            if compact.contains(&compact_pattern) {
                violations.push(format!(
                    "{rel}: production syntax composition-root violation: '{pattern}' ({reason})"
                ));
            }
        }
    }
    Ok(())
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
            "self.send_composer_prompt(",
            "render must stage terminal composer input for logic",
        ),
        (
            "self.connector_coordinator.dispatch(",
            "render must stage Connector worker dispatch for logic",
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
    check_terminal_tail_host_source(&source, violations)?;
    let call = "self.flush_workspace_terminal_protocol_tail(ui.ctx());";
    if ui_body.matches(call).count() != 1
        || ui_body
            .find(call)
            .zip(ui_body.rfind("flush_render_side_effects(ui.ctx());"))
            .is_none_or(|(tail, flush)| tail <= flush)
        || ui_body
            .find(call)
            .zip(ui_body.find("self.frame_stats.end();"))
            .is_none_or(|(tail, end)| tail >= end)
    {
        violations.push(format!(
            "{rel}: terminal host tail must run once after final render flush"
        ));
    }
    Ok(())
}

// App's sole render-time runtime capability is a bounded, nonblocking terminal FIFO prefix.
// Inspect the actual production adapter bodies, including their calls, so an extra broad helper
// cannot hide storage/lifecycle/guarded delivery behind the permitted App::ui call.
fn check_terminal_tail_host_source(
    source: &str,
    violations: &mut Vec<String>,
) -> anyhow::Result<()> {
    const ADAPTERS: &[(&str, &[&str])] = &[
        (
            "dispatch_terminal_protocol_tail",
            &[
                "will_discard",
                "take_terminal_protocol_intent",
                "operation",
                "generation",
                "focus_pane",
                "cloned",
                "send",
                "into_command",
                "finish_owned_workspace_protocol_delivery",
                "is_some",
                "cancel_terminal_focus",
                "protocol_retry_delay",
                "has_queued_protocol_intents",
                "request_repaint",
                "request_workspace_protocol_retry",
                "Some",
            ],
        ),
        (
            "flush_workspace_terminal_protocol_tail",
            &[
                "dispatch_terminal_protocol_tail",
                "send_command_owned",
                "cancel_terminal_focus_intents",
                "arm_terminal_focus",
                "values_mut",
                "Some",
            ],
        ),
        (
            "finish_owned_workspace_protocol_delivery",
            &[
                "Ok",
                "Err",
                "Some",
                "downcast_ref",
                "terminal_protocol_command",
                "return_unsent_terminal_protocol",
                "classify_workspace_protocol_delivery",
                "is_ok",
                "complete_protocol",
            ],
        ),
        (
            "request_workspace_protocol_retry",
            &[
                "input",
                "try_from_secs_f32",
                "unwrap_or_default",
                "request_repaint_after",
                "saturating_add",
            ],
        ),
    ];
    let syntax = syn::parse_file(source).context("terminal host source parsing failed")?;
    let mut bodies = std::collections::HashMap::new();
    for item in syntax.items {
        match item {
            syn::Item::Fn(function) => {
                bodies.insert(
                    function.sig.ident.to_string(),
                    function.block.to_token_stream().to_string(),
                );
            }
            syn::Item::Impl(implementation) => {
                for item in implementation.items {
                    if let syn::ImplItem::Fn(function) = item {
                        bodies.insert(
                            function.sig.ident.to_string(),
                            function.block.to_token_stream().to_string(),
                        );
                    }
                }
            }
            _ => {}
        }
    }
    for (name, allowed) in ADAPTERS {
        let Some(body) = bodies.get(*name) else {
            violations.push(format!(
                "app.rs: missing bounded terminal host adapter {name}"
            ));
            continue;
        };
        let tokens: Vec<_> = body.split_whitespace().collect();
        for (index, token) in tokens.iter().enumerate().skip(1) {
            if !token.starts_with('(') {
                continue;
            }
            let mut callee = index - 1;
            // Include macro and generic calls, rather than allowing a broad helper to evade
            // the effect allowlist simply by changing invocation syntax.
            if tokens[callee] == "!" && callee > 0 {
                callee -= 1;
            }
            if tokens[callee] == ">" {
                let mut depth = 1;
                while callee > 0 && depth > 0 {
                    callee -= 1;
                    match tokens[callee] {
                        ">" => depth += 1,
                        "<" => depth -= 1,
                        _ => {}
                    }
                }
                if depth == 0 && callee >= 2 && tokens[callee - 1] == "::" {
                    callee -= 2;
                }
            }
            let method = tokens[callee];
            if method
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
                && !allowed.contains(&method)
            {
                violations.push(format!(
                    "app.rs: terminal adapter {name} calls unapproved effect {method}"
                ));
            }
        }
        for pattern in [
            "std :: fs",
            "std :: process",
            "send_guarded",
            "shutdown",
            "join",
            "sleep",
            "recv",
            "dotenv",
            "self . db",
        ] {
            if body.contains(pattern) {
                violations.push(format!(
                    "app.rs: terminal adapter {name} contains blocking or broad effect {pattern}"
                ));
            }
        }
        if *name == "dispatch_terminal_protocol_tail"
            && (!body.contains("for _ in 0 .. 8 {") || !body.contains("ctx . will_discard ()"))
        {
            violations
                .push("app.rs: terminal host adapter lost final-pass/eight-command bound".into());
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

    let mut operation_issuer_count = 0usize;
    let mut preflight_finisher_count = 0usize;
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
        let issuers = identifier_occurrences(&content, "prepare_owned_authorization_operation");
        let finishers = identifier_occurrences(&content, "finish_owned_authorization_preflight");
        if rel == STORAGE_ISSUER {
            operation_issuer_count += issuers;
            preflight_finisher_count += finishers;
        } else if issuers != 0 || finishers != 0 {
            violations.push(format!(
                "{rel}: authorization operation/grant issuer는 {STORAGE_ISSUER} transaction wrapper만 호출할 수 있습니다"
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

    if operation_issuer_count != 1 {
        violations.push(format!(
            "{STORAGE_ISSUER}: owner-scoped authorization operation issuer expected 1 seen {operation_issuer_count}"
        ));
    }
    if preflight_finisher_count != 2 {
        violations.push(format!(
            "{STORAGE_ISSUER}: post-commit authorization preflight finisher expected 2 seen {preflight_finisher_count}"
        ));
    }
    let storage_source = std::fs::read_to_string(root.join(STORAGE_ISSUER))
        .context("storage authorization boundary source read failed")?;
    for method in [
        "commit_authorization_preflight",
        "commit_authorization_preflight_revision_cas",
    ] {
        let Some((_, tail)) = storage_source.split_once(&format!("pub fn {method}(")) else {
            violations.push(format!("{STORAGE_ISSUER}: {method} is missing"));
            continue;
        };
        let body = tail.split("\n    pub fn ").next().unwrap_or(tail);
        let Some(finish_at) = body.find("audit::finish_owned_authorization_preflight(") else {
            violations.push(format!(
                "{STORAGE_ISSUER}: {method} does not finish the exact committed operation"
            ));
            continue;
        };
        let transaction_at = body.find("with_audit_retention_normalization_retry");
        let commit_at = body[..finish_at].rfind("tx.commit()");
        if transaction_at.is_none_or(|offset| offset >= finish_at) || commit_at.is_none() {
            violations.push(format!(
                "{STORAGE_ISSUER}: {method} must create the grant only after transaction commit"
            ));
        }
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

/// Cargo가 실제로 해석한 workspace metadata에서 로컬 의존을 추출한다. 직접 `path`와
/// `{ workspace = true }`, dev/build/target dependency를 모두 포함하므로 manifest 표기법으로
/// 금지 edge나 순환 검사를 우회할 수 없다. 반환: 디렉터리명 → 의존 디렉터리명 목록.
fn local_dep_graph() -> anyhow::Result<BTreeMap<String, Vec<String>>> {
    let root = workspace_root()?;
    let output = std::process::Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1", "--locked"])
        .current_dir(&root)
        .output()
        .context("cargo metadata 실행 실패")?;
    anyhow::ensure!(
        output.status.success(),
        "cargo metadata 실패: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("cargo metadata JSON 파싱 실패")?;
    let workspace_members = metadata["workspace_members"]
        .as_array()
        .context("cargo metadata workspace_members 없음")?
        .iter()
        .map(|member| {
            member
                .as_str()
                .context("cargo metadata workspace member가 문자열이 아님")
        })
        .collect::<anyhow::Result<std::collections::BTreeSet<_>>>()?;
    let packages = metadata["packages"]
        .as_array()
        .context("cargo metadata packages 없음")?;
    let mut workspace_paths = BTreeMap::<PathBuf, String>::new();
    for package in packages {
        let id = package["id"]
            .as_str()
            .context("cargo metadata package id 없음")?;
        if !workspace_members.contains(id) {
            continue;
        }
        let manifest = PathBuf::from(
            package["manifest_path"]
                .as_str()
                .context("cargo metadata manifest_path 없음")?,
        );
        let dir = manifest
            .parent()
            .context("workspace manifest parent 없음")?;
        let name = dir
            .file_name()
            .and_then(|value| value.to_str())
            .context("workspace crate 디렉터리명 없음")?
            .to_owned();
        workspace_paths.insert(dir.to_path_buf(), name);
    }

    let mut graph = workspace_paths
        .values()
        .cloned()
        .map(|name| (name, Vec::new()))
        .collect::<BTreeMap<_, _>>();
    for package in packages {
        let id = package["id"]
            .as_str()
            .context("cargo metadata package id 없음")?;
        if !workspace_members.contains(id) {
            continue;
        }
        let manifest = PathBuf::from(
            package["manifest_path"]
                .as_str()
                .context("cargo metadata manifest_path 없음")?,
        );
        let package_dir = manifest
            .parent()
            .context("workspace manifest parent 없음")?;
        let package_name = workspace_paths
            .get(package_dir)
            .context("workspace package directory mapping 없음")?;
        let dependencies = package["dependencies"]
            .as_array()
            .context("cargo metadata dependencies 없음")?;
        let edges = graph
            .get_mut(package_name)
            .context("workspace graph package 없음")?;
        for dependency in dependencies {
            let Some(path) = dependency["path"].as_str().map(PathBuf::from) else {
                continue;
            };
            if let Some(target) = workspace_paths.get(&path) {
                edges.push(target.clone());
            }
        }
        edges.sort();
        edges.dedup();
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

    /// 실제 저장소가 이 gate를 통과하는지 고정한다 — 코드가 참조하는 리터럴 i18n 키가
    /// 5개 로케일 전부에 있는지 재발 방지로 남긴다(runtime.spawn_failed.invalid_command
    /// 사고의 회귀 테스트).
    #[test]
    fn 현재_i18n_key_coverage는_5개_로케일에_모두_있다() {
        check_i18n_key_coverage().unwrap();
    }

    #[test]
    fn key_coverage는_리터럴_키_누락은_잡고_동적_키와_test_모듈은_건너뛴다() {
        let locales = vec![
            (
                "en-US".to_owned(),
                std::collections::BTreeSet::from(["real.key".to_owned()]),
            ),
            (
                "ko-KR".to_owned(),
                std::collections::BTreeSet::from(["real.key".to_owned()]),
            ),
        ];
        let source = r#"
fn render(catalog: &Catalog, dynamic_key: &str) -> String {
    let _ = catalog.t("real.key", &[]);
    let _ = catalog.t("missing.key", &[]);
    let _ = catalog.t(dynamic_key, &[]);
    MessagePayload::new("real.key");
    catalog.t("real.key", &[])
}

#[cfg(test)]
mod tests {
    fn fixture(catalog: &Catalog) -> String {
        catalog.t("test.only.fake.key", &[])
    }
}
"#;
        let mut violations = Vec::new();
        let (literal, dynamic) =
            check_i18n_key_coverage_source("fixture.rs", source, &locales, &mut violations)
                .unwrap();
        assert_eq!(
            literal, 4,
            "real.key 3번 + MessagePayload::new(real.key) 1번 = 4건 (test 모듈의 가짜 키는 제외)"
        );
        assert_eq!(dynamic, 1, "dynamic_key 변수 호출 1건만 dynamic으로 집계");
        assert_eq!(violations.len(), 1, "missing.key 하나만 위반이어야 한다");
        assert!(violations[0].contains("fixture.rs"));
        assert!(violations[0].contains("missing.key"));
        assert!(violations[0].contains("en-US"));
        assert!(violations[0].contains("ko-KR"));
        assert!(
            !violations[0].contains("real.key"),
            "실제로 있는 키는 위반 목록에 없어야 한다"
        );
        assert!(
            !violations[0].contains("test.only.fake.key"),
            "test 모듈의 가짜 키는 애초에 추출되지 않아야 한다"
        );
    }

    #[test]
    fn pr10_terminal_host_exception_rejects_broad_hidden_calls() {
        let source = include_str!("../../crates/app/src/app.rs");
        let mut violations = Vec::new();
        check_terminal_tail_host_source(source, &mut violations).unwrap();
        assert!(violations.is_empty(), "{violations:?}");
        for (old, new) in [
            (
                "self.active.runtime.send_command_owned(command)",
                "self.active.runtime.send_guarded_input(command)",
            ),
            (
                "self.cancel_terminal_focus_intents();\n            self.active.workspace_ui.arm_terminal_focus(pane);",
                "self.refresh_workspaces();\n            self.active.workspace_ui.arm_terminal_focus(pane);",
            ),
            ("for _ in 0..8 {", "for _ in 0..8000 {"),
            (
                "self.active.runtime.send_command_owned(command)",
                "self.active.runtime.broad_helper::<()>(command)",
            ),
        ] {
            assert!(source.contains(old));
            let changed = source.replace(old, new);
            let mut violations = Vec::new();
            check_terminal_tail_host_source(&changed, &mut violations).unwrap();
            assert!(
                !violations.is_empty(),
                "hidden broad adapter call/bound escaped: {new}"
            );
        }
    }

    #[test]
    fn leaf_semantic_boundary는_test_item을제외하고뒤production도검사한다() {
        let source = r#"
#[cfg(test)]
mod tests {
    fn fixture() { let _ = storage::Db::open("test"); }
}

fn production_after_tests() {
    let _ = storage::Db::open("production");
}
"#;
        let mut violations = Vec::new();
        check_leaf_semantic_boundary("fixture.rs", source, &mut violations).unwrap();
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("storage::"));

        let tests_only = r#"
/// Test module documentation.
#[cfg(test)]
mod tests {
    fn fixture() { let _ = secret::KeyringSecretStore; }
}
"#;
        violations.clear();
        check_leaf_semantic_boundary("fixture.rs", tests_only, &mut violations).unwrap();
        assert!(violations.is_empty());
    }

    /// 합성 루트는 정확히 둘이다. 세 번째가 조용히 늘어나면 이 테스트가 먼저 깨진다.
    #[test]
    fn 합성_루트는_app_rs와_relay_repository_둘뿐이다() {
        assert_eq!(APP_COMPOSITION_ROOTS, &["app.rs", "relay_repository.rs"]);
        let root = workspace_root().unwrap();
        for name in APP_COMPOSITION_ROOTS {
            assert!(
                root.join("crates/app/src").join(name).exists(),
                "{name} 이(가) 없는데 예외로 남아 있다"
            );
        }
    }

    #[test]
    fn production_app_concrete_stores는_app_rs에만_존재한다() {
        let root = workspace_root().unwrap();
        let mut violations = Vec::new();
        check_app_composition_root_boundary(&root, &mut violations).unwrap();
        assert!(violations.is_empty(), "{violations:#?}");
    }

    #[test]
    fn composition_root_scan은_test_item을제외하고뒤production도검사한다() {
        let source = r#"
#[cfg(test)]
mod tests {
    fn fixture() { let _ = storage::Db::open("test"); }
}

fn production_after_tests() {
    let _ = storage::Db::open("production");
}
"#;
        let mut violations = Vec::new();
        check_app_composition_source("fixture.rs", source, &mut violations).unwrap();
        assert_eq!(violations.len(), 2);
        assert!(
            violations
                .iter()
                .all(|violation| violation.contains("production"))
        );
    }

    #[test]
    fn 로컬_의존_그래프가_기대_edge를_담는다() {
        let graph = local_dep_graph().unwrap();
        // 실재하는 대표 edge 몇 개로 파서가 동작함을 고정
        assert!(graph["storage"].contains(&"mcp".to_owned()) || !graph["storage"].is_empty());
        assert!(graph["runtime"].contains(&"mux".to_owned()));
        assert!(
            graph["app"].contains(&"i18n".to_owned()),
            "workspace-inherited local dependency must be present"
        );
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
    fn macos_package_gate는_공증_gatekeeper와_archive를검증한다() {
        check_package_gate_source().unwrap();
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

use audit::finish_owned_authorization_preflight as finish;
use audit::prepare_owned_authorization_operation as mint;
use audit::evaluate_authorization as compatibility_evaluator;

fn production_item() {
    mint();
    finish();
    compatibility_evaluator();
    audit::evaluate_authorization_with_fingerprint();
}
"#;
        assert_eq!(
            identifier_occurrences(source, "prepare_owned_authorization_operation"),
            1
        );
        assert_eq!(
            identifier_occurrences(source, "finish_owned_authorization_preflight"),
            1
        );
        assert_eq!(identifier_occurrences(source, "evaluate_authorization"), 1);
        assert_eq!(
            identifier_occurrences(source, "evaluate_authorization_with_fingerprint"),
            1
        );
    }
}
