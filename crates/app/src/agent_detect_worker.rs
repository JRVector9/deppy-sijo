//! agent_detect의 무거운 I/O(ps/lsof/transcript 스캔)를 UI 스레드 밖 전용 스레드에서
//! 돌린다(codex #3: UI hitch 제거). 입력은 generation이 붙은 최신 스냅샷 하나,
//! 결과는 기다리는 소비자가 없어도 누적되지 않는 capacity-one 교체 슬롯 하나만
//! 유지한다. 빈 세션 입력에서는 condvar에 무기한 park하여 subprocess/파일 I/O와
//! repaint를 모두 발생시키지 않는다. epoch+generation 검증으로 입력 변경 중 완료된
//! stale 연산은 publish 전에 폐기한다.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};

use runtime::SessionId;

use crate::agent_detect::{self, AgentBinding, AgentDisplay, RunningAgent};
use crate::agent_transcript::{self, AgentActivity, MAX_RECENT_TRANSCRIPT_TURNS, TranscriptTurn};

const BINDING_INTERVAL: Duration = Duration::from_millis(2500);
const ACTIVITY_INTERVAL: Duration = Duration::from_millis(1500);
/// 종류만 보는 싼 tier의 주기. `ps` 한 번뿐이라 바인딩 tier보다 자주 돌 수 있다.
const KINDS_INTERVAL: Duration = Duration::from_millis(1200);
/// 창이 숨겨졌을 때(가림/최소화, render_active=false) 두 tier의 폴링 완화 배수.
/// ps/lsof/transcript 스캔은 pane 배지 표시용이라 안 보일 때 자주 돌 이유가 없다
/// (2026-07-14 가림 프로파일: 숨김 CPU의 최대 단일 항목이 detect의 ps 스폰이었다).
/// 알림은 runtime worker의 status detector(출력 regex) 경로라 영향 없다.
const HIDDEN_INTERVAL_MULT: u32 = 4;
/// runtime의 실제 pane 상한보다 넉넉하지만 유한한 방어선. 입력과 모든 결과
/// map에 동일하게 적용해 장기 실행에서 항목 수가 세션 수를 넘어 증가하지 않게 한다.
pub const MAX_DETECT_SESSIONS: usize = 256;
const WORKER_SPAWN_ERROR: &str = "agent_detect_worker_spawn_failed";

/// 숨김이면 주기를 늘린다 — 세 tier가 같은 규칙을 쓴다.
fn scaled_interval(base: Duration, hidden: bool) -> Duration {
    if hidden {
        base * HIDDEN_INTERVAL_MULT
    } else {
        base
    }
}

/// tier 주기 — 숨김이면 4배로 늘린다 (바인딩 2.5s→10s, 활동 1.5s→6s).
fn tier_intervals(hidden: bool) -> (Duration, Duration) {
    if hidden {
        (
            BINDING_INTERVAL * HIDDEN_INTERVAL_MULT,
            ACTIVITY_INTERVAL * HIDDEN_INTERVAL_MULT,
        )
    } else {
        (BINDING_INTERVAL, ACTIVITY_INTERVAL)
    }
}

type InputSnapshot = (
    u64,
    Vec<(SessionId, u32)>,
    HashMap<SessionId, AgentBinding>,
    bool,
);

struct VersionedInput {
    generation: u64,
    snapshot: InputSnapshot,
}

struct DetectInputState {
    current: Arc<VersionedInput>,
    stopping: bool,
}

struct DetectInputShared {
    state: Mutex<DetectInputState>,
    changed: Condvar,
}

/// 유저 경로/세션 ID를 노출하지 않는 정적 입력 거부 코드.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectInputError {
    SessionLimit,
    OverrideLimit,
    WorkerUnavailable,
}

/// App → 스레드 capacity-one 입력. 동일한 스냅샷은 매 frame publish해도
/// allocation/clone/wake를 발생시키지 않는다.
#[derive(Clone)]
pub struct DetectInput {
    shared: Arc<DetectInputShared>,
}

impl DetectInput {
    fn new() -> Self {
        Self {
            shared: Arc::new(DetectInputShared {
                state: Mutex::new(DetectInputState {
                    current: Arc::new(VersionedInput {
                        generation: 0,
                        snapshot: (0, Vec::new(), HashMap::new(), false),
                    }),
                    stopping: false,
                }),
                changed: Condvar::new(),
            }),
        }
    }

    /// 최신 입력을 교체한다. `overrides`는 기존 값과 다를 때만 clone된다.
    /// cap 초과 시 예전 입력을 계속 실행하지 않고 빈 스냅샷으로 fail-closed한다.
    pub fn publish(
        &self,
        epoch: u64,
        sessions: Vec<(SessionId, u32)>,
        overrides: &HashMap<SessionId, AgentBinding>,
        hidden: bool,
    ) -> Result<(), DetectInputError> {
        let limit_error = if sessions.len() > MAX_DETECT_SESSIONS {
            Some(DetectInputError::SessionLimit)
        } else if overrides.len() > MAX_DETECT_SESSIONS {
            Some(DetectInputError::OverrideLimit)
        } else {
            None
        };
        let mut state = self
            .shared
            .state
            .lock()
            .map_err(|_| DetectInputError::WorkerUnavailable)?;
        if state.stopping {
            return Err(DetectInputError::WorkerUnavailable);
        }
        let unchanged = if limit_error.is_some() {
            state.current.snapshot.0 == epoch
                && state.current.snapshot.1.is_empty()
                && state.current.snapshot.2.is_empty()
                && state.current.snapshot.3 == hidden
        } else {
            state.current.snapshot.0 == epoch
                && state.current.snapshot.1 == sessions
                && state.current.snapshot.2 == *overrides
                && state.current.snapshot.3 == hidden
        };
        if !unchanged {
            let next = if limit_error.is_some() {
                (epoch, Vec::new(), HashMap::new(), hidden)
            } else {
                (epoch, sessions, overrides.clone(), hidden)
            };
            state.current = Arc::new(VersionedInput {
                generation: state.current.generation.wrapping_add(1),
                snapshot: next,
            });
            self.shared.changed.notify_one();
        }
        match limit_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn current(&self) -> Option<Arc<VersionedInput>> {
        self.shared
            .state
            .lock()
            .ok()
            .filter(|state| !state.stopping)
            .map(|state| state.current.clone())
    }

    fn is_current(&self, generation: u64) -> bool {
        self.shared
            .state
            .lock()
            .is_ok_and(|state| !state.stopping && state.current.generation == generation)
    }

    fn wait_for_change(&self, generation: u64, timeout: Option<Duration>) -> bool {
        let Ok(state) = self.shared.state.lock() else {
            return false;
        };
        if state.stopping || state.current.generation != generation {
            return !state.stopping;
        }
        match timeout {
            Some(timeout) => self
                .shared
                .changed
                .wait_timeout_while(state, timeout, |state| {
                    !state.stopping && state.current.generation == generation
                })
                .is_ok_and(|(state, _)| !state.stopping),
            None => self
                .shared
                .changed
                .wait_while(state, |state| {
                    !state.stopping && state.current.generation == generation
                })
                .is_ok_and(|state| !state.stopping),
        }
    }

    fn stop(&self) {
        if let Ok(mut state) = self.shared.state.lock() {
            state.stopping = true;
            self.shared.changed.notify_all();
        }
    }
}

/// 스레드 → App 결과. bindings/info/work_turns는 바인딩 tier에서만 Some이고 activity는 매번.
#[derive(PartialEq, Eq)]
pub struct DetectOutcome {
    pub epoch: u64,
    /// 입력 snapshot generation. worker에서 publish 전 freshness를 검증한 값이다.
    pub generation: u64,
    pub bindings: Option<HashMap<SessionId, AgentBinding>>,
    pub activity: HashMap<SessionId, AgentActivity>,
    /// 세션별 현재 작업 폴더(바인딩 tier에서만, 한 번의 lsof). 행 폴더명 + 워크스페이스명.
    pub session_cwds: Option<HashMap<SessionId, String>>,
    /// 세션별 에이전트 표시 정보(model/effort/context) — 바인딩 tier에서만.
    pub agent_info: Option<HashMap<SessionId, AgentDisplay>>,
    /// 실제 사용자 지시 단위의 최근 transcript 턴 — 바인딩 tier에서만.
    pub work_turns: Option<HashMap<SessionId, Vec<TranscriptTurn>>>,
    /// transcript 없이 프로세스만으로 판정한 세션별 에이전트 종류. 방금 띄워 아직
    /// 대화를 시작하지 않은 에이전트는 `bindings`에 없으므로 이쪽으로 잡는다.
    pub agent_kinds: Option<HashMap<SessionId, RunningAgent>>,
}

struct MailboxState {
    latest: Option<Arc<DetectOutcome>>,
    sequence: u64,
    closed: bool,
    consumer_alive: bool,
}

struct OutcomeMailbox {
    state: Mutex<MailboxState>,
    available: Condvar,
}

enum PublishResult {
    Changed,
    Unchanged,
    Closed,
}

impl OutcomeMailbox {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(MailboxState {
                latest: None,
                sequence: 0,
                closed: false,
                consumer_alive: true,
            }),
            available: Condvar::new(),
        })
    }

    fn publish(&self, outcome: DetectOutcome) -> PublishResult {
        let Ok(mut state) = self.state.lock() else {
            return PublishResult::Closed;
        };
        if state.closed || !state.consumer_alive {
            return PublishResult::Closed;
        }
        let mut outcome = outcome;
        if let Some(current) = state.latest.as_ref() {
            if current.generation > outcome.generation {
                return PublishResult::Unchanged;
            }
            // 활동 tier의 partial 결과가 소비되지 않은 binding/cwd/info/work_turns를
            // 지우지 않도록 동일 snapshot에서는 완전한 스냅샷으로 merge한다.
            if current.epoch == outcome.epoch && current.generation == outcome.generation {
                let changed = current.activity != outcome.activity
                    || outcome
                        .bindings
                        .as_ref()
                        .is_some_and(|value| current.bindings.as_ref() != Some(value))
                    || outcome
                        .session_cwds
                        .as_ref()
                        .is_some_and(|value| current.session_cwds.as_ref() != Some(value))
                    || outcome
                        .agent_info
                        .as_ref()
                        .is_some_and(|value| current.agent_info.as_ref() != Some(value))
                    || outcome
                        .work_turns
                        .as_ref()
                        .is_some_and(|value| current.work_turns.as_ref() != Some(value))
                    // 종류 tier는 이것만 바꾼다 — 검사에 없으면 Unchanged로 버려진다.
                    || outcome
                        .agent_kinds
                        .as_ref()
                        .is_some_and(|value| current.agent_kinds.as_ref() != Some(value));
                if !changed {
                    return PublishResult::Unchanged;
                }
                if outcome.bindings.is_none() {
                    outcome.bindings.clone_from(&current.bindings);
                }
                if outcome.session_cwds.is_none() {
                    outcome.session_cwds.clone_from(&current.session_cwds);
                }
                if outcome.agent_info.is_none() {
                    outcome.agent_info.clone_from(&current.agent_info);
                }
                if outcome.work_turns.is_none() {
                    outcome.work_turns.clone_from(&current.work_turns);
                }
                if outcome.agent_kinds.is_none() {
                    outcome.agent_kinds.clone_from(&current.agent_kinds);
                }
            }
            if current.as_ref() == &outcome {
                return PublishResult::Unchanged;
            }
        }
        state.latest = Some(Arc::new(outcome));
        state.sequence = state.sequence.wrapping_add(1);
        self.available.notify_one();
        PublishResult::Changed
    }

    fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.closed = true;
            self.available.notify_all();
        }
    }
}

/// capacity-one 결과 수신기. `try_recv`는 std mpsc receiver와 동일한 폴링
/// 형태를 제공하지만 저장은 항상 최신 결과 하나뿐이다.
pub struct DetectOutcomeReceiver {
    mailbox: Arc<OutcomeMailbox>,
    seen_sequence: AtomicU64,
}

impl DetectOutcomeReceiver {
    pub fn try_recv(&self) -> Result<Arc<DetectOutcome>, std::sync::mpsc::TryRecvError> {
        let Ok(state) = self.mailbox.state.lock() else {
            return Err(std::sync::mpsc::TryRecvError::Disconnected);
        };
        if state.sequence != self.seen_sequence.load(Ordering::Relaxed) {
            self.seen_sequence.store(state.sequence, Ordering::Relaxed);
            Ok(Arc::clone(
                state
                    .latest
                    .as_ref()
                    .expect("agent_detect_mailbox_sequence_without_value"),
            ))
        } else if state.closed {
            Err(std::sync::mpsc::TryRecvError::Disconnected)
        } else {
            Err(std::sync::mpsc::TryRecvError::Empty)
        }
    }

    #[cfg(test)]
    fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Result<Arc<DetectOutcome>, std::sync::mpsc::RecvTimeoutError> {
        let Ok(state) = self.mailbox.state.lock() else {
            return Err(std::sync::mpsc::RecvTimeoutError::Disconnected);
        };
        let seen = self.seen_sequence.load(Ordering::Relaxed);
        let Ok((state, _)) = self
            .mailbox
            .available
            .wait_timeout_while(state, timeout, |state| {
                state.sequence == seen && !state.closed
            })
        else {
            return Err(std::sync::mpsc::RecvTimeoutError::Disconnected);
        };
        if state.sequence != seen {
            self.seen_sequence.store(state.sequence, Ordering::Relaxed);
            Ok(Arc::clone(
                state
                    .latest
                    .as_ref()
                    .expect("agent_detect_mailbox_sequence_without_value"),
            ))
        } else if state.closed {
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
        } else {
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        }
    }
}

impl Drop for DetectOutcomeReceiver {
    fn drop(&mut self) {
        if let Ok(mut state) = self.mailbox.state.lock() {
            state.consumer_alive = false;
            self.mailbox.available.notify_all();
        }
    }
}

pub struct AgentDetectWorker {
    input: DetectInput,
    handle: Option<std::thread::JoinHandle<()>>,
}

struct BindingPass {
    bindings: HashMap<SessionId, AgentBinding>,
    activity: HashMap<SessionId, AgentActivity>,
    session_cwds: HashMap<SessionId, String>,
    agent_info: HashMap<SessionId, AgentDisplay>,
    work_turns: Option<HashMap<SessionId, Vec<TranscriptTurn>>>,
    agent_kinds: HashMap<SessionId, RunningAgent>,
}

trait DetectionBackend: Send + 'static {
    fn reset(&mut self);

    fn binding_pass(
        &mut self,
        sessions: &[(SessionId, u32)],
        overrides: &HashMap<SessionId, AgentBinding>,
    ) -> BindingPass;

    fn activity_pass(
        &mut self,
        bindings: &HashMap<SessionId, AgentBinding>,
    ) -> HashMap<SessionId, AgentActivity>;

    /// 종류만 보는 싼 패스(`ps` 한 번). 기본 구현은 빈 결과라 테스트 backend는
    /// 구현하지 않아도 된다 — 종류 tier가 아무것도 바꾸지 않을 뿐이다.
    fn kinds_pass(&mut self, _sessions: &[(SessionId, u32)]) -> HashMap<SessionId, RunningAgent> {
        HashMap::new()
    }
}

#[derive(Default)]
struct ProductionBackend {
    cache: agent_detect::BindingCache,
    /// 바인딩 tier와 종류 tier가 공유하는 `ps` 스냅샷 캐시(짧은 TTL). 두 tier가 근접한
    /// 시각에 각자 exec하던 걸 흡수한다 — `ProcessRowsCache` 문서 참고(2026-08-14).
    process_rows: agent_detect::ProcessRowsCache,
    /// 바인딩 tier(`compute_activity_and_info`)와 활동 tier(`compute_activity`)가 공유하는
    /// transcript 파싱 캐시 — 두 tier가 같은 파일을 각자 다시 파싱하던 걸 없앤다(P1-A,
    /// 코드리뷰 2026-08-15). `TranscriptStateCache` 문서 참고.
    transcript_cache: TranscriptStateCache,
}

/// transcript 파싱 캐시 한 항목. `len`/`modified`는 hit 판정용 stat 스냅샷이다.
struct TranscriptCacheEntry {
    len: u64,
    modified: SystemTime,
    state: Arc<agent_transcript::TranscriptState>,
}

/// transcript 파싱을 `(len, modified)` 키로 메모이즈한다. `TranscriptState`는 세션ID·cwd·
/// transcript 발췌 등 민감 필드를 담고 있어 의도적으로 `Clone`이 아니다
/// (`agent_transcript.rs`의 `production_transcript_reads_have_bounded_source_laws`가 이를
/// 소스 레벨에서 강제한다) — 그래서 캐시는 파싱 결과를 `Arc`로 감싸 공유하고, hit 시
/// `Arc::clone`(refcount 증가)만 하지 구조체 전체를 복제하지 않는다.
///
/// 검증 키는 `std::fs::metadata` 한 번(stat 1회)의 `len()`+`modified()`다. transcript는
/// append-only JSONL이라 **len 변화가 주 신호**이고, mtime 해상도 문제(같은 초 안 재작성)는
/// append만 하는 파일에서 len이 함께 늘어나므로 실질 위험이 없다. stat이 실패하면(파일
/// 삭제/교체) 캐시를 버리고 다시 파싱한다.
#[derive(Default)]
struct TranscriptStateCache {
    entries: HashMap<SessionId, TranscriptCacheEntry>,
}

impl TranscriptStateCache {
    /// stat이 캐시와 일치하면 이전 파싱 결과를 재사용하고(재파싱 없음), 아니면(또는 stat
    /// 실패면) `fetch`로 다시 파싱해 캐시를 갱신한다. `fetch`는 프로덕션에서 언제나
    /// `agent_detect::agent_state`이고, 테스트는 파싱 호출 횟수를 세는 클로저를 주입한다.
    fn get(
        &mut self,
        sid: SessionId,
        binding: &AgentBinding,
        fetch: impl FnOnce(&AgentBinding) -> Option<agent_transcript::TranscriptState>,
    ) -> Option<Arc<agent_transcript::TranscriptState>> {
        let stat = std::fs::metadata(&binding.transcript)
            .ok()
            .and_then(|m| m.modified().ok().map(|modified| (m.len(), modified)));
        if let Some((len, modified)) = stat
            && let Some(entry) = self.entries.get(&sid)
            && entry.len == len
            && entry.modified == modified
        {
            return Some(Arc::clone(&entry.state));
        }
        let state = Arc::new(fetch(binding)?);
        match stat {
            Some((len, modified)) => {
                self.entries.insert(
                    sid,
                    TranscriptCacheEntry {
                        len,
                        modified,
                        state: Arc::clone(&state),
                    },
                );
            }
            // stat이 실패한 채로 파싱만 성공한 경우(레이스) — 다음 hit 판정을 그르치지
            // 않도록 옛 항목이 있으면 버린다.
            None => {
                self.entries.remove(&sid);
            }
        }
        Some(state)
    }

    /// 바인딩이 사라진 세션의 캐시 항목을 정리한다 — 유계 유지(무제한 맵 금지). 바인딩
    /// tier가 돌 때마다(≤10초 주기) 호출되므로 항목 수는 현재 세션 수를 넘지 않는다.
    fn retain(&mut self, bindings: &HashMap<SessionId, AgentBinding>) {
        self.entries.retain(|sid, _| bindings.contains_key(sid));
    }
}

fn compute_activity(
    bindings: &HashMap<SessionId, AgentBinding>,
    transcripts: &mut TranscriptStateCache,
) -> HashMap<SessionId, AgentActivity> {
    bindings
        .iter()
        .filter_map(|(sid, b)| {
            transcripts
                .get(*sid, b, agent_detect::agent_state)
                .map(|state| (*sid, state.activity))
        })
        .collect()
}

type ActivityInfoAndWorkTurns = (
    HashMap<SessionId, AgentActivity>,
    HashMap<SessionId, AgentDisplay>,
    HashMap<SessionId, Vec<TranscriptTurn>>,
);

/// transcript 파싱 결과(캐시 hit 포함)에서 activity + 표시정보 + 최근 사용자 턴을 함께
/// 만든다. `transcripts`가 바인딩 tier·활동 tier 간 파싱을 공유하므로 파일이 안 변한
/// tick은 어느 쪽도 재파싱하지 않는다(P1-A).
fn compute_activity_and_info(
    bindings: &HashMap<SessionId, AgentBinding>,
    transcripts: &mut TranscriptStateCache,
) -> ActivityInfoAndWorkTurns {
    let mut activity = HashMap::new();
    let mut info = HashMap::new();
    let mut work_turns = HashMap::new();
    for (sid, b) in bindings {
        if let Some(state) = transcripts.get(*sid, b, agent_detect::agent_state) {
            activity.insert(*sid, state.activity);
            work_turns.insert(*sid, state.recent_turns.clone());
            info.insert(
                *sid,
                AgentDisplay {
                    kind: b.kind,
                    model: state.model.clone(),
                    effort: state.effort.clone(),
                    context_pct: state.context_pct,
                    last_agent_summary: state.last_agent_summary.clone(),
                    user_instruction: state.user_instruction.clone(),
                },
            );
        }
    }
    transcripts.retain(bindings);
    (activity, info, work_turns)
}

impl DetectionBackend for ProductionBackend {
    fn reset(&mut self) {
        self.cache = agent_detect::BindingCache::default();
    }

    fn binding_pass(
        &mut self,
        sessions: &[(SessionId, u32)],
        overrides: &HashMap<SessionId, AgentBinding>,
    ) -> BindingPass {
        let detected = agent_detect::detect_cached(
            sessions,
            overrides,
            &mut self.cache,
            &mut self.process_rows,
        );
        let agent_detect::DetectedAgents {
            bindings,
            kinds: agent_kinds,
        } = detected;
        let (activity, agent_info, work_turns) =
            compute_activity_and_info(&bindings, &mut self.transcript_cache);
        let pids: Vec<u32> = sessions.iter().map(|(_, pid)| *pid).collect();
        let cwd_by_pid = agent_detect::session_cwds(&pids);
        let session_cwds = sessions
            .iter()
            .filter_map(|(sid, pid)| cwd_by_pid.get(pid).map(|cwd| (*sid, cwd.clone())))
            .collect();
        BindingPass {
            bindings,
            activity,
            session_cwds,
            agent_info,
            work_turns: Some(work_turns),
            agent_kinds,
        }
    }

    fn kinds_pass(&mut self, sessions: &[(SessionId, u32)]) -> HashMap<SessionId, RunningAgent> {
        agent_detect::detect_kinds(sessions, &mut self.cache, &mut self.process_rows)
    }

    fn activity_pass(
        &mut self,
        bindings: &HashMap<SessionId, AgentBinding>,
    ) -> HashMap<SessionId, AgentActivity> {
        compute_activity(bindings, &mut self.transcript_cache)
    }
}

fn bound_pass(mut pass: BindingPass, sessions: &[(SessionId, u32)]) -> BindingPass {
    let admitted: HashSet<_> = sessions
        .iter()
        .take(MAX_DETECT_SESSIONS)
        .map(|(sid, _)| *sid)
        .collect();
    pass.bindings.retain(|sid, _| admitted.contains(sid));
    pass.activity.retain(|sid, _| admitted.contains(sid));
    pass.session_cwds.retain(|sid, _| admitted.contains(sid));
    pass.agent_info.retain(|sid, _| admitted.contains(sid));
    if let Some(work_turns) = pass.work_turns.as_mut() {
        work_turns.retain(|sid, turns| {
            admitted.contains(sid) && turns.len() <= MAX_RECENT_TRANSCRIPT_TURNS
        });
    }
    pass
}

fn bound_activity(
    mut activity: HashMap<SessionId, AgentActivity>,
    bindings: &HashMap<SessionId, AgentBinding>,
) -> HashMap<SessionId, AgentActivity> {
    activity.retain(|sid, _| bindings.contains_key(sid));
    activity
}

fn publish_outcome(mailbox: &OutcomeMailbox, ctx: &egui::Context, outcome: DetectOutcome) -> bool {
    match mailbox.publish(outcome) {
        PublishResult::Changed => {
            ctx.request_repaint();
            true
        }
        PublishResult::Unchanged => true,
        PublishResult::Closed => false,
    }
}

fn run_worker<B: DetectionBackend>(
    input: DetectInput,
    mailbox: Arc<OutcomeMailbox>,
    ctx: egui::Context,
    mut backend: B,
) {
    struct CloseMailbox(Arc<OutcomeMailbox>);
    impl Drop for CloseMailbox {
        fn drop(&mut self) {
            self.0.close();
        }
    }
    let _close = CloseMailbox(Arc::clone(&mailbox));
    let mut bindings: HashMap<SessionId, AgentBinding> = HashMap::new();
    let mut last_epoch = 0u64;
    let mut accepted_generation = None;
    let mut had_sessions = false;
    let mut last_binding = Instant::now();
    let mut last_activity = Instant::now();
    let mut last_kinds = Instant::now();
    // 종류 tier는 activity를 만들지 않는다. DetectOutcome.activity가 Option이 아니라
    // 직전 값을 실어 보내야 활동 정보가 지워지지 않는다.
    let mut current_activity: HashMap<SessionId, AgentActivity> = HashMap::new();

    while let Some(versioned) = input.current() {
        let generation = versioned.generation;
        let (epoch, sessions, overrides, hidden) = &versioned.snapshot;

        if sessions.is_empty() {
            if had_sessions {
                backend.reset();
                bindings.clear();
                had_sessions = false;
                accepted_generation = Some(generation);
                if input.is_current(generation)
                    && !publish_outcome(
                        &mailbox,
                        &ctx,
                        DetectOutcome {
                            epoch: *epoch,
                            generation,
                            bindings: Some(HashMap::new()),
                            activity: HashMap::new(),
                            session_cwds: Some(HashMap::new()),
                            agent_info: Some(HashMap::new()),
                            work_turns: Some(HashMap::new()),
                            agent_kinds: Some(HashMap::new()),
                        },
                    )
                {
                    break;
                }
            }
            // 빈 입력은 timeout이 없다. publish/stop만 이 condvar를 깨운다.
            if !input.wait_for_change(generation, None) {
                break;
            }
            continue;
        }

        had_sessions = true;
        if *epoch != last_epoch {
            last_epoch = *epoch;
            backend.reset();
            bindings.clear();
            accepted_generation = None;
        }
        let (binding_interval, activity_interval) = tier_intervals(*hidden);
        let kinds_interval = scaled_interval(KINDS_INTERVAL, *hidden);
        let input_changed = accepted_generation != Some(generation);
        let binding_due = input_changed || last_binding.elapsed() >= binding_interval;
        let activity_due = !bindings.is_empty() && last_activity.elapsed() >= activity_interval;
        let kinds_due = last_kinds.elapsed() >= kinds_interval;

        if binding_due {
            let pass = bound_pass(backend.binding_pass(sessions, overrides), sessions);
            if !input.is_current(generation) {
                // stale 연산이 cache를 오염시켰을 수 있으므로 다음 generation에서 재구성.
                backend.reset();
                accepted_generation = None;
                continue;
            }
            last_binding = Instant::now();
            last_activity = last_binding;
            last_kinds = last_binding;
            accepted_generation = Some(generation);
            bindings = pass.bindings;
            current_activity.clone_from(&pass.activity);
            if !publish_outcome(
                &mailbox,
                &ctx,
                DetectOutcome {
                    epoch: *epoch,
                    generation,
                    bindings: Some(bindings.clone()),
                    activity: pass.activity,
                    session_cwds: Some(pass.session_cwds),
                    agent_info: Some(pass.agent_info),
                    work_turns: pass.work_turns,
                    agent_kinds: Some(pass.agent_kinds),
                },
            ) {
                break;
            }
            continue;
        }

        if activity_due {
            let activity = bound_activity(backend.activity_pass(&bindings), &bindings);
            if !input.is_current(generation) {
                continue;
            }
            last_activity = Instant::now();
            current_activity.clone_from(&activity);
            if !publish_outcome(
                &mailbox,
                &ctx,
                DetectOutcome {
                    epoch: *epoch,
                    generation,
                    bindings: None,
                    activity,
                    session_cwds: None,
                    agent_info: None,
                    work_turns: None,
                    agent_kinds: None,
                },
            ) {
                break;
            }
            continue;
        }

        if kinds_due {
            // `ps` 한 번뿐이다. 빈 터미널에서 손으로 띄운 에이전트는 세션 목록이
            // 안 바뀌어 즉시 트리거가 없고, 바인딩 없는 pane은 활동 tier도 안 돈다 —
            // 그래서 이 tier가 없으면 바인딩 주기를 통째로 기다린다.
            let kinds = backend.kinds_pass(sessions);
            if !input.is_current(generation) {
                continue;
            }
            last_kinds = Instant::now();
            if !publish_outcome(
                &mailbox,
                &ctx,
                DetectOutcome {
                    epoch: *epoch,
                    generation,
                    bindings: None,
                    // activity는 Option이 아니라, 직전 값을 실어야 지워지지 않는다.
                    activity: current_activity.clone(),
                    session_cwds: None,
                    agent_info: None,
                    work_turns: None,
                    agent_kinds: Some(kinds),
                },
            ) {
                break;
            }
            continue;
        }

        let binding_wait = binding_interval.saturating_sub(last_binding.elapsed());
        let activity_wait = if bindings.is_empty() {
            binding_wait
        } else {
            activity_interval.saturating_sub(last_activity.elapsed())
        };
        let kinds_wait = kinds_interval.saturating_sub(last_kinds.elapsed());
        if !input.wait_for_change(
            generation,
            Some(binding_wait.min(activity_wait).min(kinds_wait)),
        ) {
            break;
        }
    }
}

impl AgentDetectWorker {
    /// 전용 스레드를 띄운다. 입력·결과는 모두 latest-only이며 누적 큐가 없다.
    pub fn spawn(ctx: egui::Context) -> (Self, DetectInput, DetectOutcomeReceiver) {
        Self::spawn_with_backend(ctx, ProductionBackend::default())
    }

    fn spawn_with_backend<B: DetectionBackend>(
        ctx: egui::Context,
        backend: B,
    ) -> (Self, DetectInput, DetectOutcomeReceiver) {
        let input = DetectInput::new();
        let mailbox = OutcomeMailbox::new();
        let worker_input = input.clone();
        let worker_mailbox = Arc::clone(&mailbox);
        let handle = std::thread::Builder::new()
            .name("agent-detect".to_owned())
            .spawn(move || run_worker(worker_input, worker_mailbox, ctx, backend))
            .expect(WORKER_SPAWN_ERROR);
        (
            Self {
                input: input.clone(),
                handle: Some(handle),
            },
            input,
            DetectOutcomeReceiver {
                mailbox,
                seen_sequence: AtomicU64::new(0),
            },
        )
    }
}

impl Drop for AgentDetectWorker {
    fn drop(&mut self) {
        self.input.stop();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::mpsc;

    use crate::agent_detect::AgentKind;

    fn binding(session: SessionId) -> AgentBinding {
        AgentBinding {
            kind: AgentKind::Codex,
            session_id: format!("session-{}", session.0),
            transcript: PathBuf::from("fixture.jsonl"),
        }
    }

    fn work_turn(index: usize) -> TranscriptTurn {
        TranscriptTurn {
            turn_key: format!("codex:{index:x}"),
            source_offset: index as u64,
            instruction: format!("task {index}"),
            agent_summary: None,
            occurred_at: None,
            activity: AgentActivity::Working,
        }
    }

    fn transcript_cache_temp_dir(label: &str) -> PathBuf {
        static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "deppy-agent-detect-worker-{label}-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, AtomicOrdering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn stub_transcript_state(session_id: &str) -> agent_transcript::TranscriptState {
        agent_transcript::TranscriptState {
            session_id: session_id.to_owned(),
            cwd: None,
            activity: AgentActivity::Idle,
            model: None,
            effort: None,
            context_pct: None,
            last_agent_summary: None,
            user_instruction: None,
            recent_turns: Vec::new(),
        }
    }

    /// `fetch`가 `FnOnce`라 매 호출마다 새 클로저가 필요하다 — 파싱 호출 카운터(`calls`)를
    /// 공유하는 클로저를 그때그때 만든다.
    fn counting_fetch(
        calls: Arc<AtomicUsize>,
    ) -> impl FnOnce(&AgentBinding) -> Option<agent_transcript::TranscriptState> {
        move |_: &AgentBinding| {
            calls.fetch_add(1, AtomicOrdering::SeqCst);
            Some(stub_transcript_state("s"))
        }
    }

    /// P1-A: 파일(len+mtime)이 안 바뀐 tick은 재파싱하지 않는다 — 바인딩 tier와 활동
    /// tier가 같은 transcript를 각자 다시 파싱하던 걸 없앤 것이 이 캐시의 핵심 효과다.
    #[test]
    fn transcript_cache는_파일이_안_바뀌면_재파싱을_건너뛴다() {
        let dir = transcript_cache_temp_dir("unchanged");
        let path = dir.join("t.jsonl");
        std::fs::write(&path, b"line-1\n").unwrap();
        let sid = SessionId(1);
        let binding = AgentBinding {
            kind: AgentKind::Codex,
            session_id: "s".to_owned(),
            transcript: path,
        };
        let mut cache = TranscriptStateCache::default();
        let calls = Arc::new(AtomicUsize::new(0));

        let first = cache.get(sid, &binding, counting_fetch(Arc::clone(&calls)));
        assert!(first.is_some());
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 1);

        let second = cache.get(sid, &binding, counting_fetch(Arc::clone(&calls)));
        assert!(second.is_some());
        assert_eq!(
            calls.load(AtomicOrdering::SeqCst),
            1,
            "파일이 안 바뀌었으면 두 번째 호출은 재파싱하면 안 된다"
        );

        std::fs::remove_dir_all(dir).unwrap();
    }

    /// P1-A: append로 len이 늘면 즉시 재파싱한다(append-only JSONL 신호는 len).
    #[test]
    fn transcript_cache는_append하면_즉시_재파싱한다() {
        let dir = transcript_cache_temp_dir("appended");
        let path = dir.join("t.jsonl");
        std::fs::write(&path, b"line-1\n").unwrap();
        let sid = SessionId(1);
        let binding = AgentBinding {
            kind: AgentKind::Codex,
            session_id: "s".to_owned(),
            transcript: path.clone(),
        };
        let mut cache = TranscriptStateCache::default();
        let calls = Arc::new(AtomicUsize::new(0));

        let _ = cache.get(sid, &binding, counting_fetch(Arc::clone(&calls)));
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 1);

        std::fs::write(&path, b"line-1\nline-2\n").unwrap();
        let _ = cache.get(sid, &binding, counting_fetch(Arc::clone(&calls)));
        assert_eq!(
            calls.load(AtomicOrdering::SeqCst),
            2,
            "append 후에는 다음 호출이 즉시 재파싱해야 한다"
        );

        std::fs::remove_dir_all(dir).unwrap();
    }

    /// P1-A: 바인딩이 사라진(pane 종료 등) 세션의 캐시 항목은 `retain`으로 정리된다 —
    /// 유계 유지(무제한 맵 금지).
    #[test]
    fn transcript_cache_retain은_사라진_바인딩의_항목을_정리한다() {
        let dir = transcript_cache_temp_dir("retain");
        let path = dir.join("t.jsonl");
        std::fs::write(&path, b"line-1\n").unwrap();
        let sid = SessionId(1);
        let binding = AgentBinding {
            kind: AgentKind::Codex,
            session_id: "s".to_owned(),
            transcript: path,
        };
        let mut cache = TranscriptStateCache::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let _ = cache.get(sid, &binding, counting_fetch(Arc::clone(&calls)));
        assert!(cache.entries.contains_key(&sid));

        cache.retain(&HashMap::new());

        assert!(
            !cache.entries.contains_key(&sid),
            "사라진 바인딩의 캐시 항목이 정리되지 않았다"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn complete_outcome(epoch: u64, generation: u64, state: AgentActivity) -> DetectOutcome {
        let session = SessionId(1);
        DetectOutcome {
            epoch,
            generation,
            bindings: Some(HashMap::from([(session, binding(session))])),
            activity: HashMap::from([(session, state)]),
            session_cwds: Some(HashMap::from([(session, "/fixture".to_owned())])),
            agent_kinds: Some(HashMap::from([(
                session,
                RunningAgent {
                    kind: AgentKind::Claude,
                    model: None,
                    effort: None,
                },
            )])),
            agent_info: Some(HashMap::from([(
                session,
                AgentDisplay {
                    kind: AgentKind::Codex,
                    model: Some("fixture-model".to_owned()),
                    effort: None,
                    context_pct: Some(50),
                    last_agent_summary: None,
                    user_instruction: None,
                },
            )])),
            work_turns: Some(HashMap::from([(session, vec![work_turn(1)])])),
        }
    }

    fn receiver(mailbox: Arc<OutcomeMailbox>) -> DetectOutcomeReceiver {
        DetectOutcomeReceiver {
            mailbox,
            seen_sequence: AtomicU64::new(0),
        }
    }

    struct TestBackend {
        calls: Arc<AtomicUsize>,
        started: Option<mpsc::Sender<()>>,
        first_release: Option<Arc<(Mutex<bool>, Condvar)>>,
    }

    impl DetectionBackend for TestBackend {
        fn reset(&mut self) {}

        fn binding_pass(
            &mut self,
            sessions: &[(SessionId, u32)],
            _overrides: &HashMap<SessionId, AgentBinding>,
        ) -> BindingPass {
            let call = self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            if call == 0 {
                if let Some(started) = self.started.take() {
                    let _ = started.send(());
                }
                if let Some(release) = self.first_release.take() {
                    let (lock, changed) = &*release;
                    let mut released = lock.lock().expect("test_release_lock");
                    while !*released {
                        released = changed.wait(released).expect("test_release_wait");
                    }
                }
            }
            let bindings: HashMap<_, _> = sessions
                .iter()
                .map(|(session, _)| (*session, binding(*session)))
                .collect();
            BindingPass {
                agent_kinds: bindings
                    .keys()
                    .map(|session| {
                        (
                            *session,
                            RunningAgent {
                                kind: AgentKind::Claude,
                                model: None,
                                effort: None,
                            },
                        )
                    })
                    .collect(),
                activity: bindings
                    .keys()
                    .map(|session| (*session, AgentActivity::Idle))
                    .collect(),
                session_cwds: bindings
                    .keys()
                    .map(|session| (*session, "/fixture".to_owned()))
                    .collect(),
                agent_info: HashMap::new(),
                work_turns: Some(
                    bindings
                        .keys()
                        .map(|session| (*session, vec![work_turn(session.0 as usize)]))
                        .collect(),
                ),
                bindings,
            }
        }

        fn activity_pass(
            &mut self,
            bindings: &HashMap<SessionId, AgentBinding>,
        ) -> HashMap<SessionId, AgentActivity> {
            bindings
                .keys()
                .map(|session| (*session, AgentActivity::Idle))
                .collect()
        }
    }

    /// 빈 터미널에서 손으로 띄운 에이전트는 세션 목록이 그대로라 즉시 트리거가 없고,
    /// 바인딩이 없는 pane은 활동 tier도 안 돈다. 종류 tier가 그 공백을 메우므로
    /// **바인딩 주기보다 짧아야** 의미가 있다(2026-08-09 신고).
    #[test]
    fn 종류_tier는_바인딩보다_자주_돌고_숨김_규칙을_공유한다() {
        assert!(
            KINDS_INTERVAL < BINDING_INTERVAL,
            "바인딩보다 느리면 종류 tier를 둔 이유가 없다"
        );
        assert_eq!(scaled_interval(KINDS_INTERVAL, false), KINDS_INTERVAL);
        assert_eq!(
            scaled_interval(KINDS_INTERVAL, true),
            KINDS_INTERVAL * HIDDEN_INTERVAL_MULT,
            "숨김 완화를 안 따르면 가려진 창에서만 폴링이 상대적으로 늘어난다"
        );
    }

    /// 종류 tier는 `agent_kinds`만 바꾼다. publish의 변경 검사에 그 필드가 없으면
    /// **Unchanged로 버려져** 아무 일도 일어나지 않는다 — 실제로 이 필드만 빠져 있었다.
    /// 그리고 부분 결과가 기존 bindings/agent_info를 지워서도 안 된다.
    #[test]
    fn 종류만_바뀐_결과도_소비자에게_전달되고_기존값을_지우지_않는다() {
        let mailbox = OutcomeMailbox::new();
        let receiver = receiver(Arc::clone(&mailbox));
        assert!(!matches!(
            mailbox.publish(complete_outcome(1, 7, AgentActivity::Idle)),
            PublishResult::Closed
        ));
        let _ = receiver.try_recv();

        let kimi = HashMap::from([(
            SessionId(9),
            RunningAgent {
                kind: AgentKind::Kimi,
                model: Some("kimi-code/k3".to_owned()),
                effort: None,
            },
        )]);
        assert!(matches!(
            mailbox.publish(DetectOutcome {
                epoch: 1,
                generation: 7,
                bindings: None,
                activity: HashMap::from([(SessionId(1), AgentActivity::Idle)]),
                session_cwds: None,
                agent_info: None,
                work_turns: None,
                agent_kinds: Some(kimi.clone()),
            }),
            PublishResult::Changed
        ));

        let latest = receiver.try_recv().expect("종류만 바뀐 결과도 와야 한다");
        assert_eq!(
            latest.agent_kinds.as_ref(),
            Some(&kimi),
            "버려지면 손으로 띄운 에이전트가 카드에 안 뜬다"
        );
        assert!(
            latest.bindings.is_some() && latest.agent_info.is_some() && latest.work_turns.is_some(),
            "부분 결과가 기존 binding payload를 지우면 카드가 도로 셸이 된다"
        );

        // 반대 방향 — 활동 tier는 agent_kinds를 None으로 보낸다. 이월하지 않으면
        // 아직 안 읽힌 종류가 지워져, 소비자가 카드를 셸로 되돌린다.
        assert!(matches!(
            mailbox.publish(DetectOutcome {
                epoch: 1,
                generation: 7,
                bindings: None,
                activity: HashMap::from([(SessionId(1), AgentActivity::Working)]),
                session_cwds: None,
                agent_info: None,
                work_turns: None,
                agent_kinds: None,
            }),
            PublishResult::Changed
        ));
        let after_activity = receiver.try_recv().expect("활동 갱신이 와야 한다");
        assert_eq!(
            after_activity.agent_kinds.as_ref(),
            Some(&kimi),
            "활동 tier의 부분 결과가 종류를 지우면 안 된다"
        );
    }

    #[test]
    fn 숨김이면_두_tier_주기가_4배로_늘고_보이면_원래대로다() {
        let (b, a) = tier_intervals(false);
        assert_eq!(b, BINDING_INTERVAL);
        assert_eq!(a, ACTIVITY_INTERVAL);

        let (b, a) = tier_intervals(true);
        assert_eq!(b, BINDING_INTERVAL * HIDDEN_INTERVAL_MULT);
        assert_eq!(a, ACTIVITY_INTERVAL * HIDDEN_INTERVAL_MULT);
        // 대기 슬라이스 상한(BINDING_INTERVAL)보다 길어야 슬라이스 분할이 의미 있다 —
        // 복귀 시 elapsed가 이미 정상 주기를 넘어 있어 즉시 따라잡는 전제.
        assert!(b > BINDING_INTERVAL);
    }

    #[test]
    fn 입력_세션_상한_정확히_256은_허용하고_257은_빈_스냅샷으로_거부한다() {
        let input = DetectInput::new();
        let exact: Vec<_> = (0..MAX_DETECT_SESSIONS)
            .map(|index| (SessionId(index as u64 + 1), index as u32 + 10))
            .collect();
        assert_eq!(
            input.publish(7, exact.clone(), &HashMap::new(), false),
            Ok(())
        );
        let accepted = input.current().expect("accepted input");
        assert_eq!(accepted.snapshot.1.len(), MAX_DETECT_SESSIONS);

        let over: Vec<_> = (0..=MAX_DETECT_SESSIONS)
            .map(|index| (SessionId(index as u64 + 1), index as u32 + 10))
            .collect();
        assert_eq!(
            input.publish(7, over, &HashMap::new(), false),
            Err(DetectInputError::SessionLimit)
        );
        let rejected = input.current().expect("fail closed input");
        assert!(rejected.snapshot.1.is_empty());
        assert!(rejected.snapshot.2.is_empty());
    }

    #[test]
    fn 동일_입력은_generation과_arc를_교체하지_않는다() {
        let input = DetectInput::new();
        let sessions = vec![(SessionId(1), 10)];
        let overrides = HashMap::from([(SessionId(1), binding(SessionId(1)))]);
        input
            .publish(3, sessions.clone(), &overrides, false)
            .expect("first publish");
        let first = input.current().expect("first input");
        input
            .publish(3, sessions, &overrides, false)
            .expect("same publish");
        let second = input.current().expect("second input");
        assert_eq!(first.generation, second.generation);
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn 결과_슬롯은_cap_1을_넘으면_최신값만_남긴다() {
        let mailbox = OutcomeMailbox::new();
        let receiver = receiver(Arc::clone(&mailbox));
        for generation in 1..=32 {
            assert!(!matches!(
                mailbox.publish(complete_outcome(1, generation, AgentActivity::Idle)),
                PublishResult::Closed
            ));
        }
        let latest = receiver.try_recv().expect("latest outcome");
        assert_eq!(latest.generation, 32);
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
    }

    #[test]
    fn 활동_partial은_미소비_full_결과를_보존하고_변경이_없으면_wake하지_않는다() {
        let mailbox = OutcomeMailbox::new();
        let receiver = receiver(Arc::clone(&mailbox));
        assert!(matches!(
            mailbox.publish(complete_outcome(1, 1, AgentActivity::Idle)),
            PublishResult::Changed
        ));
        let partial = DetectOutcome {
            epoch: 1,
            generation: 1,
            bindings: None,
            activity: HashMap::from([(SessionId(1), AgentActivity::Working)]),
            session_cwds: None,
            agent_info: None,
            work_turns: None,
            agent_kinds: None,
        };
        assert!(matches!(mailbox.publish(partial), PublishResult::Changed));
        let merged = receiver.try_recv().expect("merged outcome");
        assert_eq!(merged.activity[&SessionId(1)], AgentActivity::Working);
        assert!(merged.bindings.is_some());
        assert!(merged.session_cwds.is_some());
        assert!(merged.agent_info.is_some());
        assert!(merged.work_turns.is_some());

        let unchanged = DetectOutcome {
            epoch: 1,
            generation: 1,
            bindings: None,
            activity: HashMap::from([(SessionId(1), AgentActivity::Working)]),
            session_cwds: None,
            agent_info: None,
            work_turns: None,
            agent_kinds: None,
        };
        assert!(matches!(
            mailbox.publish(unchanged),
            PublishResult::Unchanged
        ));
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
    }

    #[test]
    fn 새_generation의_partial은_이전_work_turns를_이월하지_않는다() {
        let mailbox = OutcomeMailbox::new();
        assert!(matches!(
            mailbox.publish(complete_outcome(1, 1, AgentActivity::Idle)),
            PublishResult::Changed
        ));

        let fresh = DetectOutcome {
            epoch: 2,
            generation: 2,
            bindings: None,
            activity: HashMap::new(),
            session_cwds: None,
            agent_info: None,
            work_turns: None,
            agent_kinds: None,
        };
        assert!(matches!(mailbox.publish(fresh), PublishResult::Changed));

        let state = mailbox.state.lock().expect("mailbox state");
        let latest = state.latest.as_ref().expect("latest outcome");
        assert_eq!(latest.epoch, 2);
        assert_eq!(latest.generation, 2);
        assert!(
            latest.work_turns.is_none(),
            "새 입력에 없는 transcript payload를 이전 generation에서 이월하면 안 된다"
        );
    }

    #[test]
    fn 빈_입력에서_backend을_한_번도_호출하지_않고_shutdown한다() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (worker, _input, _receiver) = AgentDetectWorker::spawn_with_backend(
            egui::Context::default(),
            TestBackend {
                calls: Arc::clone(&calls),
                started: None,
                first_release: None,
            },
        );
        drop(worker);
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 0);
    }

    #[test]
    fn 소비자가_멈춰도_worker_shutdown은_결과_전송에_막히지_않는다() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (started_tx, started_rx) = mpsc::channel();
        let (worker, input, receiver) = AgentDetectWorker::spawn_with_backend(
            egui::Context::default(),
            TestBackend {
                calls: Arc::clone(&calls),
                started: Some(started_tx),
                first_release: None,
            },
        );
        input
            .publish(1, vec![(SessionId(1), 10)], &HashMap::new(), false)
            .expect("publish session");
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("backend started");
        let state = receiver.mailbox.state.lock().expect("mailbox lock");
        let (state, _) = receiver
            .mailbox
            .available
            .wait_timeout_while(state, Duration::from_secs(1), |state| {
                state.latest.is_none()
            })
            .expect("mailbox wait");
        assert!(state.latest.is_some());
        drop(state);
        drop(worker);
        let outcome = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("retained outcome");
        assert_eq!(outcome.epoch, 1);
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 1);
    }

    #[test]
    fn 연산_중_바뀐_generation의_stale_결과는_폐기한다() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (started_tx, started_rx) = mpsc::channel();
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let (worker, input, receiver) = AgentDetectWorker::spawn_with_backend(
            egui::Context::default(),
            TestBackend {
                calls: Arc::clone(&calls),
                started: Some(started_tx),
                first_release: Some(Arc::clone(&release)),
            },
        );
        input
            .publish(1, vec![(SessionId(1), 10)], &HashMap::new(), false)
            .expect("first generation");
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("first pass started");
        input
            .publish(2, vec![(SessionId(2), 20)], &HashMap::new(), false)
            .expect("second generation");
        {
            let (lock, changed) = &*release;
            *lock.lock().expect("release lock") = true;
            changed.notify_one();
        }
        let outcome = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("fresh outcome");
        assert_eq!(outcome.epoch, 2);
        assert_eq!(outcome.generation, 2);
        assert!(
            outcome
                .bindings
                .as_ref()
                .is_some_and(|bindings| bindings.contains_key(&SessionId(2)))
        );
        assert!(outcome.work_turns.as_ref().is_some_and(|turns| {
            turns.contains_key(&SessionId(2)) && !turns.contains_key(&SessionId(1))
        }));
        assert!(calls.load(AtomicOrdering::SeqCst) >= 2);
        drop(worker);
    }

    #[test]
    fn backend이_cap_초과_map을_반환해도_입력_세션으로_제한한다() {
        let sessions: Vec<_> = (0..MAX_DETECT_SESSIONS)
            .map(|index| (SessionId(index as u64 + 1), index as u32 + 10))
            .collect();
        let extra = SessionId(MAX_DETECT_SESSIONS as u64 + 1);
        let mut bindings: HashMap<_, _> = sessions
            .iter()
            .map(|(session, _)| (*session, binding(*session)))
            .collect();
        bindings.insert(extra, binding(extra));
        let pass = bound_pass(
            BindingPass {
                agent_kinds: bindings
                    .keys()
                    .map(|session| {
                        (
                            *session,
                            RunningAgent {
                                kind: AgentKind::Claude,
                                model: None,
                                effort: None,
                            },
                        )
                    })
                    .collect(),
                activity: bindings
                    .keys()
                    .map(|session| (*session, AgentActivity::Idle))
                    .collect(),
                session_cwds: bindings
                    .keys()
                    .map(|session| (*session, "/fixture".to_owned()))
                    .collect(),
                agent_info: HashMap::new(),
                work_turns: Some(
                    bindings
                        .keys()
                        .map(|session| (*session, vec![work_turn(session.0 as usize)]))
                        .collect(),
                ),
                bindings,
            },
            &sessions,
        );
        assert_eq!(pass.bindings.len(), MAX_DETECT_SESSIONS);
        assert_eq!(pass.activity.len(), MAX_DETECT_SESSIONS);
        assert_eq!(pass.session_cwds.len(), MAX_DETECT_SESSIONS);
        let work_turns = pass.work_turns.as_ref().expect("work turns");
        assert_eq!(work_turns.len(), MAX_DETECT_SESSIONS);
        assert!(!work_turns.contains_key(&extra));
        assert!(!pass.bindings.contains_key(&extra));
    }

    #[test]
    fn work_turns는_세션당_24개까지_허용하고_초과_payload는_거부한다() {
        let session = SessionId(1);
        let sessions = vec![(session, 10)];
        let make_pass = |turns| BindingPass {
            bindings: HashMap::from([(session, binding(session))]),
            activity: HashMap::new(),
            session_cwds: HashMap::new(),
            agent_info: HashMap::new(),
            work_turns: Some(HashMap::from([(session, turns)])),
            agent_kinds: HashMap::new(),
        };
        let exact = (0..MAX_RECENT_TRANSCRIPT_TURNS).map(work_turn).collect();
        let exact = bound_pass(make_pass(exact), &sessions);
        assert_eq!(
            exact.work_turns.as_ref().expect("exact turns")[&session].len(),
            MAX_RECENT_TRANSCRIPT_TURNS
        );

        let over = (0..=MAX_RECENT_TRANSCRIPT_TURNS).map(work_turn).collect();
        let over = bound_pass(make_pass(over), &sessions);
        assert!(
            !over
                .work_turns
                .as_ref()
                .expect("bounded turns")
                .contains_key(&session),
            "초과 transcript payload 일부를 mailbox에 남기면 안 된다"
        );
    }

    #[test]
    fn production_source는_무제한_결과_채널과_동적_진단을_사용하지_않는다() {
        let source = include_str!("agent_detect_worker.rs");
        let production = source
            .split("\n#[cfg(test)]\nmod tests")
            .next()
            .expect("production source");
        assert!(!production.contains("mpsc::channel()"));
        assert!(!production.contains("send(DetectOutcome"));
        assert!(!production.contains("eprintln!"));
        assert!(!production.contains("tracing::"));
        assert!(production.contains(WORKER_SPAWN_ERROR));
    }
}
