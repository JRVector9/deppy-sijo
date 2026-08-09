//! 세션(셸)의 프로세스 트리에서 실행 중인 claude/codex를 감지하고 transcript로 바인딩한다
//! (옵션2 Phase 2). 셸 pid → 자손 프로세스 → 에이전트 식별:
//!
//! - **claude**: 프로세스 argv에 `--session-id <uuid>`가 있어 **결정적**으로 세션ID를 얻는다.
//!   → `~/.claude/projects/*/<session-id>.jsonl` transcript로 직결(같은 cwd 다중 실행도 안 겹침).
//! - **codex**: argv에 세션ID가 없어 프로세스 cwd(lsof)로 rollout(session_meta.cwd)을 매칭한다.
//!
//! ps 트리 순회는 resource_monitor와 같은 `ps -axo` 방식을 따른다(재사용 패턴).

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use runtime::SessionId;

use crate::agent_transcript;

const MAX_SESSIONS: usize = 256;
const MAX_PROCESS_ROWS: usize = 16_384;
const MAX_DESCENDANTS_PER_SESSION: usize = 1_024;
const MAX_AGENT_CANDIDATES_PER_SESSION: usize = 8;
const MAX_LSOF_CALLS_PER_DETECTION: usize = 64;
const MAX_PROCESS_COMMAND_BYTES: usize = 16 * 1024;
const MAX_SESSION_ID_BYTES: usize = 256;
const MAX_PATH_BYTES: usize = 4 * 1024;
const MAX_PS_STDOUT_BYTES: usize = 8 * 1024 * 1024;
const MAX_LSOF_STDOUT_BYTES: usize = 2 * 1024 * 1024;
const MAX_CLI_STDERR_BYTES: usize = 64 * 1024;
const MAX_TRANSCRIPT_HEAD_BYTES: usize = 512 * 1024;
const MAX_TRANSCRIPT_LINE_BYTES: usize = 64 * 1024;
const MAX_TRANSCRIPT_HEAD_LINES: usize = 50;
const MAX_SCAN_FILES: usize = 4_096;
const MAX_SCAN_ENTRIES: usize = 16_384;
const MAX_DIRECTORY_ENTRIES: usize = 4_096;
const MAX_SCAN_DEPTH: usize = 8;
pub(crate) const RESUME_PROBE_ITEMS_MAX: usize = MAX_SESSIONS;
pub(crate) const RESUME_PROBE_REQUEST_BYTES_MAX: usize =
    RESUME_PROBE_ITEMS_MAX * (std::mem::size_of::<ResumeTranscriptProbe>() + MAX_SESSION_ID_BYTES);
pub(crate) const RESUME_PROBE_RESULT_BYTES_MAX: usize =
    RESUME_PROBE_ITEMS_MAX * (std::mem::size_of::<ResumeTranscriptProbeResult>() + MAX_PATH_BYTES);
const PROCESS_TIMEOUT: Duration = Duration::from_secs(3);
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(5);
const PIPE_READER_STACK_BYTES: usize = 128 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentKind {
    Claude,
    Codex,
    Kimi,
}

/// 세션에 바인딩된 에이전트 — transcript 경로까지 확정된 상태.
#[derive(Clone, PartialEq, Eq)]
pub struct AgentBinding {
    pub kind: AgentKind,
    pub session_id: String,
    pub transcript: PathBuf,
}

impl std::fmt::Debug for AgentBinding {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentBinding")
            .field("kind", &self.kind)
            .field("session_id", &"REDACTED")
            .field("transcript", &"REDACTED")
            .finish()
    }
}

struct ProcRow {
    pid: u32,
    ppid: Option<u32>,
    command: String,
}

/// 한 번의 감지 패스 결과.
///
/// `bindings`는 **transcript 경로까지 확정된** 것만 담는다(`AgentBinding` 정의). 그래서
/// 방금 띄워 아직 대화를 시작하지 않은 에이전트는 여기 없다 — 강도/모델 단축키는 바로
/// 그 시점(첫 프롬프트 전)에 쓰고 싶은 기능이라, 프로세스만으로 판정한 `kinds`를 함께
/// 낸다. transcript가 생기기 전에도 "이 pane은 Codex다"를 알 수 있어야 한다
/// (2026-08-02: 에이전트가 멀쩡히 도는데 bindings=0이라 단축키가 대상을 못 찾았다).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DetectedAgents {
    pub bindings: HashMap<SessionId, AgentBinding>,
    pub kinds: HashMap<SessionId, RunningAgent>,
}

/// 프로세스만으로 알아낸 세션의 에이전트. transcript도 statusLine도 필요 없다.
///
/// `model`/`effort`는 **런처가 argv에 넘긴 값**이라 실행 순간의 진실이다. statusLine은
/// 1시간 창(`STATUSLINES_PREFIX_PREFLIGHT`)으로 만료되므로, 오래 유휴한 세션에서는
/// 이쪽이 유일한 근거가 된다 — 실증(2026-08-03): 7시간 전 statusLine 행이 걸러져
/// `effort=None`이 되자 강도 단축키가 조용히 아무것도 하지 않았다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningAgent {
    pub kind: AgentKind,
    pub model: Option<String>,
    pub effort: Option<String>,
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

#[derive(Default)]
struct DetectionBudget {
    lsof_calls: usize,
}

impl DetectionBudget {
    fn consume_lsof(&mut self) -> Option<()> {
        self.lsof_calls = self.lsof_calls.checked_add(1)?;
        (self.lsof_calls <= MAX_LSOF_CALLS_PER_DETECTION).then_some(())
    }
}

/// 세션별로 **실행 중인 에이전트 종류**만 고른다 — transcript를 요구하지 않는다.
///
/// `AgentBinding`은 정의상 transcript 경로까지 확정된 상태라, 방금 띄워 아직 대화를
/// 시작하지 않은 에이전트는 바인딩되지 않는다. 강도/모델 단축키는 바로 그 시점(첫
/// 프롬프트 전)에 쓰고 싶은 기능이므로 프로세스만으로 판정할 수단이 따로 필요하다.
fn agent_kinds_from_rows(
    sessions: &[(SessionId, u32)],
    rows: &[ProcRow],
) -> HashMap<SessionId, RunningAgent> {
    sessions
        .iter()
        .filter_map(|(sid, shell_pid)| {
            let descendants = descendant_pids(*shell_pid, rows);
            rows.iter()
                .filter(|row| descendants.contains(&row.pid))
                .find_map(|row| {
                    classify(&row.command).map(|(kind, _)| RunningAgent {
                        kind,
                        model: argv_flag_value(&row.command, "--model"),
                        effort: argv_flag_value(&row.command, "--effort"),
                    })
                })
                .map(|agent| (*sid, agent))
        })
        .collect()
}

/// `--model opus[1m]` 처럼 **공백으로 분리된** 플래그 값을 argv 문자열에서 뽑는다.
///
/// `ps`가 준 한 줄이라 인용 정보가 없다. 값에 공백이 있으면 잘리는데, 우리가 읽는
/// `--model`/`--effort`는 공백 없는 토큰이라 문제되지 않는다. `--model=x` 형태는
/// 이 런처가 쓰지 않으므로 다루지 않는다 — 쓰게 되면 여기서 함께 처리해야 한다.
fn argv_flag_value(command: &str, flag: &str) -> Option<String> {
    let mut parts = command.split_whitespace();
    while let Some(part) = parts.next() {
        if part == flag {
            let value = parts.next()?;
            // 다음 토큰이 또 플래그면 값이 없는 것이다.
            if value.starts_with('-') {
                return None;
            }
            return Some(value.to_owned());
        }
    }
    None
}

/// 캐시를 활용한 detect. `cache`는 호출측(워커 스레드)이 소유·유지한다.
/// 프로세스만 보고 세션별 에이전트 종류를 판정한다 — `ps` 한 번, lsof도 transcript도
/// 타지 않는다.
///
/// 바인딩 tier(2.5s)는 lsof·transcript까지 도는 무거운 패스라 자주 돌릴 수 없다. 그런데
/// 「빈 터미널에서 손으로 에이전트를 띄운 경우」는 세션 목록이 그대로라 즉시 트리거도
/// 없어, 카드가 뜨기까지 그 주기를 통째로 기다렸다(2026-08-09 사용자 신고). 종류만
/// 필요한 그 경우를 위해 싼 패스를 따로 연다.
pub fn detect_kinds(sessions: &[(SessionId, u32)]) -> HashMap<SessionId, RunningAgent> {
    if sessions.len() > MAX_SESSIONS {
        return HashMap::new();
    }
    agent_kinds_from_rows(sessions, &process_rows())
}

pub fn detect_cached(
    sessions: &[(SessionId, u32)],
    overrides: &HashMap<SessionId, AgentBinding>,
    cache: &mut BindingCache,
) -> DetectedAgents {
    if sessions.len() > MAX_SESSIONS {
        cache.entries.clear();
        return DetectedAgents::default();
    }
    let rows = process_rows();
    let mut budget = DetectionBudget::default();
    let mut out = HashMap::new();
    // transcript와 무관하게 "이 세션에서 무슨 에이전트가 돌고 있나"만 따로 모은다.
    // 같은 ps 결과를 재사용하므로 추가 비용이 없다.
    let kinds = agent_kinds_from_rows(sessions, &rows);
    for (sid, shell_pid) in sessions {
        // hook(SessionStart 등)이 보고한 바인딩이 있으면 그것이 결정적이다. 단 같은 pane에서
        // Codex를 종료한 뒤 Claude를 실행할 수 있으므로, 살아 있는 에이전트의 종류까지 hook
        // 기록과 일치할 때만 사용한다. 종류가 다르면 오래된 hook을 무시하고 아래 탐색으로
        // 현재 에이전트를 다시 바인딩한다(2026-07-20 실증).
        if let Some(b) = overrides.get(sid)
            && valid_binding(b)
            && let Some(owner) = find_agent_pid(*shell_pid, &rows, b.kind)
        {
            cache.entries.insert(*sid, (b.clone(), owner, true));
            out.insert(*sid, b.clone());
            continue;
        }
        // 캐시 히트 + owner 프로세스 종류 일치 → 재발견 스킵. pid 생존만 확인하면 오래된
        // Codex 바인딩에 새 Claude pid가 들어간 캐시가 계속 재사용될 수 있다. 단 휴리스틱
        // 바인딩은 lsof로 결정적 업그레이드를 시도한다(작업 중 rollout이 열리면 정확한
        // 파일로 교체).
        if let Some((binding, owner_pid, det)) = cache.entries.get(sid)
            && agent_pid_matches_kind(*owner_pid, binding.kind, &rows)
        {
            let (owner_pid, det) = (*owner_pid, *det);
            {
                if !det
                    && binding.kind == AgentKind::Codex
                    && let Some(t) = codex_open_rollout(owner_pid, &mut budget)
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
        if let Some((binding, owner_pid, det)) = find_agent(*shell_pid, &rows, &mut budget) {
            cache
                .entries
                .insert(*sid, (binding.clone(), owner_pid, det));
            out.insert(*sid, binding);
        } else {
            cache.entries.remove(sid);
        }
    }
    // 더는 존재하지 않는 세션의 캐시 항목 정리(누수 방지).
    let alive: HashSet<SessionId> = sessions.iter().map(|(s, _)| *s).collect();
    cache.entries.retain(|sid, _| alive.contains(sid));
    DetectedAgents {
        bindings: out,
        kinds,
    }
}

/// 바인딩된 transcript를 파싱해 전체 상태(활동 + 표시 정보)를 읽는다. 파싱은 한 번만.
pub fn agent_state(binding: &AgentBinding) -> Option<agent_transcript::TranscriptState> {
    if !valid_binding(binding) {
        return None;
    }
    match binding.kind {
        AgentKind::Claude => agent_transcript::parse_claude(&binding.transcript),
        AgentKind::Codex => agent_transcript::parse_codex(&binding.transcript),
        AgentKind::Kimi => agent_transcript::parse_kimi(&binding.transcript),
    }
}

/// 바인딩된 transcript를 파싱해 현재 활동(working/idle)을 읽는다.
pub fn activity(binding: &AgentBinding) -> Option<agent_transcript::AgentActivity> {
    agent_state(binding).map(|s| s.activity)
}

/// 3줄 세션 행 2행 표시 정보 (2026-07-08). kind는 바인딩에서, model/effort/context는
/// transcript(codex 전부 / claude는 model만 — effort/context는 statusLine→DB)에서.
#[derive(Clone, PartialEq, Eq)]
pub struct AgentDisplay {
    pub kind: AgentKind,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub context_pct: Option<u8>,
    /// transcript의 최신 에이전트 응답/진행 메시지 — 사이드바 작업 설명용.
    pub last_agent_summary: Option<String>,
}

impl std::fmt::Debug for AgentDisplay {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentDisplay")
            .field("kind", &self.kind)
            .field("model_present", &self.model.is_some())
            .field("effort_present", &self.effort.is_some())
            .field("context_pct", &self.context_pct)
            .field(
                "last_agent_summary",
                &self.last_agent_summary.as_ref().map(|_| "REDACTED"),
            )
            .finish()
    }
}

/// 셸 pid의 자손 중 claude/codex를 찾아 transcript까지 바인딩한다. 캐시 생존 확인용으로
/// 그 에이전트 프로세스 pid도 함께 돌려준다.
/// 셸 자손 중 hook 바인딩과 같은 종류의 에이전트 프로세스가 있으면 그 pid를 돌려준다.
/// 종류를 대조하지 않으면 같은 PTY에서 Codex → Claude 전환 시 오래된 Codex hook이 새
/// Claude 프로세스를 자신의 owner로 오인한다.
fn find_agent_pid(shell_pid: u32, rows: &[ProcRow], expected: AgentKind) -> Option<u32> {
    let descendants = descendant_pids(shell_pid, rows);
    rows.iter()
        .filter(|r| descendants.contains(&r.pid))
        .find(|r| classify(&r.command).is_some_and(|(kind, _)| kind == expected))
        .map(|r| r.pid)
}

/// 캐시 owner pid가 여전히 같은 종류의 에이전트인지 확인한다. 프로세스가 끝났거나 pid가
/// 다른 공급자 프로세스로 바뀌면 false여서 transcript를 다시 탐색한다.
fn agent_pid_matches_kind(owner_pid: u32, expected: AgentKind, rows: &[ProcRow]) -> bool {
    rows.iter()
        .find(|r| r.pid == owner_pid)
        .and_then(|r| classify(&r.command))
        .is_some_and(|(kind, _)| kind == expected)
}

fn find_agent(
    shell_pid: u32,
    rows: &[ProcRow],
    budget: &mut DetectionBudget,
) -> Option<(AgentBinding, u32, bool)> {
    let descendants = descendant_pids(shell_pid, rows);
    // 한 셸에 에이전트가 여럿일 수 있다(^Z 중단 후 재실행 등, 2026-07-07 실증: codex 2개).
    // 선택 기준: ①결정적(argv/lsof) 바인딩이 휴리스틱(cwd)보다 우선 — 휴리스틱 mtime이
    // 더 최신이어도 오바인딩일 수 있다(실증: fresh codex가 resume 세션으로 오바인딩).
    // ②같은 등급 안에선 transcript mtime 최신(활성 대화가 append 중인 쪽).
    let mut best: Option<(AgentBinding, u32, bool, Option<std::time::SystemTime>)> = None;
    let mut candidates = 0usize;
    for row in rows.iter().filter(|r| descendants.contains(&r.pid)) {
        if let Some((kind, sid_hint)) = classify(&row.command) {
            candidates = candidates.checked_add(1)?;
            if candidates > MAX_AGENT_CANDIDATES_PER_SESSION {
                return None;
            }
            let Some((b, det)) = bind_transcript(kind, sid_hint, row.pid, budget) else {
                continue;
            };
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
    // Kimi는 런처가 `kimi`로 띄우지만 실제 워커 프로세스명은 `kimi-code`다
    // (2026-08-09 실측: pid 21295 `kimi`, pid 27534 `kimi-code`가 함께 뜬다).
    // 둘 중 하나만 보면 세션을 놓친다.
    if is("kimi") || is("kimi-code") {
        return Some((AgentKind::Kimi, None));
    }
    None
}

/// claude argv의 `--session-id <uuid>` 추출.
fn claude_session_id(command: &str) -> Option<String> {
    let mut it = command.split_whitespace();
    while let Some(tok) = it.next() {
        if tok == "--session-id" {
            return it
                .next()
                .filter(|value| valid_session_id(value))
                .map(str::to_owned);
        }
        if let Some(v) = tok.strip_prefix("--session-id=") {
            return valid_session_id(v).then(|| v.to_owned());
        }
    }
    None
}

fn valid_session_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_SESSION_ID_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn valid_absolute_path(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_PATH_BYTES
        && !value.as_bytes().contains(&0)
        && Path::new(value).is_absolute()
        && !Path::new(value).components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
}

fn valid_transcript_path(kind: AgentKind, path: &Path) -> bool {
    let Some(path_text) = path.to_str() else {
        return false;
    };
    if !valid_absolute_path(path_text) || path.extension().is_none_or(|ext| ext != "jsonl") {
        return false;
    }
    let Some(home) = crate::paths::home_dir() else {
        return false;
    };
    let root = match kind {
        AgentKind::Claude => home.join(".claude/projects"),
        AgentKind::Codex => home.join(".codex/sessions"),
        AgentKind::Kimi => home.join(".kimi-code/sessions"),
    };
    let Ok(root) = std::fs::canonicalize(root) else {
        return false;
    };
    let Ok(path) = std::fs::canonicalize(path) else {
        return false;
    };
    path.starts_with(root)
        && path
            .extension()
            .is_some_and(|extension| extension == "jsonl")
        && path.is_file()
}

fn valid_binding(binding: &AgentBinding) -> bool {
    valid_session_id(&binding.session_id)
        && valid_transcript_path(binding.kind, &binding.transcript)
}

/// 감지된 에이전트를 실제 transcript 파일로 확정한다. 두 번째 반환값 = 결정적 여부:
/// argv 세션ID(claude)·lsof 열린 파일(codex)은 결정적, cwd 매칭은 휴리스틱(오바인딩 가능 —
/// 2026-07-07 실증: 프로세스 cwd(/Users)와 rollout 기록 cwd(arteawiki)가 다르거나, 같은
/// cwd 다중 세션이 최신 쪽으로 모임). find_agent가 결정적 후보를 우선한다.
fn bind_transcript(
    kind: AgentKind,
    sid_hint: Option<String>,
    pid: u32,
    budget: &mut DetectionBudget,
) -> Option<(AgentBinding, bool)> {
    match kind {
        // transcript 바인딩은 파서가 있어야 의미가 있다. Kimi는 아직 없으므로
        // 프로세스 감지(agent_kinds)까지만 남고 바인딩은 만들지 않는다.
        AgentKind::Kimi => None,
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
            let cwd = process_cwd(pid, budget)?;
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
            if let Some(transcript) = codex_open_rollout(pid, budget)
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
            let cwd = process_cwd(pid, budget)?;
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
///
/// codex 확인은 `~/.codex/sessions` 재귀 스캔(최대 16,384 entries/4,096 files)이라, 복원 pass가
/// pane마다 반복하지 않게 스캔 결과를 finder 수명 동안 1회만 수집해 재사용한다 —
/// 호출측(UI 스레드)이 pane 수 × 스캔 비용을 물지 않는다.
#[derive(Default)]
pub struct TranscriptFinder {
    codex_files: Option<Vec<PathBuf>>,
}

impl TranscriptFinder {
    pub fn new() -> Self {
        Self { codex_files: None }
    }

    pub fn find(&mut self, kind: AgentKind, session_id: &str) -> Option<PathBuf> {
        if !valid_session_id(session_id) {
            return None;
        }
        match kind {
            AgentKind::Kimi => None,
            AgentKind::Claude => find_claude_transcript(session_id),
            AgentKind::Codex => {
                let files = self.codex_files.get_or_insert_with(|| {
                    let mut files = Vec::new();
                    if let Some(home) = crate::paths::home_dir() {
                        let _ = collect_jsonl(&home.join(".codex/sessions"), &mut files);
                    }
                    files
                });
                files
                    .iter()
                    .find(|p| {
                        p.file_name()
                            .and_then(|n| n.to_str())
                            .and_then(agent_transcript::codex_session_id)
                            .as_deref()
                            == Some(session_id)
                    })
                    .cloned()
            }
        }
    }
}

/// Opaque, allocation-canonicalized input for one off-thread resume transcript probe.
///
/// The session identifier is deliberately private and its `Debug` representation is redacted.
/// Turning the input `String` into a boxed string drops arbitrary spare capacity before the
/// request can be retained by the AgentState worker.
pub(crate) struct ResumeTranscriptProbe {
    kind: AgentKind,
    session_id: Box<str>,
}

impl ResumeTranscriptProbe {
    pub(crate) fn try_new(
        kind: AgentKind,
        session_id: String,
    ) -> Result<Self, ResumeTranscriptProbeError> {
        if !valid_session_id(&session_id) {
            return Err(ResumeTranscriptProbeError::InvalidRequest);
        }
        Ok(Self {
            kind,
            session_id: session_id.into_boxed_str(),
        })
    }

    pub(crate) fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>() + self.session_id.len()
    }
}

impl std::fmt::Debug for ResumeTranscriptProbe {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResumeTranscriptProbe")
            .field("kind", &self.kind)
            .field("session_id", &"REDACTED")
            .finish()
    }
}

/// Sanitized worker-side probe output. Transcript paths never cross the worker boundary, and the
/// cwd is retained only when it is a bounded absolute path to a directory at probe time.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ResumeTranscriptProbeResult {
    found: bool,
    cwd: Option<Box<str>>,
}

impl ResumeTranscriptProbeResult {
    pub(crate) const fn found(&self) -> bool {
        self.found
    }

    pub(crate) fn cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }
}

impl std::fmt::Debug for ResumeTranscriptProbeResult {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResumeTranscriptProbeResult")
            .field("found", &self.found)
            .field("cwd_present", &self.cwd.is_some())
            .finish()
    }
}

/// Low-cardinality failure returned before any raw identifier or filesystem error can escape the
/// worker-side probe boundary.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResumeTranscriptProbeError {
    InvalidRequest,
    ResourceLimit,
}

impl ResumeTranscriptProbeError {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::ResourceLimit => "resource_limit",
        }
    }
}

impl std::fmt::Debug for ResumeTranscriptProbeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

fn check_resume_probe_limit(
    items: usize,
    retained_bytes: usize,
    bytes_max: usize,
) -> Result<(), ResumeTranscriptProbeError> {
    if items > RESUME_PROBE_ITEMS_MAX || retained_bytes > bytes_max {
        return Err(ResumeTranscriptProbeError::ResourceLimit);
    }
    Ok(())
}

/// Resolve one bounded batch on the caller's worker thread. One finder is reused for the whole
/// batch, so Codex's bounded recursive scan is performed at most once. All request item/byte caps
/// are checked before the first filesystem lookup.
pub(crate) fn probe_resume_transcripts(
    requests: &[ResumeTranscriptProbe],
) -> Result<Vec<ResumeTranscriptProbeResult>, ResumeTranscriptProbeError> {
    let mut finder = TranscriptFinder::new();
    probe_resume_transcripts_with(
        requests,
        |kind, session_id| finder.find(kind, session_id),
        transcript_cwd,
        |cwd| Path::new(cwd).is_dir(),
    )
}

fn probe_resume_transcripts_with(
    requests: &[ResumeTranscriptProbe],
    mut find: impl FnMut(AgentKind, &str) -> Option<PathBuf>,
    mut read_cwd: impl FnMut(&Path) -> Option<String>,
    mut is_directory: impl FnMut(&str) -> bool,
) -> Result<Vec<ResumeTranscriptProbeResult>, ResumeTranscriptProbeError> {
    let request_bytes = requests.iter().try_fold(0usize, |total, request| {
        total.checked_add(request.retained_bytes())
    });
    let request_bytes = request_bytes.ok_or(ResumeTranscriptProbeError::ResourceLimit)?;
    check_resume_probe_limit(
        requests.len(),
        request_bytes,
        RESUME_PROBE_REQUEST_BYTES_MAX,
    )?;
    let maximum_result_bytes = std::mem::size_of::<ResumeTranscriptProbeResult>()
        .checked_add(MAX_PATH_BYTES)
        .and_then(|bytes| bytes.checked_mul(requests.len()))
        .ok_or(ResumeTranscriptProbeError::ResourceLimit)?;
    check_resume_probe_limit(
        requests.len(),
        maximum_result_bytes,
        RESUME_PROBE_RESULT_BYTES_MAX,
    )?;

    let mut results = Vec::with_capacity(requests.len());
    let mut result_bytes = std::mem::size_of::<ResumeTranscriptProbeResult>()
        .checked_mul(requests.len())
        .ok_or(ResumeTranscriptProbeError::ResourceLimit)?;
    for request in requests {
        let Some(transcript) = find(request.kind, &request.session_id) else {
            results.push(ResumeTranscriptProbeResult {
                found: false,
                cwd: None,
            });
            continue;
        };
        let cwd = read_cwd(&transcript)
            .filter(|cwd| valid_absolute_path(cwd))
            .filter(|cwd| is_directory(cwd))
            .map(String::into_boxed_str);
        result_bytes = result_bytes
            .checked_add(cwd.as_ref().map_or(0, |cwd| cwd.len()))
            .ok_or(ResumeTranscriptProbeError::ResourceLimit)?;
        check_resume_probe_limit(requests.len(), result_bytes, RESUME_PROBE_RESULT_BYTES_MAX)?;
        results.push(ResumeTranscriptProbeResult { found: true, cwd });
    }
    Ok(results)
}

/// transcript(jsonl) 앞부분에서 세션의 원래 cwd를 읽는다 — 복원 resume 시 그 폴더로
/// `cd` 하기 위함(2026-07-08: resume은 이어졌는데 셸 폴더가 workspace 루트라 실제 작업
/// 폴더와 달랐다). codex rollout은 1행 session_meta payload.cwd, claude는 초반 행들에
/// "cwd" 필드. 파싱 실패 줄은 건너뛴다.
pub fn transcript_cwd(path: &std::path::Path) -> Option<String> {
    if std::fs::symlink_metadata(path)
        .ok()?
        .file_type()
        .is_symlink()
    {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let mut observed_bytes = 0usize;
    for _ in 0..MAX_TRANSCRIPT_HEAD_LINES {
        let line = read_line_bounded(&mut reader, MAX_TRANSCRIPT_LINE_BYTES).ok()??;
        observed_bytes = observed_bytes.checked_add(line.len())?;
        if observed_bytes > MAX_TRANSCRIPT_HEAD_BYTES {
            return None;
        }
        let Ok(line) = std::str::from_utf8(&line) else {
            return None;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let Some(cwd) = v
            .get("cwd")
            .and_then(|x| x.as_str())
            .or_else(|| v.pointer("/payload/cwd").and_then(|x| x.as_str()))
            .filter(|cwd| valid_absolute_path(cwd))
        {
            return Some(cwd.to_owned());
        }
    }
    None
}

fn read_line_bounded(
    reader: &mut impl std::io::BufRead,
    max_bytes: usize,
) -> Result<Option<Vec<u8>>, ()> {
    let hard_limit = max_bytes.checked_add(1).ok_or(())?;
    let mut line = Vec::with_capacity(max_bytes.min(8 * 1024));
    let read = reader
        .take(hard_limit as u64)
        .read_until(b'\n', &mut line)
        .map_err(|_| ())?;
    if read == 0 {
        return Ok(None);
    }
    if line.len() > max_bytes {
        return Err(());
    }
    Ok(Some(line))
}

/// "claude"/"codex" 문자열 → AgentKind.
pub fn kind_from_str(s: &str) -> Option<AgentKind> {
    match s {
        "claude" => Some(AgentKind::Claude),
        "codex" => Some(AgentKind::Codex),
        "kimi" => Some(AgentKind::Kimi),
        _ => None,
    }
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
    if !valid_absolute_path(cwd) {
        return None;
    }
    let dir = crate::paths::home_dir()?
        .join(".claude/projects")
        .join(claude_project_dir_escape(cwd));
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    let mut entries = std::fs::read_dir(dir).ok()?;
    for _ in 0..MAX_DIRECTORY_ENTRIES {
        let Some(e) = entries.next() else { break };
        let Ok(e) = e else { return None };
        let p = e.path();
        if e.file_type().ok().is_some_and(|kind| kind.is_file())
            && p.extension().is_some_and(|x| x == "jsonl")
            && let Ok(meta) = e.metadata()
            && let Ok(mtime) = meta.modified()
            && best.as_ref().is_none_or(|(bm, _)| mtime > *bm)
        {
            best = Some((mtime, p));
        }
    }
    if entries.next().is_some() {
        return None;
    }
    let (_, p) = best?;
    let sid = p.file_stem()?.to_str()?;
    if !valid_session_id(sid) {
        return None;
    }
    let sid = sid.to_owned();
    Some((sid, p))
}

/// 세션ID로 claude transcript를 찾는다 (`~/.claude/projects/*/<sid>.jsonl`).
fn find_claude_transcript(session_id: &str) -> Option<PathBuf> {
    if !valid_session_id(session_id) {
        return None;
    }
    let projects = crate::paths::home_dir()?.join(".claude/projects");
    let mut entries = std::fs::read_dir(projects).ok()?;
    for _ in 0..MAX_DIRECTORY_ENTRIES {
        let Some(proj) = entries.next() else {
            break;
        };
        let Ok(proj) = proj else { return None };
        if !proj.file_type().ok().is_some_and(|kind| kind.is_dir()) {
            continue;
        }
        let candidate = proj.path().join(format!("{session_id}.jsonl"));
        if std::fs::symlink_metadata(&candidate)
            .ok()
            .is_some_and(|meta| meta.file_type().is_file())
        {
            return Some(candidate);
        }
    }
    if entries.next().is_some() {
        return None;
    }
    None
}

/// cwd로 codex rollout을 찾는다 — session_meta.cwd가 일치하는 것 중 가장 최근.
fn find_codex_transcript(cwd: &str) -> Option<(String, PathBuf)> {
    if !valid_absolute_path(cwd) {
        return None;
    }
    let root = crate::paths::home_dir()?.join(".codex/sessions");
    let mut files = Vec::new();
    if !collect_jsonl(&root, &mut files) {
        return None;
    }
    files.sort_by_cached_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
    for p in files.iter().rev().take(64) {
        if transcript_cwd(p).as_deref() == Some(cwd)
            && let Some(session_id) = p
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(agent_transcript::codex_session_id)
        {
            return Some((session_id, p.clone()));
        }
    }
    None
}

fn collect_jsonl(dir: &Path, out: &mut Vec<PathBuf>) -> bool {
    let mut budget = ScanBudget::default();
    if collect_jsonl_bounded(dir, out, 0, &mut budget) {
        true
    } else {
        out.clear();
        false
    }
}

#[derive(Default)]
struct ScanBudget {
    entries: usize,
}

/// 재귀 스캔에 상한을 둔다(codex 리뷰): ①심링크 미추적(루프·외부 거대 디렉터리 방지),
/// ②depth 상한, ③operation 전체 디렉터리 항목/파일 수 상한. resume 존재확인이 UI
/// 스레드에서도 부르므로 무한 재귀/대량 스캔으로 인한 freeze를 막는다.
///
/// **역순(최신 먼저) 순회**: ~/.codex/sessions는 YYYY/MM/DD 구조 + rollout-<타임스탬프> 파일명
/// 이라 이름 역순 = 시간 역순. 상한에 걸리면 '오래된 쪽'이 잘려야 한다 — 정순 순회는 rollout이
/// 상한(4096)을 넘는 순간 최신 세션이 누락돼 codex 감지가 죽었다(2026-07-07 실증: 7,447개).
fn collect_jsonl_bounded(
    dir: &Path,
    out: &mut Vec<PathBuf>,
    depth: usize,
    budget: &mut ScanBudget,
) -> bool {
    if depth > MAX_SCAN_DEPTH || out.len() >= MAX_SCAN_FILES || budget.entries >= MAX_SCAN_ENTRIES {
        return false;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return false;
    };
    // 이름 역순 정렬 — read_dir 순서는 비보장이라 명시 정렬해야 "최신 먼저"가 성립한다.
    let mut entries = Vec::with_capacity(MAX_DIRECTORY_ENTRIES.min(256));
    let mut directory_entries = 0usize;
    for entry in rd {
        directory_entries = match directory_entries.checked_add(1) {
            Some(entries) if entries <= MAX_DIRECTORY_ENTRIES => entries,
            _ => return false,
        };
        budget.entries = match budget.entries.checked_add(1) {
            Some(entries) if entries <= MAX_SCAN_ENTRIES => entries,
            _ => return false,
        };
        let Ok(entry) = entry else { return false };
        entries.push(entry);
    }
    entries.sort_by_key(|e| std::cmp::Reverse(e.file_name()));
    for e in entries {
        if out.len() >= MAX_SCAN_FILES {
            return false;
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
            if !collect_jsonl_bounded(&p, out, depth + 1, budget) {
                return false;
            }
        } else if p.extension().is_some_and(|x| x == "jsonl") {
            out.push(p);
        }
    }
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandError {
    SpawnFailed,
    PipeUnavailable,
    ReaderSpawnFailed,
    ReadFailed,
    OutputTooLarge,
    WaitFailed,
    TimedOut,
    ReaderPanicked,
    CommandFailed,
    InvalidUtf8,
}

struct BoundedCapture {
    bytes: Vec<u8>,
    truncated: bool,
}

fn read_bounded(
    mut reader: impl Read,
    max_bytes: usize,
    limit_reached: &AtomicBool,
    retain: bool,
) -> Result<BoundedCapture, CommandError> {
    let hard_limit = max_bytes
        .checked_add(1)
        .ok_or(CommandError::OutputTooLarge)?;
    let mut bytes = Vec::with_capacity(if retain { max_bytes.min(64 * 1024) } else { 0 });
    let mut observed = 0usize;
    let mut chunk = [0u8; 64 * 1024];
    while observed < hard_limit {
        let remaining = hard_limit - observed;
        let read_len = remaining.min(chunk.len());
        let count = reader
            .read(&mut chunk[..read_len])
            .map_err(|_| CommandError::ReadFailed)?;
        if count == 0 {
            break;
        }
        observed += count;
        if retain {
            bytes.extend_from_slice(&chunk[..count]);
        }
    }
    let truncated = observed > max_bytes;
    if truncated {
        if retain {
            bytes.truncate(max_bytes);
        }
        limit_reached.store(true, Ordering::Release);
    }
    Ok(BoundedCapture { bytes, truncated })
}

#[cfg(test)]
static ACTIVE_COMMAND_READERS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
struct ActiveCommandReader;

#[cfg(test)]
impl ActiveCommandReader {
    fn enter() -> Self {
        ACTIVE_COMMAND_READERS.fetch_add(1, Ordering::AcqRel);
        Self
    }
}

#[cfg(test)]
impl Drop for ActiveCommandReader {
    fn drop(&mut self) {
        ACTIVE_COMMAND_READERS.fetch_sub(1, Ordering::AcqRel);
    }
}

struct RunningCommand {
    child: Child,
    reaped: bool,
    stdout_reader: Option<std::thread::JoinHandle<Result<BoundedCapture, CommandError>>>,
    stderr_reader: Option<std::thread::JoinHandle<Result<BoundedCapture, CommandError>>>,
    output_limit_reached: Arc<AtomicBool>,
}

impl RunningCommand {
    fn spawn(
        program: &Path,
        args: &[&str],
        stdout_max_bytes: usize,
        stderr_max_bytes: usize,
    ) -> Result<Self, CommandError> {
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            command.process_group(0);
        }
        let mut child = command.spawn().map_err(|_| CommandError::SpawnFailed)?;
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                let _ = kill_and_reap_command(&mut child);
                return Err(CommandError::PipeUnavailable);
            }
        };
        let stderr = match child.stderr.take() {
            Some(stderr) => stderr,
            None => {
                drop(stdout);
                let _ = kill_and_reap_command(&mut child);
                return Err(CommandError::PipeUnavailable);
            }
        };
        let output_limit_reached = Arc::new(AtomicBool::new(false));
        let stdout_limit = Arc::clone(&output_limit_reached);
        let stdout_reader = match std::thread::Builder::new()
            .name("agent-detect-stdout".to_owned())
            .stack_size(PIPE_READER_STACK_BYTES)
            .spawn(move || {
                #[cfg(test)]
                let _active = ActiveCommandReader::enter();
                read_bounded(stdout, stdout_max_bytes, &stdout_limit, true)
            }) {
            Ok(reader) => reader,
            Err(_) => {
                drop(stderr);
                let _ = kill_and_reap_command(&mut child);
                return Err(CommandError::ReaderSpawnFailed);
            }
        };
        let stderr_limit = Arc::clone(&output_limit_reached);
        let stderr_reader = match std::thread::Builder::new()
            .name("agent-detect-stderr".to_owned())
            .stack_size(PIPE_READER_STACK_BYTES)
            .spawn(move || {
                #[cfg(test)]
                let _active = ActiveCommandReader::enter();
                read_bounded(stderr, stderr_max_bytes, &stderr_limit, false)
            }) {
            Ok(reader) => reader,
            Err(_) => {
                let _ = kill_and_reap_command(&mut child);
                let _ = stdout_reader.join();
                return Err(CommandError::ReaderSpawnFailed);
            }
        };
        Ok(Self {
            child,
            reaped: false,
            stdout_reader: Some(stdout_reader),
            stderr_reader: Some(stderr_reader),
            output_limit_reached,
        })
    }

    fn output_limit_reached(&self) -> bool {
        self.output_limit_reached.load(Ordering::Acquire)
    }

    #[cfg(unix)]
    fn try_wait(&mut self) -> Result<Option<ExitStatus>, CommandError> {
        // WNOWAIT holds the group leader pid until descendants have been killed. This prevents
        // pgid reuse between observing parent exit and closing inherited stdout/stderr pipes.
        let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                self.child.id() as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result != 0 {
            return Err(CommandError::WaitFailed);
        }
        if unsafe { info.si_pid() } == 0 {
            return Ok(None);
        }
        self.kill_group();
        let status = self.child.wait().map_err(|_| CommandError::WaitFailed)?;
        self.reaped = true;
        Ok(Some(status))
    }

    #[cfg(not(unix))]
    fn try_wait(&mut self) -> Result<Option<ExitStatus>, CommandError> {
        let status = self
            .child
            .try_wait()
            .map_err(|_| CommandError::WaitFailed)?;
        if status.is_some() {
            self.reaped = true;
        }
        Ok(status)
    }

    #[cfg(unix)]
    fn kill_group(&self) {
        unsafe {
            libc::killpg(self.child.id() as libc::pid_t, libc::SIGKILL);
        }
    }

    #[cfg(not(unix))]
    fn kill_group(&self) {}

    fn kill_and_reap(&mut self) -> Result<(), CommandError> {
        if !self.reaped {
            self.kill_group();
            let _ = self.child.kill();
            self.child.wait().map_err(|_| CommandError::WaitFailed)?;
            self.reaped = true;
        }
        Ok(())
    }

    fn join_readers(&mut self) -> Result<(BoundedCapture, BoundedCapture), CommandError> {
        fn join(
            reader: Option<std::thread::JoinHandle<Result<BoundedCapture, CommandError>>>,
        ) -> Result<BoundedCapture, CommandError> {
            reader
                .ok_or(CommandError::PipeUnavailable)?
                .join()
                .map_err(|_| CommandError::ReaderPanicked)?
        }
        let stdout = join(self.stdout_reader.take())?;
        let stderr = join(self.stderr_reader.take())?;
        Ok((stdout, stderr))
    }
}

impl Drop for RunningCommand {
    fn drop(&mut self) {
        let _ = self.kill_and_reap();
        let _ = self.join_readers();
    }
}

fn kill_and_reap_command(child: &mut Child) -> Result<(), CommandError> {
    #[cfg(unix)]
    unsafe {
        libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
    }
    let _ = child.kill();
    child.wait().map_err(|_| CommandError::WaitFailed)?;
    Ok(())
}

fn run_command_bounded(
    program: &Path,
    args: &[&str],
    stdout_max_bytes: usize,
) -> Result<String, CommandError> {
    run_command_bounded_with_timeout(program, args, stdout_max_bytes, PROCESS_TIMEOUT)
}

fn run_command_bounded_with_timeout(
    program: &Path,
    args: &[&str],
    stdout_max_bytes: usize,
    timeout: Duration,
) -> Result<String, CommandError> {
    let mut running = RunningCommand::spawn(program, args, stdout_max_bytes, MAX_CLI_STDERR_BYTES)?;
    let started = Instant::now();
    let status = loop {
        if running.output_limit_reached() {
            let _ = running.kill_and_reap();
            let _ = running.join_readers();
            return Err(CommandError::OutputTooLarge);
        }
        match running.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() >= timeout => {
                let _ = running.kill_and_reap();
                let _ = running.join_readers();
                return Err(CommandError::TimedOut);
            }
            Ok(None) => std::thread::sleep(PROCESS_POLL_INTERVAL.min(timeout)),
            Err(error) => {
                let _ = running.kill_and_reap();
                let _ = running.join_readers();
                return Err(error);
            }
        }
    };
    let (stdout, stderr) = running.join_readers()?;
    if stdout.truncated || stderr.truncated {
        return Err(CommandError::OutputTooLarge);
    }
    if !status.success() {
        return Err(CommandError::CommandFailed);
    }
    String::from_utf8(stdout.bytes).map_err(|_| CommandError::InvalidUtf8)
}

/// codex 프로세스가 열어둔 rollout(.jsonl) 파일 — `lsof -p <pid>`의 열린 파일 중
/// `~/.codex/sessions/**.jsonl`. 있으면 그게 곧 이 프로세스의 transcript(결정적).
#[cfg(unix)]
fn codex_open_rollout(pid: u32, budget: &mut DetectionBudget) -> Option<PathBuf> {
    budget.consume_lsof()?;
    let pid = pid.to_string();
    let out = run_command_bounded(
        Path::new("lsof"),
        &["-a", "-p", &pid, "-Fn"],
        MAX_LSOF_STDOUT_BYTES,
    )
    .ok()?;
    let logical_root = crate::paths::home_dir()?.join(".codex/sessions");
    let canonical_root = std::fs::canonicalize(&logical_root).ok()?;
    out.lines()
        .filter_map(|l| l.strip_prefix('n'))
        .filter(|path| valid_absolute_path(path))
        .map(Path::new)
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
                && (path.starts_with(&logical_root) || path.starts_with(&canonical_root))
        })
        .find(|path| valid_transcript_path(AgentKind::Codex, path))
        .map(PathBuf::from)
}

#[cfg(not(unix))]
fn codex_open_rollout(_pid: u32, _budget: &mut DetectionBudget) -> Option<PathBuf> {
    None
}

/// 여러 셸 pid의 현재 작업 디렉터리를 **한 번의 lsof**로 얻는다(세션 행 폴더명 +
/// 워크스페이스 이름 추적, 2026-07-08). off-thread 호출. `-p pid1,pid2,...`는 lsof가
/// 지원하는 다중 pid 형식이라 세션 수만큼 프로세스를 띄우지 않는다.
#[cfg(unix)]
pub(crate) fn session_cwds(pids: &[u32]) -> HashMap<u32, String> {
    let mut out = HashMap::new();
    if pids.is_empty() || pids.len() > MAX_SESSIONS || pids.contains(&0) {
        return out;
    }
    let unique: HashSet<u32> = pids.iter().copied().collect();
    if unique.len() != pids.len() {
        return out;
    }
    let list = pids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let output = run_command_bounded(
        Path::new("lsof"),
        &["-a", "-d", "cwd", "-p", &list, "-Fpn"],
        MAX_LSOF_STDOUT_BYTES,
    );
    let Ok(output) = output else { return out };
    // -Fpn: 'p<pid>' 라인 뒤에 그 프로세스의 'n<경로>' 라인이 온다.
    let mut cur: Option<u32> = None;
    for line in output.lines() {
        if let Some(pid) = line.strip_prefix('p') {
            cur = pid.parse().ok().filter(|pid| unique.contains(pid));
        } else if let Some(path) = line.strip_prefix('n')
            && let Some(pid) = cur
            && valid_absolute_path(path)
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
    if !valid_absolute_path(cwd) {
        return None;
    }
    let path = std::path::Path::new(cwd);
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

/// 프로세스의 cwd — 감지 pass의 lsof 호출 예산을 공유한다.
fn process_cwd(pid: u32, budget: &mut DetectionBudget) -> Option<String> {
    budget.consume_lsof()?;
    session_cwds(&[pid]).remove(&pid)
}

/// 셸 pid의 모든 자손 pid 집합 (resource_monitor와 동일한 BFS).
fn descendant_pids(root_pid: u32, rows: &[ProcRow]) -> std::collections::HashSet<u32> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for row in rows.iter().take(MAX_PROCESS_ROWS) {
        if let Some(parent) = row.ppid {
            children.entry(parent).or_default().push(row.pid);
        }
    }
    let mut wanted = HashSet::from([root_pid]);
    let mut pending = VecDeque::from([root_pid]);
    while let Some(parent) = pending.pop_front() {
        if let Some(child_pids) = children.get(&parent) {
            for &pid in child_pids {
                if wanted.len() >= MAX_DESCENDANTS_PER_SESSION {
                    return HashSet::new();
                }
                if wanted.insert(pid) {
                    pending.push_back(pid);
                }
            }
        }
    }
    wanted
}

#[cfg(unix)]
fn process_rows() -> Vec<ProcRow> {
    let output = run_command_bounded(
        Path::new("ps"),
        &["-axo", "pid=,ppid=,command="],
        MAX_PS_STDOUT_BYTES,
    );
    let Ok(output) = output else {
        return Vec::new();
    };
    let mut rows = Vec::with_capacity(MAX_PROCESS_ROWS.min(1_024));
    for line in output.lines() {
        if rows.len() >= MAX_PROCESS_ROWS {
            return Vec::new();
        }
        let Some(row) = parse_ps_line(line) else {
            return Vec::new();
        };
        rows.push(row);
    }
    if rows.is_empty() && !output.is_empty() {
        return Vec::new();
    }
    rows
}

#[cfg(not(unix))]
fn process_rows() -> Vec<ProcRow> {
    Vec::new()
}

/// `ps -axo pid=,ppid=,command=` 한 줄: "pid ppid command with spaces".
/// pid/ppid는 우측정렬이라 공백이 여러 칸일 수 있어 whitespace run으로 자른다.
fn parse_ps_line(line: &str) -> Option<ProcRow> {
    if line.len() > MAX_PROCESS_COMMAND_BYTES.saturating_add(32) {
        return None;
    }
    let line = line.trim_start();
    let (pid_str, rest) = line.split_once(char::is_whitespace)?;
    let (ppid_str, rest) = rest.trim_start().split_once(char::is_whitespace)?;
    let pid = pid_str.parse().ok()?;
    if pid == 0 {
        return None;
    }
    let ppid = Some(ppid_str.parse().ok()?);
    let command = rest.trim_start();
    if command.is_empty() || command.len() > MAX_PROCESS_COMMAND_BYTES {
        return None;
    }
    let command = command.to_owned();
    Some(ProcRow { pid, ppid, command })
}

#[cfg(test)]
mod tests {
    use super::*;

    static COMMAND_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn temp_dir(label: &str) -> PathBuf {
        static SEQUENCE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "deppy-agent-detect-{label}-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn bounded_capture_accepts_exact_and_rejects_plus_one() {
        let exact_limit = AtomicBool::new(false);
        let exact =
            read_bounded(std::io::Cursor::new(vec![b'x'; 64]), 64, &exact_limit, true).unwrap();
        assert_eq!(exact.bytes.len(), 64);
        assert!(!exact.truncated);
        assert!(!exact_limit.load(Ordering::Acquire));

        let plus_one_limit = AtomicBool::new(false);
        let plus_one = read_bounded(
            std::io::Cursor::new(vec![b'x'; 65]),
            64,
            &plus_one_limit,
            true,
        )
        .unwrap();
        assert_eq!(plus_one.bytes.len(), 64);
        assert!(plus_one.truncated);
        assert!(plus_one_limit.load(Ordering::Acquire));
    }

    #[test]
    fn bounded_line_accepts_exact_and_rejects_plus_one() {
        let mut exact = std::io::BufReader::new(std::io::Cursor::new(vec![b'x'; 64]));
        assert_eq!(
            read_line_bounded(&mut exact, 64).unwrap().unwrap().len(),
            64
        );

        let mut plus_one = std::io::BufReader::new(std::io::Cursor::new(vec![b'x'; 65]));
        assert_eq!(read_line_bounded(&mut plus_one, 64), Err(()));
    }

    #[test]
    fn hostile_identifiers_commands_and_debug_are_fail_closed() {
        assert!(!valid_session_id("../../secrets"));
        assert!(!valid_session_id(&"a".repeat(MAX_SESSION_ID_BYTES + 1)));
        assert!(valid_session_id(&"a".repeat(MAX_SESSION_ID_BYTES)));
        assert!(!valid_absolute_path("/tmp/../private/secret"));
        assert!(!valid_absolute_path(&format!(
            "/{}",
            "x".repeat(MAX_PATH_BYTES + 1)
        )));
        assert!(
            parse_ps_line(&format!(
                "1 0 {}",
                "x".repeat(MAX_PROCESS_COMMAND_BYTES + 1)
            ))
            .is_none()
        );

        let binding = AgentBinding {
            kind: AgentKind::Codex,
            session_id: "private-session".to_owned(),
            transcript: PathBuf::from("/private/transcript.jsonl"),
        };
        let debug = format!("{binding:?}");
        assert!(debug.contains("REDACTED"));
        assert!(!debug.contains("private-session"));
        assert!(!debug.contains("/private/transcript.jsonl"));
    }

    #[test]
    fn transcript_head_rejects_oversized_and_hostile_cwd() {
        let root = temp_dir("transcript-head");
        let transcript = root.join("transcript.jsonl");
        std::fs::write(&transcript, vec![b'x'; MAX_TRANSCRIPT_LINE_BYTES + 1]).unwrap();
        assert_eq!(transcript_cwd(&transcript), None);

        std::fs::write(&transcript, b"{\"cwd\":\"/tmp/../private\"}\n").unwrap();
        assert_eq!(transcript_cwd(&transcript), None);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resume_probe_limits_accept_exact_and_reject_plus_one() {
        assert_eq!(
            check_resume_probe_limit(
                RESUME_PROBE_ITEMS_MAX,
                RESUME_PROBE_REQUEST_BYTES_MAX,
                RESUME_PROBE_REQUEST_BYTES_MAX,
            ),
            Ok(())
        );
        assert_eq!(
            check_resume_probe_limit(
                RESUME_PROBE_ITEMS_MAX + 1,
                RESUME_PROBE_REQUEST_BYTES_MAX,
                RESUME_PROBE_REQUEST_BYTES_MAX,
            ),
            Err(ResumeTranscriptProbeError::ResourceLimit)
        );
        assert_eq!(
            check_resume_probe_limit(
                RESUME_PROBE_ITEMS_MAX,
                RESUME_PROBE_REQUEST_BYTES_MAX + 1,
                RESUME_PROBE_REQUEST_BYTES_MAX,
            ),
            Err(ResumeTranscriptProbeError::ResourceLimit)
        );
        assert_eq!(
            check_resume_probe_limit(
                RESUME_PROBE_ITEMS_MAX,
                RESUME_PROBE_RESULT_BYTES_MAX,
                RESUME_PROBE_RESULT_BYTES_MAX,
            ),
            Ok(())
        );
        assert_eq!(
            check_resume_probe_limit(
                RESUME_PROBE_ITEMS_MAX,
                RESUME_PROBE_RESULT_BYTES_MAX + 1,
                RESUME_PROBE_RESULT_BYTES_MAX,
            ),
            Err(ResumeTranscriptProbeError::ResourceLimit)
        );
    }

    #[test]
    fn resume_probe_rejects_oversized_batch_before_lookup() {
        let requests = (0..=RESUME_PROBE_ITEMS_MAX)
            .map(|index| {
                ResumeTranscriptProbe::try_new(AgentKind::Codex, format!("session-{index}"))
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let lookups = std::cell::Cell::new(0usize);
        let result = probe_resume_transcripts_with(
            &requests,
            |_, _| {
                lookups.set(lookups.get() + 1);
                None
            },
            |_| None,
            |_| false,
        );
        assert_eq!(result, Err(ResumeTranscriptProbeError::ResourceLimit));
        assert_eq!(lookups.get(), 0);
    }

    #[test]
    fn resume_probe_sanitizes_missing_invalid_and_valid_transcripts() {
        let root = temp_dir("resume-probe");
        let valid_cwd = root.join("valid-cwd");
        std::fs::create_dir_all(&valid_cwd).unwrap();
        let malformed = root.join("malformed.jsonl");
        let hostile_cwd = root.join("hostile-cwd.jsonl");
        let missing_cwd = root.join("missing-cwd.jsonl");
        let valid = root.join("valid.jsonl");
        std::fs::write(&malformed, b"not-json\n").unwrap();
        std::fs::write(&hostile_cwd, b"{\"cwd\":\"../private\"}\n").unwrap();
        std::fs::write(
            &missing_cwd,
            b"{\"cwd\":\"/definitely/missing/deppy-resume-probe\"}\n",
        )
        .unwrap();
        std::fs::write(
            &valid,
            format!("{{\"cwd\":{:?}}}\n", valid_cwd.to_str().unwrap()),
        )
        .unwrap();
        let requests = [
            ("missing", None),
            ("malformed", Some(malformed)),
            ("hostile", Some(hostile_cwd)),
            ("missing-cwd", Some(missing_cwd)),
            ("valid", Some(valid)),
        ];
        let probes = requests
            .iter()
            .map(|(session_id, _)| {
                ResumeTranscriptProbe::try_new(AgentKind::Codex, (*session_id).to_owned()).unwrap()
            })
            .collect::<Vec<_>>();
        let results = probe_resume_transcripts_with(
            &probes,
            |_, session_id| {
                requests
                    .iter()
                    .find(|(candidate, _)| *candidate == session_id)
                    .and_then(|(_, transcript)| transcript.clone())
            },
            transcript_cwd,
            |cwd| Path::new(cwd).is_dir(),
        )
        .unwrap();

        assert!(!results[0].found());
        for result in &results[1..4] {
            assert!(result.found());
            assert_eq!(result.cwd(), None);
        }
        assert!(results[4].found());
        assert_eq!(results[4].cwd(), valid_cwd.to_str());
        assert!(results.iter().all(|result| {
            result
                .cwd
                .as_ref()
                .is_none_or(|cwd| cwd.len() <= MAX_PATH_BYTES)
        }));

        let request_debug = format!("{:?}", probes[4]);
        let result_debug = format!("{:?}", results[4]);
        assert!(request_debug.contains("REDACTED"));
        assert!(!request_debug.contains("valid"));
        assert!(!result_debug.contains(valid_cwd.to_str().unwrap()));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resume_probe_rejects_invalid_identifier_without_retaining_spare_capacity() {
        assert_eq!(
            ResumeTranscriptProbe::try_new(AgentKind::Claude, "../secret".to_owned()).unwrap_err(),
            ResumeTranscriptProbeError::InvalidRequest
        );
        let mut session_id = String::with_capacity(1024 * 1024);
        session_id.push_str("session-id");
        let probe = ResumeTranscriptProbe::try_new(AgentKind::Claude, session_id).unwrap();
        assert_eq!(
            probe.retained_bytes(),
            std::mem::size_of::<ResumeTranscriptProbe>() + "session-id".len()
        );
    }

    #[test]
    fn recursive_scan_budget_is_operation_wide_exact_then_plus_one() {
        let root = temp_dir("scan-budget");
        std::fs::write(root.join("rollout.jsonl"), b"{}\n").unwrap();

        let mut exact_out = Vec::new();
        let mut exact_budget = ScanBudget {
            entries: MAX_SCAN_ENTRIES - 1,
        };
        assert!(collect_jsonl_bounded(
            &root,
            &mut exact_out,
            0,
            &mut exact_budget
        ));
        assert_eq!(exact_budget.entries, MAX_SCAN_ENTRIES);
        assert_eq!(exact_out.len(), 1);

        let mut plus_one_out = Vec::new();
        let mut plus_one_budget = ScanBudget {
            entries: MAX_SCAN_ENTRIES,
        };
        assert!(!collect_jsonl_bounded(
            &root,
            &mut plus_one_out,
            0,
            &mut plus_one_budget
        ));
        assert!(plus_one_out.is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn lsof_budget_accepts_exact_and_rejects_plus_one() {
        let mut budget = DetectionBudget {
            lsof_calls: MAX_LSOF_CALLS_PER_DETECTION - 1,
        };
        assert_eq!(budget.consume_lsof(), Some(()));
        assert_eq!(budget.consume_lsof(), None);
    }

    #[cfg(unix)]
    #[test]
    fn bounded_command_timeout_kills_descendant_and_joins_readers() {
        let _guard = COMMAND_TEST_LOCK.lock().unwrap();
        let started = Instant::now();
        let result = run_command_bounded_with_timeout(
            Path::new("/bin/sh"),
            &["-c", "sleep 5"],
            64,
            Duration::from_millis(50),
        );
        assert_eq!(result, Err(CommandError::TimedOut));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(ACTIVE_COMMAND_READERS.load(Ordering::Acquire), 0);
    }

    #[cfg(unix)]
    #[test]
    fn clean_parent_with_descendant_pipe_does_not_block() {
        let _guard = COMMAND_TEST_LOCK.lock().unwrap();
        let started = Instant::now();
        let result = run_command_bounded_with_timeout(
            Path::new("/bin/sh"),
            &["-c", "(sleep 5) & printf ok"],
            64,
            Duration::from_secs(1),
        );
        assert_eq!(result.as_deref(), Ok("ok"));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(ACTIVE_COMMAND_READERS.load(Ordering::Acquire), 0);
    }

    #[cfg(unix)]
    #[test]
    fn repeated_bounded_commands_leave_no_reader_growth() {
        let _guard = COMMAND_TEST_LOCK.lock().unwrap();
        for _ in 0..16 {
            assert_eq!(
                run_command_bounded_with_timeout(
                    Path::new("/usr/bin/printf"),
                    &["ok"],
                    2,
                    Duration::from_secs(1)
                )
                .as_deref(),
                Ok("ok")
            );
            assert_eq!(ACTIVE_COMMAND_READERS.load(Ordering::Acquire), 0);
        }
    }

    #[cfg(unix)]
    #[test]
    fn command_output_plus_one_fails_and_joins_readers() {
        let _guard = COMMAND_TEST_LOCK.lock().unwrap();
        let exact = "x".repeat(64);
        assert_eq!(
            run_command_bounded_with_timeout(
                Path::new("/usr/bin/printf"),
                &[&exact],
                64,
                Duration::from_secs(1)
            )
            .as_deref(),
            Ok(exact.as_str())
        );
        let plus_one = "x".repeat(65);
        assert_eq!(
            run_command_bounded_with_timeout(
                Path::new("/usr/bin/printf"),
                &[&plus_one],
                64,
                Duration::from_secs(1)
            ),
            Err(CommandError::OutputTooLarge)
        );
        assert_eq!(ACTIVE_COMMAND_READERS.load(Ordering::Acquire), 0);
    }

    #[test]
    fn production_source_has_no_unbounded_capture_or_raw_diagnostics() {
        let production = include_str!("agent_detect.rs")
            .split("#[cfg(test)]\nmod tests")
            .next()
            .unwrap();
        for forbidden in [
            ".output()",
            "read_to_end",
            "String::from_utf8_lossy",
            "tracing::",
            "eprintln!",
            "println!",
        ] {
            assert!(!production.contains(forbidden), "found {forbidden}");
        }
        assert_eq!(production.matches("Command::new").count(), 1);
        assert_eq!(
            production
                .matches("let mut finder = TranscriptFinder::new();")
                .count(),
            1
        );
        assert!(production.contains("libc::WNOWAIT"));
        assert!(production.contains("process_group(0)"));
    }

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

    /// Kimi는 런처가 `kimi`로 띄우는데 **실제 워커 프로세스명은 `kimi-code`**다
    /// (2026-08-09 실측: pid 21295 `kimi`와 pid 27534 `kimi-code`가 함께 떠 있었다).
    /// 하나만 보면 세션을 놓쳐 카드가 회색 셸로 강등된다.
    #[test]
    fn classify_kimi는_런처명과_워커명을_모두_잡는다() {
        let (kind, sid) = classify("/Users/jr/.kimi-code/bin/kimi --yolo --model k3").unwrap();
        assert_eq!(kind, AgentKind::Kimi);
        assert_eq!(sid, None, "Kimi argv에서 세션 id를 뽑는 근거는 아직 없다");

        let (worker, _) = classify("kimi-code").unwrap();
        assert_eq!(worker, AgentKind::Kimi, "워커 프로세스명을 놓치면 안 된다");
    }

    /// Kimi 바인딩은 **hook에서만** 온다. 프로세스 탐색(`bind`)은 만들지 않는다.
    ///
    /// Claude/Codex는 transcript 파일명·경로에서 세션을 역추적할 수 있지만, Kimi는
    /// 세션 id ↔ 디렉터리 대응이 `session_index.jsonl`에만 있어 프로세스만 보고는
    /// 어느 세션인지 알 수 없다. 추측해서 묶으면 **엉뚱한 세션의 상태**를 보여준다.
    ///
    /// (이 테스트는 원래 "파서가 없으니 상태도 만들지 않는다"를 고정했는데, 파서가
    /// 생기면서 그 계약은 사라졌다. 지금 지키는 것은 바인딩 출처 쪽이다.)
    #[test]
    fn kimi_바인딩은_프로세스_탐색이_아니라_hook에서만_온다() {
        assert_eq!(kind_from_str("kimi"), Some(AgentKind::Kimi));
        let mut budget = DetectionBudget::default();
        assert!(
            bind_transcript(AgentKind::Kimi, None, 1, &mut budget).is_none(),
            "프로세스만 보고 세션을 추측해 묶으면 엉뚱한 상태를 보여준다"
        );
        // 루트 밖 경로는 hook이 줬더라도 무효다 — 임의 파일을 transcript로 읽지 않는다.
        let Some(home) = crate::paths::home_dir() else {
            return;
        };
        assert!(!valid_transcript_path(
            AgentKind::Kimi,
            &home.join("somewhere-else/wire.jsonl")
        ));
    }

    #[test]
    fn classify_ignores_unrelated() {
        assert!(classify("/bin/zsh -l").is_none());
        assert!(classify("vim claude_notes.md").is_none()); // 인자 언급은 오탐 안 함
    }

    /// statusLine은 1시간 창으로 만료된다. 오래 유휴한 세션에서는 argv가 유일한
    /// 근거라, 여기서 값을 못 뽑으면 강도·모델 단축키가 "현재 값을 몰라" 조용히
    /// 아무것도 하지 않는다(2026-08-03 실증).
    #[test]
    fn argv에서_모델과_강도를_뽑는다() {
        // 사용자 환경에서 실제로 관측된 형태.
        let command =
            "/Users/jr/.local/bin/claude --settings /tmp/s.json --model opus[1m] --effort high";
        assert_eq!(
            argv_flag_value(command, "--model").as_deref(),
            Some("opus[1m]")
        );
        assert_eq!(
            argv_flag_value(command, "--effort").as_deref(),
            Some("high")
        );
        assert_eq!(argv_flag_value(command, "--missing"), None);

        // 값 없이 플래그만 있으면 다음 플래그를 값으로 삼으면 안 된다.
        assert_eq!(
            argv_flag_value("claude --effort --model opus", "--effort"),
            None
        );
        // 맨 끝이면 값이 없다.
        assert_eq!(argv_flag_value("claude --effort", "--effort"), None);
        // 부분 일치에 속지 않는다.
        assert_eq!(argv_flag_value("claude --effortless x", "--effort"), None);
    }

    /// 프로세스 스캔 결과에 argv 값이 함께 실려야 폴백이 성립한다.
    #[test]
    fn 프로세스_스캔이_모델과_강도를_함께_싣는다() {
        let rows = vec![
            ProcRow {
                pid: 100,
                ppid: Some(1),
                command: "/bin/zsh".into(),
            },
            ProcRow {
                pid: 101,
                ppid: Some(100),
                command: "claude --model opus[1m] --effort high".into(),
            },
        ];
        let found = agent_kinds_from_rows(&[(SessionId(1), 100)], &rows);
        let agent = found.get(&SessionId(1)).expect("에이전트를 찾아야 한다");
        assert_eq!(agent.kind, AgentKind::Claude);
        assert_eq!(agent.model.as_deref(), Some("opus[1m]"));
        assert_eq!(agent.effort.as_deref(), Some("high"));
    }

    /// transcript가 없어도 프로세스만으로 종류를 잡아야 한다.
    ///
    /// 실제 증상(2026-08-02): 에이전트를 띄우자마자 강도 단축키를 누르면 아무 일도
    /// 일어나지 않았다. 아직 대화를 시작하지 않아 transcript가 없었고, 그래서
    /// `bindings=0`이라 PTY 표면 자체가 만들어지지 않았다.
    #[test]
    fn transcript_없이도_프로세스로_종류를_잡는다() {
        let rows = vec![
            ProcRow {
                pid: 100,
                ppid: Some(1),
                command: "/bin/sh".into(),
            },
            // 실제로 사용자 환경에서 관측된 형태 — 셸의 자식으로 뜬다.
            ProcRow {
                pid: 101,
                ppid: Some(100),
                command: "/Users/jr/.local/bin/codex --enable hooks".into(),
            },
            ProcRow {
                pid: 200,
                ppid: Some(1),
                command: "/bin/zsh".into(),
            },
            ProcRow {
                pid: 201,
                ppid: Some(200),
                command: "claude".into(),
            },
            // 에이전트가 없는 셸은 목록에 없어야 한다.
            ProcRow {
                pid: 300,
                ppid: Some(1),
                command: "/bin/zsh".into(),
            },
        ];
        let sessions = [
            (SessionId(1), 100),
            (SessionId(2), 200),
            (SessionId(3), 300),
        ];
        let kinds = agent_kinds_from_rows(&sessions, &rows);

        assert_eq!(
            kinds.get(&SessionId(1)).map(|a| a.kind),
            Some(AgentKind::Codex)
        );
        assert_eq!(
            kinds.get(&SessionId(2)).map(|a| a.kind),
            Some(AgentKind::Claude)
        );
        assert_eq!(kinds.get(&SessionId(3)), None);
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
    fn hook_owner는_저장된_에이전트_종류와_같아야_한다() {
        let rows = vec![
            ProcRow {
                pid: 100,
                ppid: Some(1),
                command: "zsh".into(),
            },
            ProcRow {
                pid: 200,
                ppid: Some(100),
                command: "/Users/jr/.local/bin/claude --session-id current-claude".into(),
            },
        ];

        assert_eq!(find_agent_pid(100, &rows, AgentKind::Claude), Some(200));
        assert_eq!(find_agent_pid(100, &rows, AgentKind::Codex), None);
    }

    #[test]
    fn 캐시_owner의_공급자가_바뀌면_재사용하지_않는다() {
        let rows = vec![ProcRow {
            pid: 200,
            ppid: Some(100),
            command: "/Users/jr/.local/bin/claude --session-id current-claude".into(),
        }];

        assert!(agent_pid_matches_kind(200, AgentKind::Claude, &rows));
        assert!(!agent_pid_matches_kind(200, AgentKind::Codex, &rows));
        assert!(!agent_pid_matches_kind(999, AgentKind::Claude, &rows));
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
        let mut budget = DetectionBudget::default();
        println!("ps rows: {}", rows.len());
        let mut found = 0;
        for row in &rows {
            if let Some((kind, sid)) = classify(&row.command)
                && let Some((b, det)) = bind_transcript(kind, sid, row.pid, &mut budget)
            {
                println!("  {:?} det={det}", b.kind);
                found += 1;
            }
        }
        println!("바인딩 성공: {found}");
    }
}
