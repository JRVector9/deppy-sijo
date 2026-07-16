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
    /// (바인딩, 에이전트 owner pid, 결정적 여부). 휴리스틱 바인딩은 매 tick lsof로
    /// 업그레이드를 시도한다 — codex가 작업 중 rollout을 열면 정확한 것으로 교체.
    entries: HashMap<SessionId, (AgentBinding, u32, bool)>,
}

/// 캐시를 활용한 detect. `cache`는 호출측(워커 스레드)이 소유·유지한다.
pub fn detect_cached(
    sessions: &[(SessionId, u32)],
    overrides: &HashMap<SessionId, AgentBinding>,
    cache: &mut BindingCache,
) -> HashMap<SessionId, AgentBinding> {
    let rows = process_rows();
    let live_pids: std::collections::HashSet<u32> = rows.iter().map(|r| r.pid).collect();
    let mut out = HashMap::new();
    for (sid, shell_pid) in sessions {
        // hook(SessionStart 등)이 보고한 바인딩이 있으면 그것이 결정적 — 프로세스 생존만
        // 확인하고 탐색 전체를 스킵한다(cmux식 이벤트 바인딩, 2026-07-07).
        if let Some(b) = overrides.get(sid) {
            if let Some(owner) = find_agent_pid(*shell_pid, &rows) {
                cache.entries.insert(*sid, (b.clone(), owner, true));
                out.insert(*sid, b.clone());
            } else {
                cache.entries.remove(sid);
            }
            continue;
        }
        // 캐시 히트 + owner 프로세스 생존 → 재발견 스킵. 단 휴리스틱 바인딩은 lsof로
        // 결정적 업그레이드를 시도한다(작업 중 rollout이 열리면 정확한 파일로 교체).
        if let Some((binding, owner_pid, det)) = cache.entries.get(sid)
            && live_pids.contains(owner_pid)
        {
            let (owner_pid, det) = (*owner_pid, *det);
            {
                if !det
                    && binding.kind == AgentKind::Codex
                    && let Some(t) = codex_open_rollout(owner_pid)
                    && let Some(id) = t
                        .file_name()
                        .and_then(|n| n.to_str())
                        .and_then(agent_transcript::codex_session_id)
                {
                    let upgraded = AgentBinding {
                        kind: AgentKind::Codex,
                        session_id: id,
                        transcript: t,
                    };
                    cache
                        .entries
                        .insert(*sid, (upgraded.clone(), owner_pid, true));
                    out.insert(*sid, upgraded);
                } else {
                    out.insert(*sid, cache.entries[sid].0.clone());
                }
            }
            continue;
        }
        // 미스/종료 → 전체 탐색 후 캐시 갱신.
        if let Some((binding, owner_pid, det)) = find_agent(*shell_pid, &rows) {
            cache
                .entries
                .insert(*sid, (binding.clone(), owner_pid, det));
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

/// 바인딩된 transcript를 파싱해 전체 상태(활동 + 표시 정보)를 읽는다. 파싱은 한 번만.
pub fn agent_state(binding: &AgentBinding) -> Option<agent_transcript::TranscriptState> {
    match binding.kind {
        AgentKind::Claude => agent_transcript::parse_claude(&binding.transcript),
        AgentKind::Codex => agent_transcript::parse_codex(&binding.transcript),
    }
}

/// 바인딩된 transcript를 파싱해 현재 활동(working/idle)을 읽는다.
pub fn activity(binding: &AgentBinding) -> Option<agent_transcript::AgentActivity> {
    agent_state(binding).map(|s| s.activity)
}

/// 3줄 세션 행 2행 표시 정보 (2026-07-08). kind는 바인딩에서, model/effort/context는
/// transcript(codex 전부 / claude는 model만 — effort/context는 statusLine→DB)에서.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDisplay {
    pub kind: AgentKind,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub context_pct: Option<u8>,
}

/// 셸 pid의 자손 중 claude/codex를 찾아 transcript까지 바인딩한다. 캐시 생존 확인용으로
/// 그 에이전트 프로세스 pid도 함께 돌려준다.
/// 셸 자손 중 에이전트 프로세스가 있으면 그 pid (생존 확인용 — 바인딩은 hook이 제공).
fn find_agent_pid(shell_pid: u32, rows: &[ProcRow]) -> Option<u32> {
    let descendants = descendant_pids(shell_pid, rows);
    rows.iter()
        .filter(|r| descendants.contains(&r.pid))
        .find(|r| classify(&r.command).is_some())
        .map(|r| r.pid)
}

fn find_agent(shell_pid: u32, rows: &[ProcRow]) -> Option<(AgentBinding, u32, bool)> {
    let descendants = descendant_pids(shell_pid, rows);
    // 한 셸에 에이전트가 여럿일 수 있다(^Z 중단 후 재실행 등, 2026-07-07 실증: codex 2개).
    // 선택 기준: ①결정적(argv/lsof) 바인딩이 휴리스틱(cwd)보다 우선 — 휴리스틱 mtime이
    // 더 최신이어도 오바인딩일 수 있다(실증: fresh codex가 resume 세션으로 오바인딩).
    // ②같은 등급 안에선 transcript mtime 최신(활성 대화가 append 중인 쪽).
    let mut best: Option<(AgentBinding, u32, bool, Option<std::time::SystemTime>)> = None;
    for row in rows.iter().filter(|r| descendants.contains(&r.pid)) {
        if let Some((kind, sid_hint)) = classify(&row.command)
            && let Some((b, det)) = bind_transcript(kind, sid_hint, row.pid)
        {
            let mtime = std::fs::metadata(&b.transcript)
                .and_then(|m| m.modified())
                .ok();
            let better = best
                .as_ref()
                .is_none_or(|(_, _, bdet, bmt)| (det, mtime) > (*bdet, *bmt));
            if better {
                best = Some((b, row.pid, det, mtime));
            }
        }
    }
    best.map(|(b, pid, det, _)| (b, pid, det))
}

/// command가 claude/codex인지 판별하고, claude면 argv에서 세션ID를 추출한다.
fn classify(command: &str) -> Option<(AgentKind, Option<String>)> {
    // 처음 두 토큰(프로그램, 또는 인터프리터+스크립트)의 파일명이 claude/codex인지.
    // 이전의 `contains("/codex ")`는 인자 없는 `node /opt/.../codex`(뒤공백 없음)를
    // 놓쳤다(2026-07-07 실증 — fresh codex wrapper 미분류).
    let is = |name: &str| {
        command.split_whitespace().take(2).any(|tok| {
            Path::new(tok)
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n == name)
        })
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

/// 감지된 에이전트를 실제 transcript 파일로 확정한다. 두 번째 반환값 = 결정적 여부:
/// argv 세션ID(claude)·lsof 열린 파일(codex)은 결정적, cwd 매칭은 휴리스틱(오바인딩 가능 —
/// 2026-07-07 실증: 프로세스 cwd(/Users)와 rollout 기록 cwd(arteawiki)가 다르거나, 같은
/// cwd 다중 세션이 최신 쪽으로 모임). find_agent가 결정적 후보를 우선한다.
fn bind_transcript(
    kind: AgentKind,
    sid_hint: Option<String>,
    pid: u32,
) -> Option<(AgentBinding, bool)> {
    match kind {
        AgentKind::Claude => {
            // 1순위: argv --session-id (결정적 — cmux/자동화 실행 케이스).
            if let Some(sid) = sid_hint {
                let transcript = find_claude_transcript(&sid)?;
                return Some((
                    AgentBinding {
                        kind,
                        session_id: sid,
                        transcript,
                    },
                    true,
                ));
            }
            // fallback: 손타이핑 `claude`(argv에 세션ID 없음 — 2026-07-07 실증)는 cwd의
            // 프로젝트 디렉터리(~/.claude/projects/<escaped-cwd>/)에서 최신 mtime transcript.
            // 이게 없으면 상태가 느린 화면 regex로만 잡혀 딜레이가 났다. 같은 cwd에 claude
            // 2개면 최신 대화 쪽으로 모일 수 있는 한계는 codex cwd fallback과 동일.
            let cwd = process_cwd(pid)?;
            let (session_id, transcript) = find_claude_transcript_by_cwd(&cwd)?;
            Some((
                AgentBinding {
                    kind,
                    session_id,
                    transcript,
                },
                false,
            ))
        }
        AgentKind::Codex => {
            // 1순위: 프로세스가 append 중인 rollout을 lsof로 직접 획득 — 결정적(스캔·상한·
            // cwd 매칭 불필요, resume된 옛 파일·같은 cwd 다중 세션도 정확). codex는 rollout을
            // write 모드로 열어둔다(2026-07-07 실증: 07/03 파일을 resume 중인 프로세스에서 확인).
            if let Some(transcript) = codex_open_rollout(pid)
                && let Some(session_id) = transcript
                    .file_name()
                    .and_then(|n| n.to_str())
                    .and_then(agent_transcript::codex_session_id)
            {
                return Some((
                    AgentBinding {
                        kind,
                        session_id,
                        transcript,
                    },
                    true,
                ));
            }
            // fallback: cwd 매칭 스캔 — 휴리스틱. codex는 rollout을 항상 열어두지 않아
            // (실증: idle fresh 세션은 닫혀 있음) lsof가 자주 miss라 필요하다.
            let cwd = process_cwd(pid)?;
            let (session_id, transcript) = find_codex_transcript(&cwd)?;
            Some((
                AgentBinding {
                    kind,
                    session_id,
                    transcript,
                },
                false,
            ))
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

/// transcript(jsonl) 앞부분에서 세션의 원래 cwd를 읽는다 — 복원 resume 시 그 폴더로
/// `cd` 하기 위함(2026-07-08: resume은 이어졌는데 셸 폴더가 workspace 루트라 실제 작업
/// 폴더와 달랐다). codex rollout은 1행 session_meta payload.cwd, claude는 초반 행들에
/// "cwd" 필드. 파싱 실패 줄은 건너뛴다.
pub fn transcript_cwd(path: &std::path::Path) -> Option<String> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    for line in reader.lines().take(50).map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if let Some(cwd) = v
            .get("cwd")
            .and_then(|x| x.as_str())
            .or_else(|| v.pointer("/payload/cwd").and_then(|x| x.as_str()))
        {
            return Some(cwd.to_owned());
        }
    }
    None
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
    let root = crate::paths::home_dir()?.join(".codex/sessions");
    let mut files = Vec::new();
    collect_jsonl(&root, &mut files);
    files.into_iter().find(|p| {
        p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.contains(session_id))
    })
}

/// claude 프로젝트 디렉터리 이름 — cwd의 비영숫자를 전부 '-'로 치환한다
/// (실증: `/Users/jr/Desktop/Projects/deppy/.claude/...` → `-Users-jr-Desktop-Projects-deppy--claude-...`).
fn claude_project_dir_escape(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// cwd 기준으로 claude transcript를 찾는다 — 그 cwd의 프로젝트 디렉터리에서 최신 mtime
/// jsonl(활성 대화가 append 중인 것). 파일명(stem) = 세션ID.
fn find_claude_transcript_by_cwd(cwd: &str) -> Option<(String, PathBuf)> {
    let dir = crate::paths::home_dir()?
        .join(".claude/projects")
        .join(claude_project_dir_escape(cwd));
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "jsonl")
            && let Ok(meta) = e.metadata()
            && let Ok(mtime) = meta.modified()
            && best.as_ref().is_none_or(|(bm, _)| mtime > *bm)
        {
            best = Some((mtime, p));
        }
    }
    let (_, p) = best?;
    let sid = p.file_stem()?.to_str()?.to_owned();
    Some((sid, p))
}

/// 세션ID로 claude transcript를 찾는다 (`~/.claude/projects/*/<sid>.jsonl`).
fn find_claude_transcript(session_id: &str) -> Option<PathBuf> {
    let projects = crate::paths::home_dir()?.join(".claude/projects");
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
    let root = crate::paths::home_dir()?.join(".codex/sessions");
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

/// codex 프로세스가 열어둔 rollout(.jsonl) 파일 — `lsof -p <pid>`의 열린 파일 중
/// `~/.codex/sessions/**.jsonl`. 있으면 그게 곧 이 프로세스의 transcript(결정적).
#[cfg(unix)]
fn codex_open_rollout(pid: u32) -> Option<PathBuf> {
    let out = std::process::Command::new("lsof")
        .args(["-a", "-p", &pid.to_string(), "-Fn"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.strip_prefix('n'))
        .find(|p| p.contains("/.codex/sessions/") && p.ends_with(".jsonl"))
        .map(PathBuf::from)
}

#[cfg(not(unix))]
fn codex_open_rollout(_pid: u32) -> Option<PathBuf> {
    None
}

/// 여러 셸 pid의 현재 작업 디렉터리를 **한 번의 lsof**로 얻는다(세션 행 폴더명 +
/// 워크스페이스 이름 추적, 2026-07-08). off-thread 호출. `-p pid1,pid2,...`는 lsof가
/// 지원하는 다중 pid 형식이라 세션 수만큼 프로세스를 띄우지 않는다.
#[cfg(unix)]
pub(crate) fn session_cwds(pids: &[u32]) -> HashMap<u32, String> {
    let mut out = HashMap::new();
    if pids.is_empty() {
        return out;
    }
    let list = pids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let output = std::process::Command::new("lsof")
        .args(["-a", "-d", "cwd", "-p", &list, "-Fpn"])
        .output();
    let Ok(output) = output else { return out };
    // -Fpn: 'p<pid>' 라인 뒤에 그 프로세스의 'n<경로>' 라인이 온다.
    let mut cur: Option<u32> = None;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some(pid) = line.strip_prefix('p') {
            cur = pid.parse().ok();
        } else if let Some(path) = line.strip_prefix('n')
            && let Some(pid) = cur
        {
            out.insert(pid, path.to_owned());
        }
    }
    out
}

#[cfg(not(unix))]
pub(crate) fn session_cwds(_pids: &[u32]) -> HashMap<u32, String> {
    HashMap::new()
}

/// cwd → 표시명 (2026-07-13: 설정에서 스타일 선택).
/// - [`SessionNameStyle::Folder`](기본): **현재(마지막) 폴더명** — Crawler/printbakery면 "printbakery".
/// - [`SessionNameStyle::Repo`]: git 저장소 루트명(.git 상향 탐색, 최대 40단계) —
///   저장소 하위 어디서든 "Crawler". 저장소 밖이면 현재 폴더명 폴백.
///   홈 디렉터리 자체는 두 스타일 모두 "~".
pub(crate) fn project_display_name(
    cwd: &str,
    style: crate::config::SessionNameStyle,
) -> Option<String> {
    let path = std::path::Path::new(cwd);
    if !path.is_absolute() {
        return None;
    }
    // 홈 루트는 특별 취급 — 폴더명("jr" 등) 대신 "~".
    if crate::paths::home_dir().is_some_and(|home| path == home) {
        return Some("~".to_owned());
    }
    if style == crate::config::SessionNameStyle::Repo {
        // .git을 위로 탐색(최대 40단계 — 극단 경로 방어) → 저장소 루트명.
        let mut cur = Some(path);
        let mut steps = 0;
        while let Some(dir) = cur {
            if steps >= 40 {
                break;
            }
            if dir.join(".git").exists() {
                return dir.file_name().map(|n| n.to_string_lossy().into_owned());
            }
            cur = dir.parent();
            steps += 1;
        }
    }
    // Folder 스타일 또는 저장소 밖 — 현재 폴더명.
    path.file_name().map(|n| n.to_string_lossy().into_owned())
}

/// 프로세스의 cwd — platform::process_cwd(lsof) 공용 구현을 쓴다.
fn process_cwd(pid: u32) -> Option<String> {
    Some(platform::process_cwd(pid)?.to_string_lossy().into_owned())
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
    fn project_display_name은_스타일에_따라_폴더명_또는_레포명() {
        use crate::config::SessionNameStyle as S;
        let base = std::env::temp_dir().join(format!("deppy-proj-{}", std::process::id()));
        let repo = base.join("Crawler");
        let sub = repo.join("printbakery");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let sub_s = sub.to_str().unwrap();
        // 기본(Folder): 저장소 하위 깊은 폴더도 **현재 폴더명**
        assert_eq!(
            project_display_name(sub_s, S::Folder),
            Some("printbakery".to_owned())
        );
        // Repo: 저장소 하위 어디서든 **레포 루트명**
        assert_eq!(
            project_display_name(sub_s, S::Repo),
            Some("Crawler".to_owned())
        );
        // 레포 루트 자체는 두 스타일 모두 레포명
        let repo_s = repo.to_str().unwrap();
        assert_eq!(
            project_display_name(repo_s, S::Folder),
            Some("Crawler".to_owned())
        );
        assert_eq!(
            project_display_name(repo_s, S::Repo),
            Some("Crawler".to_owned())
        );
        // 저장소 밖 폴더는 두 스타일 모두 폴더명 (Repo는 폴백)
        let plain = base.join("plainfolder");
        std::fs::create_dir_all(&plain).unwrap();
        for style in [S::Folder, S::Repo] {
            assert_eq!(
                project_display_name(plain.to_str().unwrap(), style),
                Some("plainfolder".to_owned())
            );
        }
        // 상대경로는 None
        assert_eq!(project_display_name("relative/path", S::Folder), None);
        let _ = std::fs::remove_dir_all(&base);
    }

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

    #[test]
    fn claude_project_dir_escape_비영숫자를_하이픈으로() {
        assert_eq!(
            claude_project_dir_escape("/Users/jr/Desktop/Projects/deppy-sijo"),
            "-Users-jr-Desktop-Projects-deppy-sijo"
        );
        // dot도 '-' (실증: deppy/.claude → deppy--claude)
        assert_eq!(claude_project_dir_escape("/a/b.c/d_e"), "-a-b-c-d-e");
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
                && let Some((b, det)) = bind_transcript(kind, sid, row.pid)
            {
                println!(
                    "  {:?} pid={} det={det} sid={}… → {}",
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
