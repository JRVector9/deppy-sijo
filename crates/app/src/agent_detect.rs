//! 세션(셸)의 프로세스 트리에서 실행 중인 claude/codex를 감지하고 transcript로 바인딩한다
//! (옵션2 Phase 2). 셸 pid → 자손 프로세스 → 에이전트 식별:
//!
//! - **claude**: 프로세스 argv에 `--session-id <uuid>`가 있어 **결정적**으로 세션ID를 얻는다.
//!   → `~/.claude/projects/*/<session-id>.jsonl` transcript로 직결(같은 cwd 다중 실행도 안 겹침).
//! - **codex**: argv에 세션ID가 없어 프로세스 cwd(lsof)로 rollout(session_meta.cwd)을 매칭한다.
//!
//! ps 트리 순회는 resource_monitor와 같은 `ps -axo` 방식을 따른다(재사용 패턴).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use runtime::SessionId;

use crate::agent_transcript;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentKind {
    Claude,
    Codex,
}

/// 세션에 바인딩된 에이전트 — transcript 경로까지 확정된 상태.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentBinding {
    pub kind: AgentKind,
    pub session_id: String,
    pub transcript: PathBuf,
}

struct ProcRow {
    pid: u32,
    ppid: Option<u32>,
    command: String,
}

/// 이미 확정된 세션→바인딩을 캐시해 재발견(lsof/codex 재귀 스캔)을 스킵한다(codex #3).
/// 각 세션(셸 pid)에서 실행 중인 에이전트를 감지해 transcript로 바인딩한다 — ps를 한 번
/// 에이전트 프로세스(owner_pid)가 여전히 ps 결과에 살아있으면 캐시를 재사용하고, 사라졌으면
/// 다시 탐색한다. 정상 케이스(에이전트 생존)에서 세션당 O(1) pid 확인만 남는다.
#[derive(Default)]
pub struct BindingCache {
    entries: HashMap<SessionId, (AgentBinding, u32)>, // (바인딩, 에이전트 owner pid)
}

/// 캐시를 활용한 detect. `cache`는 호출측(워커 스레드)이 소유·유지한다.
pub fn detect_cached(
    sessions: &[(SessionId, u32)],
    cache: &mut BindingCache,
) -> HashMap<SessionId, AgentBinding> {
    let rows = process_rows();
    let live_pids: std::collections::HashSet<u32> = rows.iter().map(|r| r.pid).collect();
    let mut out = HashMap::new();
    for (sid, shell_pid) in sessions {
        // 캐시 히트 + owner 프로세스 생존 → 재발견 스킵.
        if let Some((binding, owner_pid)) = cache.entries.get(sid)
            && live_pids.contains(owner_pid)
        {
            out.insert(*sid, binding.clone());
            continue;
        }
        // 미스/종료 → 전체 탐색 후 캐시 갱신.
        if let Some((binding, owner_pid)) = find_agent(*shell_pid, &rows) {
            cache.entries.insert(*sid, (binding.clone(), owner_pid));
            out.insert(*sid, binding);
        } else {
            cache.entries.remove(sid);
        }
    }
    // 더는 존재하지 않는 세션의 캐시 항목 정리(누수 방지).
    let alive: std::collections::HashSet<SessionId> = sessions.iter().map(|(s, _)| *s).collect();
    cache.entries.retain(|sid, _| alive.contains(sid));
    out
}

/// 바인딩된 transcript를 파싱해 현재 활동(working/idle)을 읽는다.
pub fn activity(binding: &AgentBinding) -> Option<agent_transcript::AgentActivity> {
    let state = match binding.kind {
        AgentKind::Claude => agent_transcript::parse_claude(&binding.transcript),
        AgentKind::Codex => agent_transcript::parse_codex(&binding.transcript),
    }?;
    Some(state.activity)
}

/// 셸 pid의 자손 중 claude/codex를 찾아 transcript까지 바인딩한다. 캐시 생존 확인용으로
/// 그 에이전트 프로세스 pid도 함께 돌려준다.
fn find_agent(shell_pid: u32, rows: &[ProcRow]) -> Option<(AgentBinding, u32)> {
    let descendants = descendant_pids(shell_pid, rows);
    // 가장 안쪽(최근 spawn) 우선 — 트리 순서상 뒤에 오는 pid가 대체로 최신.
    for row in rows.iter().filter(|r| descendants.contains(&r.pid)) {
        if let Some((kind, sid_hint)) = classify(&row.command)
            && let Some(b) = bind_transcript(kind, sid_hint, row.pid)
        {
            return Some((b, row.pid));
        }
    }
    None
}

/// command가 claude/codex인지 판별하고, claude면 argv에서 세션ID를 추출한다.
fn classify(command: &str) -> Option<(AgentKind, Option<String>)> {
    // 바이너리 경로가 claude/codex로 끝나는 토큰이 있는지 (부분일치 오탐 회피).
    let is = |name: &str| {
        command.split_whitespace().next().is_some_and(|prog| {
            Path::new(prog)
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n == name)
        }) || command.contains(&format!("/{name} ")) // node wrapper: "node .../codex ..."
            || command.contains(&format!("/{name}\t"))
    };
    if is("claude") {
        return Some((AgentKind::Claude, claude_session_id(command)));
    }
    if is("codex") {
        return Some((AgentKind::Codex, None));
    }
    None
}

/// claude argv의 `--session-id <uuid>` 추출.
fn claude_session_id(command: &str) -> Option<String> {
    let mut it = command.split_whitespace();
    while let Some(tok) = it.next() {
        if tok == "--session-id" {
            return it.next().map(str::to_owned);
        }
        if let Some(v) = tok.strip_prefix("--session-id=") {
            return Some(v.to_owned());
        }
    }
    None
}

/// 감지된 에이전트를 실제 transcript 파일로 확정한다.
fn bind_transcript(kind: AgentKind, sid_hint: Option<String>, pid: u32) -> Option<AgentBinding> {
    match kind {
        AgentKind::Claude => {
            let sid = sid_hint?; // argv에서 세션ID를 못 얻으면 바인딩 안 함(모호 회피)
            let transcript = find_claude_transcript(&sid)?;
            Some(AgentBinding {
                kind,
                session_id: sid,
                transcript,
            })
        }
        AgentKind::Codex => {
            let cwd = process_cwd(pid)?;
            let (session_id, transcript) = find_codex_transcript(&cwd)?;
            Some(AgentBinding {
                kind,
                session_id,
                transcript,
            })
        }
    }
}

/// 저장된 (kind, session_id)로 transcript 파일을 찾는다 — 복원 resume 전에 대상이 아직
/// 존재하는지 확인해, 이미 지워진 세션에 `--resume`을 던지지 않게 한다.
pub fn find_transcript(kind: AgentKind, session_id: &str) -> Option<PathBuf> {
    match kind {
        AgentKind::Claude => find_claude_transcript(session_id),
        AgentKind::Codex => find_codex_transcript_by_id(session_id),
    }
}

/// "claude"/"codex" 문자열 → AgentKind.
pub fn kind_from_str(s: &str) -> Option<AgentKind> {
    match s {
        "claude" => Some(AgentKind::Claude),
        "codex" => Some(AgentKind::Codex),
        _ => None,
    }
}

/// codex rollout을 session_id(UUID)가 파일명에 든 것으로 찾는다.
fn find_codex_transcript_by_id(session_id: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let root = Path::new(&home).join(".codex/sessions");
    let mut files = Vec::new();
    collect_jsonl(&root, &mut files);
    files.into_iter().find(|p| {
        p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.contains(session_id))
    })
}

/// 세션ID로 claude transcript를 찾는다 (`~/.claude/projects/*/<sid>.jsonl`).
fn find_claude_transcript(session_id: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let projects = Path::new(&home).join(".claude/projects");
    for proj in std::fs::read_dir(projects).ok()?.flatten() {
        let candidate = proj.path().join(format!("{session_id}.jsonl"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// cwd로 codex rollout을 찾는다 — session_meta.cwd가 일치하는 것 중 가장 최근.
fn find_codex_transcript(cwd: &str) -> Option<(String, PathBuf)> {
    let home = std::env::var_os("HOME")?;
    let root = Path::new(&home).join(".codex/sessions");
    let mut files = Vec::new();
    collect_jsonl(&root, &mut files);
    files.sort_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
    for p in files.iter().rev().take(64) {
        if let Some(state) = agent_transcript::parse_codex(p)
            && state.cwd.as_deref() == Some(cwd)
        {
            return Some((state.session_id, p.clone()));
        }
    }
    None
}

fn collect_jsonl(dir: &Path, out: &mut Vec<PathBuf>) {
    collect_jsonl_bounded(dir, out, 0);
}

/// 재귀 스캔에 상한을 둔다(codex 리뷰): ①심링크 미추적(루프·외부 거대 디렉터리 방지),
/// ②depth 상한, ③파일 수 상한. resume 존재확인이 UI 스레드에서도 부르므로 무한 재귀/대량
/// 스캔으로 인한 freeze를 막는다.
///
/// **역순(최신 먼저) 순회**: ~/.codex/sessions는 YYYY/MM/DD 구조 + rollout-<타임스탬프> 파일명
/// 이라 이름 역순 = 시간 역순. 상한에 걸리면 '오래된 쪽'이 잘려야 한다 — 정순 순회는 rollout이
/// 상한(4096)을 넘는 순간 최신 세션이 누락돼 codex 감지가 죽었다(2026-07-07 실증: 7,447개).
fn collect_jsonl_bounded(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    const MAX_DEPTH: usize = 8;
    const MAX_FILES: usize = 4096;
    if depth > MAX_DEPTH || out.len() >= MAX_FILES {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    // 이름 역순 정렬 — read_dir 순서는 비보장이라 명시 정렬해야 "최신 먼저"가 성립한다.
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| std::cmp::Reverse(e.file_name()));
    for e in entries {
        if out.len() >= MAX_FILES {
            break;
        }
        // file_type()는 심링크를 따라가지 않는다(Path::is_dir과 달리) — 심링크는 스킵.
        let Ok(ft) = e.file_type() else {
            continue;
        };
        if ft.is_symlink() {
            continue;
        }
        let p = e.path();
        if ft.is_dir() {
            collect_jsonl_bounded(&p, out, depth + 1);
        } else if p.extension().is_some_and(|x| x == "jsonl") {
            out.push(p);
        }
    }
}

/// 프로세스의 cwd (macOS/Unix: `lsof -p <pid> -d cwd`).
#[cfg(unix)]
fn process_cwd(pid: u32) -> Option<String> {
    let out = std::process::Command::new("lsof")
        .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
        .output()
        .ok()?;
    // -Fn: 'n' 접두 라인이 경로. 여러 줄 중 마지막 n 라인.
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.strip_prefix('n'))
        .next_back()
        .map(str::to_owned)
}

#[cfg(not(unix))]
fn process_cwd(_pid: u32) -> Option<String> {
    None
}

/// 셸 pid의 모든 자손 pid 집합 (resource_monitor와 동일한 BFS).
fn descendant_pids(root_pid: u32, rows: &[ProcRow]) -> std::collections::HashSet<u32> {
    let mut wanted = std::collections::HashSet::from([root_pid]);
    let mut changed = true;
    while changed {
        changed = false;
        for row in rows {
            if row.ppid.is_some_and(|ppid| wanted.contains(&ppid)) && wanted.insert(row.pid) {
                changed = true;
            }
        }
    }
    wanted
}

#[cfg(unix)]
fn process_rows() -> Vec<ProcRow> {
    let output = std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,command="])
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(parse_ps_line)
        .collect()
}

#[cfg(not(unix))]
fn process_rows() -> Vec<ProcRow> {
    Vec::new()
}

/// `ps -axo pid=,ppid=,command=` 한 줄: "pid ppid command with spaces".
/// pid/ppid는 우측정렬이라 공백이 여러 칸일 수 있어 whitespace run으로 자른다.
fn parse_ps_line(line: &str) -> Option<ProcRow> {
    let line = line.trim_start();
    let (pid_str, rest) = line.split_once(char::is_whitespace)?;
    let (ppid_str, rest) = rest.trim_start().split_once(char::is_whitespace)?;
    let pid = pid_str.parse().ok()?;
    let ppid = ppid_str.parse().ok();
    let command = rest.trim_start().to_owned();
    Some(ProcRow { pid, ppid, command })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ps_line_splits_command_with_spaces() {
        let r =
            parse_ps_line("  7788  6572 /Users/jr/.local/bin/claude --session-id abc-123").unwrap();
        assert_eq!(r.pid, 7788);
        assert_eq!(r.ppid, Some(6572));
        assert_eq!(
            r.command,
            "/Users/jr/.local/bin/claude --session-id abc-123"
        );
    }

    #[test]
    fn classify_claude_extracts_session_id() {
        let (kind, sid) =
            classify("/Users/jr/.local/bin/claude --session-id e741d734-2e58 --foo").unwrap();
        assert_eq!(kind, AgentKind::Claude);
        assert_eq!(sid.as_deref(), Some("e741d734-2e58"));
    }

    #[test]
    fn classify_codex_node_wrapper() {
        let (kind, sid) = classify("node /opt/homebrew/bin/codex --enable hooks").unwrap();
        assert_eq!(kind, AgentKind::Codex);
        assert_eq!(sid, None);
    }

    #[test]
    fn classify_ignores_unrelated() {
        assert!(classify("/bin/zsh -l").is_none());
        assert!(classify("vim claude_notes.md").is_none()); // 인자 언급은 오탐 안 함
    }

    #[test]
    fn descendants_follow_ppid_chain() {
        let rows = vec![
            ProcRow {
                pid: 100,
                ppid: Some(1),
                command: "zsh".into(),
            },
            ProcRow {
                pid: 200,
                ppid: Some(100),
                command: "claude".into(),
            },
            ProcRow {
                pid: 300,
                ppid: Some(200),
                command: "child".into(),
            },
            ProcRow {
                pid: 999,
                ppid: Some(1),
                command: "other".into(),
            },
        ];
        let d = descendant_pids(100, &rows);
        assert!(d.contains(&200) && d.contains(&300));
        assert!(!d.contains(&999));
    }

    /// 실제 머신의 claude/codex 프로세스를 감지해 transcript 바인딩까지 되는지 smoke-test.
    /// 실행: `cargo test -p deppy-sijo smoke_detect -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn smoke_detect_real() {
        let rows = process_rows();
        println!("ps rows: {}", rows.len());
        let mut found = 0;
        for row in &rows {
            if let Some((kind, sid)) = classify(&row.command)
                && let Some(b) = bind_transcript(kind, sid, row.pid)
            {
                println!(
                    "  {:?} pid={} sid={}… → {}",
                    b.kind,
                    row.pid,
                    &b.session_id[..b.session_id.len().min(16)],
                    b.transcript.display()
                );
                found += 1;
            }
        }
        println!("바인딩 성공: {found}");
    }
}
