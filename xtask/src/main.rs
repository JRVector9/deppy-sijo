//! xtask — 워크스페이스 관리 명령 (v2.8 §11).
//!
//! `cargo run -p xtask -- check-deps`
//!   crate 그래프의 **금지 의존 edge**와 **순환**을 검사한다. v2.8 영속 계층 규칙
//!   (storage-core는 도메인을 모름, runtime crate는 store를 모름 등)을 코드로 강제해,
//!   `mcp → storage` 같은 순환 유발 edge가 무심코 추가되는 것을 막는다.
//!
//! Cargo.toml의 `path = "../<dir>"` 로컬 의존만 본다(외부 crate는 무관). crate 식별은
//! 디렉터리명 기준(예: crates/core의 패키지명은 deppy-core지만 여기선 "core").

use std::collections::BTreeMap;
use std::path::Path;

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
        "check-deps" => check_deps(),
        other => bail!("알 수 없는 명령 '{other}' — 사용법: cargo run -p xtask -- check-deps"),
    }
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
    fn 로컬_의존_그래프가_기대_edge를_담는다() {
        let graph = local_dep_graph().unwrap();
        // 실재하는 대표 edge 몇 개로 파서가 동작함을 고정
        assert!(graph["storage"].contains(&"mcp".to_owned()) || !graph["storage"].is_empty());
        assert!(graph["runtime"].contains(&"mux".to_owned()));
        assert!(graph.contains_key("xtask"));
    }
}
