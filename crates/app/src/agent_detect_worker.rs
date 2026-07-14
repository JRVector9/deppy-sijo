//! agent_detect의 무거운 I/O(ps/lsof/transcript 스캔)를 UI 스레드 밖 전용 스레드에서
//! 돌린다(codex #3: UI hitch 제거). 2-tier 타이머(바인딩 2.5s / 활동 1.5s)를 스레드가
//! 자체 관리하고, 결과를 mpsc로 앱에 던진 뒤 ctx.request_repaint로 깨운다. 입력(세션 pid
//! 목록 + epoch)은 Arc<Mutex>로 최신 스냅샷 하나만 공유한다(이벤트 큐 아님 — 항상 최신
//! 1개). epoch로 워크스페이스 전환 시 stale 결과를 폐기한다. 스레드/Drop 패턴은
//! ApprovalWatcher를 따른다.

use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use runtime::SessionId;

use crate::agent_detect::{self, AgentBinding, AgentDisplay};
use crate::agent_transcript::AgentActivity;

const BINDING_INTERVAL: Duration = Duration::from_millis(2500);
const ACTIVITY_INTERVAL: Duration = Duration::from_millis(1500);
/// 창이 숨겨졌을 때(가림/최소화, render_active=false) 두 tier의 폴링 완화 배수.
/// ps/lsof/transcript 스캔은 pane 배지 표시용이라 안 보일 때 자주 돌 이유가 없다
/// (2026-07-14 가림 프로파일: 숨김 CPU의 최대 단일 항목이 detect의 ps 스폰이었다).
/// 알림은 runtime worker의 status detector(출력 regex) 경로라 영향 없다.
const HIDDEN_INTERVAL_MULT: u32 = 4;

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

/// App → 스레드 입력: (epoch, 활성 세션 pid 목록, hook 오버라이드, 창 숨김 여부).
/// 최신 값 하나만 의미 있다.
pub type DetectInput = Arc<
    Mutex<(
        u64,
        Vec<(SessionId, u32)>,
        HashMap<SessionId, AgentBinding>,
        bool,
    )>,
>;

/// 스레드 → App 결과. bindings는 바인딩 tier에서만 Some(활동 tier는 None), activity는 매번.
pub struct DetectOutcome {
    pub epoch: u64,
    pub bindings: Option<HashMap<SessionId, AgentBinding>>,
    pub activity: HashMap<SessionId, AgentActivity>,
    /// 세션별 현재 작업 폴더(바인딩 tier에서만, 한 번의 lsof). 행 폴더명 + 워크스페이스명.
    pub session_cwds: Option<HashMap<SessionId, String>>,
    /// 세션별 에이전트 표시 정보(model/effort/context) — 바인딩 tier에서만.
    pub agent_info: Option<HashMap<SessionId, AgentDisplay>>,
}

pub struct AgentDetectWorker {
    stop_tx: Option<mpsc::Sender<()>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

fn compute_activity(
    bindings: &HashMap<SessionId, AgentBinding>,
) -> HashMap<SessionId, AgentActivity> {
    bindings
        .iter()
        .filter_map(|(sid, b)| agent_detect::activity(b).map(|a| (*sid, a)))
        .collect()
}

/// transcript를 세션당 **한 번만** 파싱해 activity 맵 + 표시정보(model/effort/context) 맵을
/// 함께 만든다(중복 파싱 방지). model/effort/context는 바인딩 tier에서만 필요.
fn compute_activity_and_info(
    bindings: &HashMap<SessionId, AgentBinding>,
) -> (
    HashMap<SessionId, AgentActivity>,
    HashMap<SessionId, AgentDisplay>,
) {
    let mut activity = HashMap::new();
    let mut info = HashMap::new();
    for (sid, b) in bindings {
        if let Some(state) = agent_detect::agent_state(b) {
            activity.insert(*sid, state.activity);
            info.insert(
                *sid,
                AgentDisplay {
                    kind: b.kind,
                    model: state.model,
                    effort: state.effort,
                    context_pct: state.context_pct,
                },
            );
        }
    }
    (activity, info)
}

impl AgentDetectWorker {
    /// 전용 스레드를 띄운다. 입력 핸들과 결과 수신 채널을 함께 돌려준다.
    pub fn spawn(ctx: egui::Context) -> (Self, DetectInput, mpsc::Receiver<DetectOutcome>) {
        let input: DetectInput = Arc::new(Mutex::new((0, Vec::new(), HashMap::new(), false)));
        let (out_tx, out_rx) = mpsc::channel();
        let (stop_tx, stop_rx) = mpsc::channel();
        let input2 = input.clone();
        let handle = std::thread::Builder::new()
            .name("agent-detect".to_owned())
            .spawn(move || {
                let mut last_binding = Instant::now();
                let mut last_activity = Instant::now();
                let mut bindings: HashMap<SessionId, AgentBinding> = HashMap::new();
                let mut cache = agent_detect::BindingCache::default();
                let mut last_epoch = 0u64;
                let mut hidden = false;
                loop {
                    // 다음 만기까지 잔다. 활동 tier는 바인딩이 있을 때만 (없으면 바인딩 tier만).
                    // 숨김이면 주기가 4배지만 대기 슬라이스는 BINDING_INTERVAL로 자른다 —
                    // 복귀(hidden=false) 후 최대 한 슬라이스 안에 elapsed가 정상 주기를
                    // 넘어 있으므로 즉시 따라잡는다 (별도 전이 처리 불필요).
                    let (binding_interval, activity_interval) = tier_intervals(hidden);
                    let binding_wait = binding_interval.saturating_sub(last_binding.elapsed());
                    let activity_wait = if bindings.is_empty() {
                        Duration::from_secs(3600)
                    } else {
                        activity_interval.saturating_sub(last_activity.elapsed())
                    };
                    let wait = binding_wait
                        .min(activity_wait)
                        .clamp(Duration::from_millis(50), BINDING_INTERVAL);
                    match stop_rx.recv_timeout(wait) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    let (epoch, sessions, overrides, now_hidden) =
                        input2.lock().unwrap().clone();
                    hidden = now_hidden;
                    let (binding_interval, activity_interval) = tier_intervals(hidden);
                    // 워크스페이스 전환(epoch 변경) 시 캐시를 비운다 — SessionId가 워커마다
                    // 1부터라 캐시가 다른 워크스페이스 세션과 충돌하는 것을 막는다.
                    if epoch != last_epoch {
                        last_epoch = epoch;
                        cache = agent_detect::BindingCache::default();
                        bindings.clear();
                    }
                    if last_binding.elapsed() >= binding_interval {
                        last_binding = Instant::now();
                        last_activity = Instant::now();
                        bindings = agent_detect::detect_cached(&sessions, &overrides, &mut cache);
                        // transcript 1-pass로 activity + 표시정보(model/effort/context).
                        let (activity, agent_info) = compute_activity_and_info(&bindings);
                        // 세션별 현재 작업 폴더 — 한 번의 lsof(행 폴더명 + 워크스페이스명).
                        let pids: Vec<u32> = sessions.iter().map(|(_, p)| *p).collect();
                        let cwd_by_pid = agent_detect::session_cwds(&pids);
                        let session_cwds = sessions
                            .iter()
                            .filter_map(|(sid, pid)| cwd_by_pid.get(pid).map(|c| (*sid, c.clone())))
                            .collect();
                        let sent = out_tx.send(DetectOutcome {
                            epoch,
                            bindings: Some(bindings.clone()),
                            activity,
                            session_cwds: Some(session_cwds),
                            agent_info: Some(agent_info),
                        });
                        if sent.is_err() {
                            break; // 수신측(App) drop → 종료
                        }
                        ctx.request_repaint();
                    } else if !bindings.is_empty() && last_activity.elapsed() >= activity_interval {
                        last_activity = Instant::now();
                        let activity = compute_activity(&bindings);
                        if out_tx
                            .send(DetectOutcome {
                                epoch,
                                bindings: None,
                                activity,
                                session_cwds: None,
                                agent_info: None,
                            })
                            .is_err()
                        {
                            break;
                        }
                        ctx.request_repaint();
                    }
                }
            })
            .expect("agent detect thread spawn");
        (
            Self {
                stop_tx: Some(stop_tx),
                handle: Some(handle),
            },
            input,
            out_rx,
        )
    }
}

impl Drop for AgentDetectWorker {
    fn drop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
