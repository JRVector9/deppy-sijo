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

fn main() -> anyhow::Result<()> {
    let command = std::env::args().nth(1).unwrap_or_default();
    match command.as_str() {
        "check-boundary" => check_boundary(),
        "check-deps" => check_deps(),
        "smoke-db-migrations" => smoke_db_migrations(),
        "security-scan" => security_scan(),
        "perf-smoke" => perf_smoke(),
        "i18n-check" => i18n_check(),
        other => bail!(
            "알 수 없는 명령 '{other}' — 사용법: cargo run -p xtask -- check-deps|check-boundary|smoke-db-migrations|security-scan|perf-smoke|i18n-check"
        ),
    }
}

fn smoke_db_migrations() -> anyhow::Result<()> {
    run_cargo(&["test", "-p", "storage", "마이그레이션"])?;
    run_cargo(&["test", "-p", "storage", "v8에서_v9"])?;
    run_cargo(&["test", "-p", "storage", "v9에서_v10"])?;
    run_cargo(&["test", "-p", "storage", "v10에서_v11"])?;
    run_cargo(&["test", "-p", "storage", "v11에서_v12"])?;
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
        snippet: "use mcp::{LocalMcpManager, McpServerConfig, McpTool};",
        count: 1,
        reason: "PR-B00 deferred connector MCP runtime boundary",
    },
    BoundaryAllow {
        path: "crates/app/src/ui/connectors.rs",
        snippet: "let manager = LocalMcpManager::new(self.redaction.clone());",
        count: 3,
        reason: "PR-B00 deferred connector MCP discover/prepare/call execution",
    },
];

const RECORD_TOOL_AUDIT_ALLOW: &[BoundaryAllow] = &[BoundaryAllow {
    path: "crates/app/src/ui/connectors.rs",
    snippet: "if let Err(e) = db.record_tool_audit(&record, &self.redaction, None) {",
    count: 1,
    reason: "PR-B00 deferred connector audit storage boundary",
}];

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
        snippet: "if let Err(e) = db.insert_credential(&meta) {",
        count: 1,
        reason: "PR-B00 deferred connector OAuth metadata storage boundary",
    },
    BoundaryAllow {
        path: "crates/app/src/ui/connectors.rs",
        snippet: "if let Ok(tools) = db.list_mcp_tools(&server.id) {",
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
    BoundaryAllow {
        path: "crates/app/src/ui/connectors.rs",
        snippet: "if let Err(e) = db.record_tool_audit(&record, &self.redaction, None) {",
        count: 1,
        reason: "PR-B00 deferred connector audit storage boundary",
    },
    BoundaryAllow {
        path: "crates/app/src/ui/connectors.rs",
        snippet: "match db.replace_mcp_tools(&server_id, &rows) {",
        count: 1,
        reason: "PR-B00 deferred connector MCP tools storage boundary",
    },
    BoundaryAllow {
        path: "crates/app/src/ui/connectors.rs",
        snippet: "match db.insert_mcp_server(&row) {",
        count: 1,
        reason: "PR-B00 deferred connector MCP server storage boundary",
    },
    BoundaryAllow {
        path: "crates/app/src/ui/env_profiles.rs",
        snippet: "let p = db.list_env_profiles(workspace_id)?;",
        count: 1,
        reason: "existing env profile storage UI exception",
    },
    BoundaryAllow {
        path: "crates/app/src/ui/env_profiles.rs",
        snippet: "db.delete_env_profile(&id)?;",
        count: 1,
        reason: "existing env profile storage UI exception",
    },
    BoundaryAllow {
        path: "crates/app/src/ui/env_profiles.rs",
        snippet: "db.insert_env_profile(workspace_id, self.new_name.trim(), self.new_kind)?;",
        count: 1,
        reason: "existing env profile storage UI exception",
    },
    BoundaryAllow {
        path: "crates/app/src/ui/env_profiles.rs",
        snippet: "let credentials = db.list_credentials()?;",
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
    BoundaryAllow {
        path: "crates/app/src/ui/env_profiles.rs",
        snippet: "db.upsert_env_var(&profile_id, self.var_key.trim(), &value)?;",
        count: 1,
        reason: "existing env profile storage UI exception",
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
        allowed: NO_ALLOW,
    },
    BoundaryRule {
        pattern: "set_secret(",
        label: "leaf UI must not write secrets directly",
        allowed: NO_ALLOW,
    },
    BoundaryRule {
        pattern: "get_secret(",
        label: "leaf UI must not read secrets directly",
        allowed: NO_ALLOW,
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
        for (line_idx, line) in content.lines().enumerate() {
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

    if violations.is_empty() {
        let explicit_exceptions: usize = DB_CALL_ALLOW
            .iter()
            .chain(LOCAL_MCP_MANAGER_ALLOW)
            .chain(RECORD_TOOL_AUDIT_ALLOW)
            .map(|allow| allow.count)
            .sum();
        println!(
            "check-boundary OK — UI leaf boundary guard passed; {} explicit UI DB/MCP/audit exceptions remain",
            explicit_exceptions
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
}
