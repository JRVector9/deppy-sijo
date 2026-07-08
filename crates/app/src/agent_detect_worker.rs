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

use crate::agent_detect::{self, AgentBinding};
use crate::agent_transcript::AgentActivity;

const BINDING_INTERVAL: Duration = Duration::from_millis(2500);
const ACTIVITY_INTERVAL: Duration = Duration::from_millis(1500);

/// App → 스레드 입력: (epoch, 활성 세션 pid 목록, hook 오버라이드). 최신 값 하나만 의미 있다.
pub type DetectInput = Arc<Mutex<(u64, Vec<(SessionId, u32)>, HashMap<SessionId, AgentBinding>)>>;

/// 스레드 → App 결과. bindings는 바인딩 tier에서만 Some(활동 tier는 None), activity는 매번.
pub struct DetectOutcome {
    pub epoch: u64,
    pub bindings: Option<HashMap<SessionId, AgentBinding>>,
    pub activity: HashMap<SessionId, AgentActivity>,
    /// 세션별 현재 작업 폴더(바인딩 tier에서만, 한 번의 lsof). 행 폴더명 + 워크스페이스명.
    pub session_cwds: Option<HashMap<SessionId, String>>,
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

impl AgentDetectWorker {
    /// 전용 스레드를 띄운다. 입력 핸들과 결과 수신 채널을 함께 돌려준다.
    pub fn spawn(ctx: egui::Context) -> (Self, DetectInput, mpsc::Receiver<DetectOutcome>) {
        let input: DetectInput = Arc::new(Mutex::new((0, Vec::new(), HashMap::new())));
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
                loop {
                    // 다음 만기까지 잔다. 활동 tier는 바인딩이 있을 때만 (없으면 바인딩 tier만).
                    let binding_wait = BINDING_INTERVAL.saturating_sub(last_binding.elapsed());
                    let activity_wait = if bindings.is_empty() {
                        Duration::from_secs(3600)
                    } else {
                        ACTIVITY_INTERVAL.saturating_sub(last_activity.elapsed())
                    };
                    let wait = binding_wait
                        .min(activity_wait)
                        .max(Duration::from_millis(50));
                    match stop_rx.recv_timeout(wait) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    let (epoch, sessions, overrides) = input2.lock().unwrap().clone();
                    // 워크스페이스 전환(epoch 변경) 시 캐시를 비운다 — SessionId가 워커마다
                    // 1부터라 캐시가 다른 워크스페이스 세션과 충돌하는 것을 막는다.
                    if epoch != last_epoch {
                        last_epoch = epoch;
                        cache = agent_detect::BindingCache::default();
                        bindings.clear();
                    }
                    if last_binding.elapsed() >= BINDING_INTERVAL {
                        last_binding = Instant::now();
                        last_activity = Instant::now();
                        bindings = agent_detect::detect_cached(&sessions, &overrides, &mut cache);
                        let activity = compute_activity(&bindings);
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
                        });
                        if sent.is_err() {
                            break; // 수신측(App) drop → 종료
                        }
                        ctx.request_repaint();
                    } else if !bindings.is_empty() && last_activity.elapsed() >= ACTIVITY_INTERVAL {
                        last_activity = Instant::now();
                        let activity = compute_activity(&bindings);
                        if out_tx
                            .send(DetectOutcome {
                                epoch,
                                bindings: None,
                                activity,
                                session_cwds: None,
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
