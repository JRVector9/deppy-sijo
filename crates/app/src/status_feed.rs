//! 홈 업데이트 피드 — Claude/OpenAI/GitHub Statuspage, Hugging Face trending
//! models, Grok 공식 상태 RSS. 하단 상태바의 서비스 점등과 홈 업데이트 목록이 쓴다
//! (2026-07-18 사용자). Home 활성 또는 수동 refresh가 처음 생길 때만 백그라운드
//! 워커를 지연 시작해 mpsc로 스냅샷을 보낸다 — UI 스레드 네트워크 금지 관례.
//! Home이 숨겨지면 네트워크와 repaint를 멈추고 idle TTL 뒤 worker를 회수한다.
//! 활성 주기는 이원화: **상태 점등 5분**(터미널 작업용 신선도), **공지 4시간**
//! (사용자 지정) + 홈의 수동 갱신 버튼(refresh 채널). 실패 시 해당 provider만 None
//! (오프라인이어도 앱 동작 무영향).

use std::fmt;
use std::io::Read;
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use std::{
    collections::{BTreeSet, HashSet, VecDeque},
    path::Path,
};

/// 2026-08-14 실증: 커스텀 도메인 status.claude.com은 CNAME(tymt9n04zgry.stspg-customer.com)은
/// 살아있지만 서버가 제시하는 인증서 CN이 `*.statuspage.io`라 호스트명이 일치하지 않아
/// TLS 핸드셰이크가 실패한다(status.anthropic.com도 동일 증상). page id
/// `tymt9n04zgry`가 가리키는 원래 statuspage.io 서브도메인은 인증서·API 응답 모두 정상이고,
/// 브라우저로 이 base를 그대로 열어도 사람이 보는 상태 페이지가 뜬다 — API 베이스와 클릭
/// 시 여는 URL을 겸용해도 된다. 인증서 검증 우회(-k 상당)는 쓰지 않았다. 커스텀 도메인이
/// 복구됐다는 얘기가 나오면 되돌리기 전에 먼저 curl로 인증서를 다시 확인할 것.
pub const CLAUDE_STATUS_URL: &str = "https://anthropic.statuspage.io";
pub const OPENAI_STATUS_URL: &str = "https://status.openai.com";
pub const GITHUB_STATUS_URL: &str = "https://www.githubstatus.com";
const HUGGING_FACE_MODELS_API: &str =
    "https://huggingface.co/api/models?sort=trendingScore&direction=-1&limit=5";
const GROK_STATUS_RSS: &str = "https://status.x.ai/feed.xml";
/// 상태(점등) 폴링 주기 — 장애 감지용이라 짧게 유지.
const STATUS_INTERVAL: Duration = Duration::from_secs(300);
/// Home이 숨겨진 뒤 bounded cache/HTTP pool/thread를 회수하기까지의 유예.
const WORKER_IDLE_TTL: Duration = Duration::from_secs(30);
/// 공지(인시던트 목록) 갱신 주기 (2026-07-21 사용자: 4시간).
const INCIDENTS_INTERVAL: Duration = Duration::from_secs(4 * 60 * 60);
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const STATUS_RESPONSE_MAX_BYTES: usize = 64 * 1024;
const NOTICE_RESPONSE_MAX_BYTES: usize = 4 * 1024 * 1024;
const PROVIDER_DESCRIPTION_MAX_BYTES: usize = 4 * 1024;
const NOTICE_TITLE_MAX_BYTES: usize = 4 * 1024;
const NOTICE_STATUS_MAX_BYTES: usize = 256;
const NOTICE_DATE_MAX_BYTES: usize = 32;
const NOTICE_URL_MAX_BYTES: usize = 8 * 1024;
const NOTICE_READ_PROVIDERS_MAX_ITEMS: usize = 16;
const NOTICE_READ_PROVIDER_MAX_BYTES: usize = 64;
const NOTICE_READ_IDS_MAX_ITEMS: usize = 512;
const NOTICE_READ_ID_MAX_BYTES: usize = 16 * 1024;
const NOTICE_READ_IDS_MAX_BYTES: usize = 512 * 1024;
const NOTICE_READ_STATE_FILE_MAX_BYTES: usize = 4 * 1024 * 1024;
/// 홈 공지 카드 수 (provider당, 2026-07-20 사용자: "5줄").
const INCIDENTS_PER_PROVIDER: usize = 5;
const NOTICE_READ_STATE_VERSION: u32 = 1;

/// Statuspage `status.indicator` 매핑.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceIndicator {
    Operational,
    Minor,
    Major,
    Critical,
    Unknown,
}

impl ServiceIndicator {
    fn from_api(indicator: &str) -> Self {
        match indicator {
            "none" => Self::Operational,
            "minor" => Self::Minor,
            "major" => Self::Major,
            "critical" => Self::Critical,
            _ => Self::Unknown,
        }
    }
}

/// 인시던트 1건 — 홈 공지 카드 1장.
#[derive(Debug, Clone, PartialEq)]
pub struct IncidentNotice {
    pub title: String,
    /// Statuspage 원문 상태 (resolved/investigating/identified/monitoring/postmortem).
    pub status: String,
    /// **로컬 시간대**로 환산한 날짜("2026-07-17") — 카드 하단 표기용.
    /// 원본은 전부 UTC라 변환하지 않으면 상태 페이지와 하루가 어긋난다.
    pub date: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderStatus {
    pub indicator: ServiceIndicator,
    /// 상태 페이지의 요약 문구("All Systems Operational" 등) — hover 표기.
    pub description: String,
    pub incidents: Vec<IncidentNotice>,
}

/// 워커 → App 스냅샷. provider별 None = 이번 라운드 조회 실패(이전 값 유지는 App 몫).
#[derive(Debug, Clone, Default)]
pub struct StatusFeedSnapshot {
    pub claude: Option<ProviderStatus>,
    pub openai: Option<ProviderStatus>,
    pub github: Option<ProviderStatus>,
    pub hugging_face: Option<ProviderStatus>,
    pub grok: Option<ProviderStatus>,
}

/// 결과 backlog는 wake token 하나와 최신 스냅샷 하나로 제한한다. 표준
/// `sync_channel(1)`만으로는 full slot의 오래된 값을 sender가 교체할 수 없으므로,
/// payload는 공유 latest slot에 두고 채널은 수신 가능 여부만 알린다.
pub struct StatusFeedReceiver {
    latest: Arc<Mutex<Option<StatusFeedSnapshot>>>,
    ready_rx: Receiver<()>,
    sender: StatusFeedSender,
    control: Arc<WorkerControl>,
    worker: Option<JoinHandle<()>>,
    egui_ctx: egui::Context,
    idle_ttl: Duration,
    worker_spawner: Arc<WorkerSpawner>,
}

impl StatusFeedReceiver {
    /// Home 표시 상태를 전달한다. `true` 전환 또는 pending refresh만 worker를 지연
    /// 시작한다. 같은 상태를 매 logic tick 전달해도 thread를 추가 생성하지 않는다.
    pub fn set_active(&mut self, active: bool) {
        self.reap_finished_worker();
        self.control.set_active(active);
        self.ensure_worker();
    }

    /// 현재 pending인 최신 스냅샷 하나만 꺼낸다. publish/receive가 교차해 남은 wake
    /// token은 내부에서 버리며, 결과 payload backlog는 항상 0 또는 1이다.
    /// pending refresh가 idle worker를 필요로 하면 이 호출에서 지연 시작한다.
    pub fn try_recv(&mut self) -> Result<StatusFeedSnapshot, TryRecvError> {
        self.reap_finished_worker();
        self.ensure_worker();
        loop {
            match self.ready_rx.try_recv() {
                Ok(()) => {
                    if let Some(snapshot) = self.take_latest() {
                        return Ok(snapshot);
                    }
                }
                Err(TryRecvError::Empty) => return Err(TryRecvError::Empty),
                Err(TryRecvError::Disconnected) => {
                    return self.take_latest().ok_or(TryRecvError::Disconnected);
                }
            }
        }
    }

    fn take_latest(&self) -> Option<StatusFeedSnapshot> {
        self.latest
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    fn ensure_worker(&mut self) {
        if !self.control.start_needed() {
            return;
        }

        // idle-expired worker는 running=false를 같은 lifecycle lock 아래 기록한 뒤
        // 반환한다. 새 start를 reserve하기 전에 짧게 join해 handle 수를 항상 1로 둔다.
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            tracing::debug!("status-feed worker exited after panic");
        }
        if !self.control.reserve_start() {
            return;
        }
        match (self.worker_spawner)(
            Arc::clone(&self.control),
            self.sender.clone(),
            self.egui_ctx.clone(),
            self.idle_ttl,
        ) {
            Ok(worker) => self.worker = Some(worker),
            Err(_) => {
                self.control.spawn_failed();
                tracing::warn!(
                    kind = "status_feed",
                    phase = "worker_start",
                    error_code = "thread_spawn_failed",
                    "status-feed worker failed to start"
                );
            }
        }
    }

    fn reap_finished_worker(&mut self) {
        let handle_finished = self
            .worker
            .as_ref()
            .is_some_and(std::thread::JoinHandle::is_finished);
        let lifecycle_stopped = !self
            .control
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .running;
        if !handle_finished && !lifecycle_stopped {
            return;
        }
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            tracing::debug!("status-feed worker exited after panic");
        }
    }

    /// 진행 중 HTTP 한 건의 bounded timeout 뒤 worker를 반드시 join한다. 반복 호출해도
    /// 안전하며 pending result payload도 즉시 해제한다.
    pub fn shutdown(&mut self) {
        self.control.shutdown();
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            tracing::debug!("status-feed worker exited after panic");
        }
        self.take_latest();
    }
}

impl Drop for StatusFeedReceiver {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[derive(Clone)]
struct StatusFeedSender {
    latest: Arc<Mutex<Option<StatusFeedSnapshot>>>,
    ready_tx: SyncSender<()>,
}

impl StatusFeedSender {
    /// `true`면 최신 값이 게시/병합됐고, `false`면 receiver가 사라졌다.
    fn publish(&self, snapshot: StatusFeedSnapshot) -> bool {
        *self
            .latest
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(snapshot);
        match self.ready_tx.try_send(()) {
            Ok(()) | Err(TrySendError::Full(())) => true,
            Err(TrySendError::Disconnected(())) => {
                // 마지막 sender가 worker와 함께 종료될 때까지 payload를 붙잡지 않는다.
                self.latest
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take();
                false
            }
        }
    }
}

type WorkerSpawner = dyn Fn(
        Arc<WorkerControl>,
        StatusFeedSender,
        egui::Context,
        Duration,
    ) -> std::io::Result<JoinHandle<()>>
    + Send
    + Sync;

fn status_feed_channel_with(
    control: Arc<WorkerControl>,
    egui_ctx: egui::Context,
    idle_ttl: Duration,
    worker_spawner: Arc<WorkerSpawner>,
) -> (StatusFeedSender, StatusFeedReceiver) {
    let latest = Arc::new(Mutex::new(None));
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let sender = StatusFeedSender {
        latest: Arc::clone(&latest),
        ready_tx,
    };
    (
        sender.clone(),
        StatusFeedReceiver {
            latest,
            ready_rx,
            sender,
            control,
            worker: None,
            egui_ctx,
            idle_ttl,
            worker_spawner,
        },
    )
}

/// 수동 refresh 요청도 하나만 pending한다. `send`는 기존 호출 형태를 보존하지만
/// 절대 block하지 않으며, 이미 pending이면 성공으로 병합한다.
#[derive(Clone)]
pub struct StatusFeedRefresh {
    control: Arc<WorkerControl>,
    egui_ctx: egui::Context,
}

impl StatusFeedRefresh {
    pub fn send(&self, _signal: ()) -> Result<(), std::sync::mpsc::SendError<()>> {
        match self.control.request_refresh() {
            Some(newly_pending) => {
                // refresh는 명시적 사용자 intent다. worker가 아직 없으면 다음 logic
                // tick의 receiver drain이 지연 시작하도록 최초 pending 전환만 깨운다.
                if newly_pending {
                    self.egui_ctx.request_repaint();
                }
                Ok(())
            }
            None => Err(std::sync::mpsc::SendError(())),
        }
    }
}

#[derive(Default)]
struct WorkerControlState {
    active: bool,
    refresh_pending: bool,
    shutdown: bool,
    running: bool,
    restart_blocked: bool,
}

#[derive(Default)]
struct WorkerControl {
    state: Mutex<WorkerControlState>,
    wake: Condvar,
}

impl WorkerControl {
    fn set_active(&self, active: bool) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.shutdown || state.active == active {
            return;
        }
        state.active = active;
        if active {
            // spawn 실패/worker panic은 frame-loop retry하지 않는다. 새 활성 intent만
            // latch를 해제한다.
            state.restart_blocked = false;
        }
        self.wake.notify_one();
    }

    /// `Some(true)`는 새 pending 전환, `Some(false)`는 기존 request에 병합, `None`은
    /// shutdown 뒤 거부다.
    fn request_refresh(&self) -> Option<bool> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.shutdown {
            return None;
        }
        let newly_pending = !state.refresh_pending;
        state.refresh_pending = true;
        state.restart_blocked = false;
        self.wake.notify_one();
        Some(newly_pending)
    }

    fn shutdown(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.shutdown = true;
        state.active = false;
        state.refresh_pending = false;
        self.wake.notify_one();
    }

    fn start_needed(&self) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        !state.shutdown
            && !state.running
            && !state.restart_blocked
            && (state.active || state.refresh_pending)
    }

    fn reserve_start(&self) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.shutdown
            || state.running
            || state.restart_blocked
            || (!state.active && !state.refresh_pending)
        {
            return false;
        }
        state.running = true;
        true
    }

    fn spawn_failed(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.running = false;
        state.refresh_pending = false;
        state.restart_blocked = true;
        self.wake.notify_all();
    }

    fn worker_exited(&self, unexpected: bool) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.running = false;
        if unexpected && !state.shutdown {
            state.restart_blocked = true;
        }
        self.wake.notify_all();
    }

    fn take_initial_cycle(&self) -> Option<WorkerCycle> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.shutdown {
            state.running = false;
            None
        } else if state.refresh_pending {
            state.refresh_pending = false;
            Some(WorkerCycle::Refresh)
        } else if state.active {
            Some(WorkerCycle::Active)
        } else {
            // A visibility transition can cancel the only requested cycle before the new thread
            // starts. Publish the clean stop while holding the lifecycle lock so a concurrent
            // reactivation joins this handle and reserves exactly one replacement.
            state.running = false;
            None
        }
    }

    fn cycle_allowed(&self, cycle: WorkerCycle) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        !state.shutdown && (cycle == WorkerCycle::Refresh || state.active)
    }

    fn is_active(&self) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        !state.shutdown && state.active
    }

    fn wait_for_work(&self, poll_interval: Duration, idle_ttl: Duration) -> WorkerWake {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if state.shutdown {
                return WorkerWake::Shutdown;
            }
            if state.refresh_pending {
                state.refresh_pending = false;
                return WorkerWake::Refresh;
            }
            if state.active {
                let (next, timeout) = self
                    .wake
                    .wait_timeout_while(state, poll_interval, |state| {
                        state.active && !state.refresh_pending && !state.shutdown
                    })
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state = next;
                if timeout.timed_out() && state.active && !state.shutdown {
                    return WorkerWake::Poll;
                }
                continue;
            }

            let (next, timeout) = self
                .wake
                .wait_timeout_while(state, idle_ttl, |state| {
                    !state.active && !state.refresh_pending && !state.shutdown
                })
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state = next;
            if timeout.timed_out() && !state.active && !state.refresh_pending && !state.shutdown {
                // Make the clean idle exit observable under the same lifecycle lock as an
                // activation intent. The receiver can now join this handle before reserving its
                // replacement instead of losing the only activation edge in a pre-exit gap.
                state.running = false;
                return WorkerWake::IdleExpired;
            }
            if state.active && !state.shutdown {
                return WorkerWake::Poll;
            }
        }
    }
}

enum WorkerWake {
    Refresh,
    Poll,
    IdleExpired,
    Shutdown,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WorkerCycle {
    Active,
    Refresh,
}

struct WorkerExitGuard {
    control: Arc<WorkerControl>,
    clean: bool,
}

impl WorkerExitGuard {
    fn new(control: Arc<WorkerControl>) -> Self {
        Self {
            control,
            clean: false,
        }
    }

    fn complete(mut self) {
        self.clean = true;
    }
}

impl Drop for WorkerExitGuard {
    fn drop(&mut self) {
        self.control.worker_exited(!self.clean);
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct NoticeReadStateFile {
    version: u32,
    initialized_providers: Vec<String>,
    read_ids: Vec<String>,
}

/// 홈 공지의 읽음 기준. 공급자별 첫 성공 조회는 기존 공지로 기준화하고, 이후 새 URL만
/// 배지에 센다. 읽은 ID는 최근 512건/512 KiB rolling window로 유지해 한때 최신 5건
/// 밖으로 밀린 공지의 재진입을 막되, 장기 실행에서 RAM/상태 파일이 계속 자라지 않는다.
#[derive(Default)]
pub struct NoticeReadState {
    initialized_providers: BTreeSet<String>,
    read_ids: HashSet<String>,
    read_order: VecDeque<String>,
    read_bytes: usize,
}

impl fmt::Debug for NoticeReadState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NoticeReadState")
            .field("initialized_providers", &self.initialized_providers.len())
            .field("read_ids", &self.read_ids.len())
            .field("read_bytes", &self.read_bytes)
            .finish()
    }
}

impl NoticeReadState {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(error.into()),
        };
        let mut bytes = Vec::new();
        file.take((NOTICE_READ_STATE_FILE_MAX_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        anyhow::ensure!(
            bytes.len() <= NOTICE_READ_STATE_FILE_MAX_BYTES,
            "공지 읽음 상태 파일이 크기 상한을 초과했습니다"
        );
        let file: NoticeReadStateFile = serde_json::from_slice(&bytes)?;
        if file.version != NOTICE_READ_STATE_VERSION {
            anyhow::bail!(
                "지원하지 않는 공지 읽음 상태 버전: {} (현재 {NOTICE_READ_STATE_VERSION})",
                file.version
            );
        }
        let initialized_providers = file
            .initialized_providers
            .into_iter()
            .filter(|provider| valid_bounded_text(provider, NOTICE_READ_PROVIDER_MAX_BYTES, false))
            .take(NOTICE_READ_PROVIDERS_MAX_ITEMS)
            .collect();
        let mut state = Self {
            initialized_providers,
            ..Self::default()
        };
        for id in file.read_ids {
            let _ = state.insert_read_id(id);
        }
        Ok(state)
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = NoticeReadStateFile {
            version: NOTICE_READ_STATE_VERSION,
            initialized_providers: self.initialized_providers.iter().cloned().collect(),
            read_ids: self.read_order.iter().cloned().collect(),
        };
        let bytes = serde_json::to_vec_pretty(&file)?;
        anyhow::ensure!(
            bytes.len() <= NOTICE_READ_STATE_FILE_MAX_BYTES,
            "공지 읽음 상태 직렬화가 크기 상한을 초과했습니다"
        );
        deppy_core::fs::atomic_write(path, &bytes)?;
        Ok(())
    }

    /// 새 공급자의 첫 성공 응답은 baseline으로 읽음 처리한다. `mark_read`는 Home이 실제
    /// 선택된 경우이며, 현재 목록의 URL을 그 공급자의 누적 읽음 기준에 추가한다.
    pub fn reconcile(&mut self, feed: &StatusFeedSnapshot, mark_read: bool) -> bool {
        let mut changed = false;
        for (provider, status) in announcement_providers(feed) {
            let Some(status) = status else { continue };
            let first_success = if self.initialized_providers.contains(provider) {
                false
            } else if self.initialized_providers.len() < NOTICE_READ_PROVIDERS_MAX_ITEMS
                && valid_bounded_text(provider, NOTICE_READ_PROVIDER_MAX_BYTES, false)
            {
                self.initialized_providers.insert(provider.to_owned())
            } else {
                false
            };
            changed |= first_success;
            if first_success || mark_read {
                changed |= self.mark_provider_read(provider, status);
            }
        }
        changed
    }

    pub fn unread_count(&self, feed: &StatusFeedSnapshot) -> usize {
        announcement_providers(feed)
            .into_iter()
            .filter_map(|(provider, status)| {
                if !self.initialized_providers.contains(provider) {
                    return None;
                }
                status.map(|status| (provider, status))
            })
            .flat_map(|(provider, status)| {
                status
                    .incidents
                    .iter()
                    .map(move |incident| notice_id(provider, incident))
            })
            .filter(|id| !self.read_ids.contains(id))
            .collect::<BTreeSet<_>>()
            .len()
    }

    fn mark_provider_read(&mut self, provider: &str, status: &ProviderStatus) -> bool {
        let mut changed = false;
        for incident in &status.incidents {
            changed |= self.insert_read_id(notice_id(provider, incident));
        }
        changed
    }

    fn insert_read_id(&mut self, id: String) -> bool {
        if !valid_bounded_text(&id, NOTICE_READ_ID_MAX_BYTES, false) || self.read_ids.contains(&id)
        {
            return false;
        }
        self.read_bytes = self.read_bytes.saturating_add(id.len());
        self.read_ids.insert(id.clone());
        self.read_order.push_back(id);
        while self.read_order.len() > NOTICE_READ_IDS_MAX_ITEMS
            || self.read_bytes > NOTICE_READ_IDS_MAX_BYTES
        {
            let Some(evicted) = self.read_order.pop_front() else {
                break;
            };
            self.read_bytes = self.read_bytes.saturating_sub(evicted.len());
            self.read_ids.remove(&evicted);
        }
        true
    }
}

fn announcement_providers(
    feed: &StatusFeedSnapshot,
) -> [(&'static str, Option<&ProviderStatus>); 4] {
    [
        ("OpenAI", feed.openai.as_ref()),
        ("Claude", feed.claude.as_ref()),
        ("Grok", feed.grok.as_ref()),
        ("Hugging Face", feed.hugging_face.as_ref()),
    ]
}

fn notice_id(provider: &str, incident: &IncidentNotice) -> String {
    let identity = if incident.url.trim().is_empty() {
        format!("{}\u{1f}{}", incident.date, incident.title)
    } else {
        incident.url.trim().to_owned()
    };
    format!("{provider}\u{1f}{identity}")
}

struct ProductionFeedFetcher {
    agent: ureq::Agent,
    incidents_at: Option<Instant>,
    claude_incidents: Vec<IncidentNotice>,
    openai_incidents: Vec<IncidentNotice>,
    hugging_face_updates: Option<Vec<IncidentNotice>>,
    grok_updates: Option<Vec<IncidentNotice>>,
}

impl ProductionFeedFetcher {
    fn new() -> Self {
        Self {
            agent: ureq::builder().timeout(HTTP_TIMEOUT).build(),
            incidents_at: None,
            claude_incidents: Vec::new(),
            openai_incidents: Vec::new(),
            hugging_face_updates: None,
            grok_updates: None,
        }
    }

    fn fetch(&mut self, cycle: WorkerCycle, control: &WorkerControl) -> Option<StatusFeedSnapshot> {
        if !control.cycle_allowed(cycle) {
            return None;
        }
        let incidents_due = cycle == WorkerCycle::Refresh
            || self
                .incidents_at
                .is_none_or(|at| at.elapsed() >= INCIDENTS_INTERVAL);
        if incidents_due {
            if let Ok(list) = fetch_incidents(&self.agent, CLAUDE_STATUS_URL).map_err(|_| {
                tracing::debug!(
                    kind = "status_feed",
                    phase = "claude_notice",
                    error_code = "fetch_failed",
                    "status feed fetch failed"
                )
            }) {
                self.claude_incidents = list;
            }
            if !control.cycle_allowed(cycle) {
                return None;
            }
            if let Ok(list) = fetch_incidents(&self.agent, OPENAI_STATUS_URL).map_err(|_| {
                tracing::debug!(
                    kind = "status_feed",
                    phase = "openai_notice",
                    error_code = "fetch_failed",
                    "status feed fetch failed"
                )
            }) {
                self.openai_incidents = list;
            }
            if !control.cycle_allowed(cycle) {
                return None;
            }
            if let Ok(list) = fetch_hugging_face_models(&self.agent).map_err(|_| {
                tracing::debug!(
                    kind = "status_feed",
                    phase = "hugging_face_notice",
                    error_code = "fetch_failed",
                    "status feed fetch failed"
                )
            }) {
                self.hugging_face_updates = Some(list);
            }
            if !control.cycle_allowed(cycle) {
                return None;
            }
            if let Ok(list) = fetch_grok_status(&self.agent).map_err(|_| {
                tracing::debug!(
                    kind = "status_feed",
                    phase = "grok_notice",
                    error_code = "fetch_failed",
                    "status feed fetch failed"
                )
            }) {
                self.grok_updates = Some(list);
            }
            self.incidents_at = Some(Instant::now());
        }

        if !control.cycle_allowed(cycle) {
            return None;
        }
        let claude = fetch_status(&self.agent, CLAUDE_STATUS_URL)
            .map_err(|_| {
                tracing::debug!(
                    kind = "status_feed",
                    phase = "claude_status",
                    error_code = "fetch_failed",
                    "status feed fetch failed"
                )
            })
            .ok()
            .map(|(indicator, description)| ProviderStatus {
                indicator,
                description,
                incidents: self.claude_incidents.clone(),
            });
        if !control.cycle_allowed(cycle) {
            return None;
        }
        let openai = fetch_status(&self.agent, OPENAI_STATUS_URL)
            .map_err(|_| {
                tracing::debug!(
                    kind = "status_feed",
                    phase = "openai_status",
                    error_code = "fetch_failed",
                    "status feed fetch failed"
                )
            })
            .ok()
            .map(|(indicator, description)| ProviderStatus {
                indicator,
                description,
                incidents: self.openai_incidents.clone(),
            });
        if !control.cycle_allowed(cycle) {
            return None;
        }
        let github = fetch_status(&self.agent, GITHUB_STATUS_URL)
            .map_err(|_| {
                tracing::debug!(
                    kind = "status_feed",
                    phase = "github_status",
                    error_code = "fetch_failed",
                    "status feed fetch failed"
                )
            })
            .ok()
            .map(|(indicator, description)| ProviderStatus {
                indicator,
                description,
                incidents: Vec::new(),
            });
        Some(StatusFeedSnapshot {
            claude,
            openai,
            github,
            hugging_face: self
                .hugging_face_updates
                .as_ref()
                .map(|updates| ProviderStatus {
                    indicator: ServiceIndicator::Operational,
                    description: "Trending models".to_owned(),
                    incidents: updates.clone(),
                }),
            grok: self.grok_updates.as_ref().map(|updates| ProviderStatus {
                indicator: ServiceIndicator::Operational,
                description: "Grok status updates".to_owned(),
                incidents: updates.clone(),
            }),
        })
    }
}

fn run_status_feed_worker<F, R>(
    control: &Arc<WorkerControl>,
    sender: &StatusFeedSender,
    idle_ttl: Duration,
    mut fetch: F,
    mut request_repaint: R,
) where
    F: FnMut(WorkerCycle, &WorkerControl) -> Option<StatusFeedSnapshot>,
    R: FnMut(),
{
    let Some(mut cycle) = control.take_initial_cycle() else {
        return;
    };
    loop {
        if let Some(snapshot) = fetch(cycle, control)
            && control.cycle_allowed(cycle)
        {
            if !sender.publish(snapshot) {
                return;
            }
            // 명시적 hidden refresh 결과는 캐시에 남기되 inactive viewport를 깨우지 않는다.
            if control.is_active() {
                request_repaint();
            }
        }
        cycle = match control.wait_for_work(STATUS_INTERVAL, idle_ttl) {
            WorkerWake::Refresh => WorkerCycle::Refresh,
            WorkerWake::Poll => WorkerCycle::Active,
            WorkerWake::IdleExpired | WorkerWake::Shutdown => return,
        };
    }
}

fn production_worker_spawner() -> Arc<WorkerSpawner> {
    Arc::new(|control, sender, egui_ctx, idle_ttl| {
        std::thread::Builder::new()
            .name("status-feed".into())
            .spawn(move || {
                let exit_guard = WorkerExitGuard::new(Arc::clone(&control));
                let mut fetcher = ProductionFeedFetcher::new();
                run_status_feed_worker(
                    &control,
                    &sender,
                    idle_ttl,
                    |cycle, control| fetcher.fetch(cycle, control),
                    || egui_ctx.request_repaint(),
                );
                exit_guard.complete();
            })
    })
}

fn new_with_spawner(
    egui_ctx: egui::Context,
    idle_ttl: Duration,
    worker_spawner: Arc<WorkerSpawner>,
) -> (StatusFeedReceiver, StatusFeedRefresh) {
    let control = Arc::new(WorkerControl::default());
    let (_, receiver) = status_feed_channel_with(
        Arc::clone(&control),
        egui_ctx.clone(),
        idle_ttl,
        worker_spawner,
    );
    (receiver, StatusFeedRefresh { control, egui_ctx })
}

/// 스냅샷 수신/수동 갱신 handle만 만든다. thread, HTTP agent, timer, network request,
/// repaint는 만들지 않으며 `StatusFeedReceiver::set_active(true)` 또는 pending refresh를
/// 처음 관측할 때 worker를 지연 시작한다.
pub fn new(egui_ctx: egui::Context) -> (StatusFeedReceiver, StatusFeedRefresh) {
    new_with_spawner(egui_ctx, WORKER_IDLE_TTL, production_worker_spawner())
}

/// 기존 call site 호환 alias. 이름과 달리 생성 시 worker를 spawn하지 않는다.
pub fn spawn(egui_ctx: egui::Context) -> (StatusFeedReceiver, StatusFeedRefresh) {
    let (mut receiver, refresh) = new(egui_ctx);
    // 기존 App call site도 새 lifecycle API를 통과해 명시적인 inactive 상태로 시작한다.
    // false→false라 lock 확인 외 side effect는 없다.
    receiver.set_active(false);
    (receiver, refresh)
}

fn fetch_status(agent: &ureq::Agent, base: &str) -> anyhow::Result<(ServiceIndicator, String)> {
    let response = agent.get(&format!("{base}/api/v2/status.json")).call()?;
    let status_json = read_response_limited(response, STATUS_RESPONSE_MAX_BYTES)?;
    parse_status(&status_json)
}

fn fetch_incidents(agent: &ureq::Agent, base: &str) -> anyhow::Result<Vec<IncidentNotice>> {
    let response = agent.get(&format!("{base}/api/v2/incidents.json")).call()?;
    let incidents_json = read_response_limited(response, NOTICE_RESPONSE_MAX_BYTES)?;
    parse_incidents(&incidents_json, base)
}

fn fetch_hugging_face_models(agent: &ureq::Agent) -> anyhow::Result<Vec<IncidentNotice>> {
    let response = agent
        .get(HUGGING_FACE_MODELS_API)
        .set("Accept", "application/json")
        .set("User-Agent", "Deppy-Sijo/External-Updates")
        .call()?;
    let json = read_response_limited(response, NOTICE_RESPONSE_MAX_BYTES)?;
    parse_hugging_face_models(&json)
}

fn fetch_grok_status(agent: &ureq::Agent) -> anyhow::Result<Vec<IncidentNotice>> {
    let response = agent
        .get(GROK_STATUS_RSS)
        .set(
            "Accept",
            "application/rss+xml, application/xml;q=0.9, text/xml;q=0.8",
        )
        .set("User-Agent", "Deppy-Sijo/External-Updates")
        .call()?;
    let json = read_response_limited(response, NOTICE_RESPONSE_MAX_BYTES)?;
    parse_grok_status_rss(&json)
}

fn read_response_limited(response: ureq::Response, max_bytes: usize) -> anyhow::Result<String> {
    read_utf8_limited(response.into_reader(), max_bytes)
}

fn read_utf8_limited(reader: impl Read, max_bytes: usize) -> anyhow::Result<String> {
    let mut bytes = Vec::with_capacity(max_bytes.min(64 * 1024));
    reader
        .take(max_bytes.saturating_add(1) as u64)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len() <= max_bytes, "HTTP 응답 크기 상한 초과");
    String::from_utf8(bytes).map_err(Into::into)
}

fn valid_bounded_text(value: &str, max_bytes: usize, allow_empty: bool) -> bool {
    (allow_empty || !value.trim().is_empty())
        && value.len() <= max_bytes
        && !value.as_bytes().contains(&0)
}

/// `/api/v2/status.json` → (indicator, description).
fn parse_status(json: &str) -> anyhow::Result<(ServiceIndicator, String)> {
    let value: serde_json::Value = serde_json::from_str(json)?;
    let status = value
        .get("status")
        .ok_or_else(|| anyhow::anyhow!("status 필드 없음"))?;
    let indicator = status
        .get("indicator")
        .and_then(|v| v.as_str())
        .map(ServiceIndicator::from_api)
        .unwrap_or(ServiceIndicator::Unknown);
    let description = status
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    anyhow::ensure!(
        valid_bounded_text(description, PROVIDER_DESCRIPTION_MAX_BYTES, true),
        "status description 크기 상한 초과"
    );
    Ok((indicator, description.to_owned()))
}

/// `/api/v2/incidents.json` → 최신 5건. 링크는 shortlink 우선, 없으면(OpenAI가 그렇다)
/// Statuspage 표준 경로 `{base}/incidents/{id}`로 조립한다.
fn parse_incidents(json: &str, base: &str) -> anyhow::Result<Vec<IncidentNotice>> {
    let value: serde_json::Value = serde_json::from_str(json)?;
    let incidents = value
        .get("incidents")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow::anyhow!("incidents 배열 없음"))?;
    Ok(incidents
        .iter()
        .filter_map(|incident| {
            let title = incident.get("name")?.as_str()?;
            let status = incident
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !valid_bounded_text(title, NOTICE_TITLE_MAX_BYTES, false)
                || !valid_bounded_text(status, NOTICE_STATUS_MAX_BYTES, true)
            {
                return None;
            }
            // 상태 페이지와 RSS(pubDate)는 **마지막 갱신**을 그 사건의 날짜로 쓴다.
            // created_at만 보면 오래 끈 사건이 시작일에 묶여 실제 활동과 어긋난다.
            let date = incident
                .get("updated_at")
                .or_else(|| incident.get("created_at"))
                .and_then(|v| v.as_str())
                .map(local_date)
                .unwrap_or_default();
            let url = incident
                .get("shortlink")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
                .or_else(|| {
                    incident
                        .get("id")
                        .and_then(|v| v.as_str())
                        .map(|id| format!("{base}/incidents/{id}"))
                })
                .unwrap_or_else(|| base.to_owned());
            if !valid_bounded_text(&date, NOTICE_DATE_MAX_BYTES, true)
                || !valid_bounded_text(&url, NOTICE_URL_MAX_BYTES, false)
            {
                return None;
            }
            Some(IncidentNotice {
                title: title.to_owned(),
                status: status.to_owned(),
                date,
                url,
            })
        })
        .take(INCIDENTS_PER_PROVIDER)
        .collect())
}

fn parse_hugging_face_models(json: &str) -> anyhow::Result<Vec<IncidentNotice>> {
    let models: Vec<serde_json::Value> = serde_json::from_str(json)?;
    Ok(models
        .into_iter()
        .filter_map(|model| {
            let title = model.get("id").or_else(|| model.get("modelId"))?.as_str()?;
            if !valid_bounded_text(title, NOTICE_TITLE_MAX_BYTES, false) {
                return None;
            }
            let date = model
                .get("createdAt")
                .and_then(|value| value.as_str())
                .map(local_date)
                .unwrap_or_default();
            let url = format!("https://huggingface.co/{title}");
            if !valid_bounded_text(&date, NOTICE_DATE_MAX_BYTES, true)
                || !valid_bounded_text(&url, NOTICE_URL_MAX_BYTES, false)
            {
                return None;
            }
            Some(IncidentNotice {
                url,
                title: title.to_owned(),
                status: "trending".to_owned(),
                date,
            })
        })
        .take(INCIDENTS_PER_PROVIDER)
        .collect())
}

#[derive(serde::Deserialize)]
struct GrokRss {
    channel: GrokRssChannel,
}

#[derive(serde::Deserialize)]
struct GrokRssChannel {
    #[serde(default)]
    item: Vec<GrokRssItem>,
}

#[derive(serde::Deserialize)]
struct GrokRssItem {
    title: String,
    link: String,
    #[serde(rename = "pubDate", default)]
    published_at: String,
}

fn parse_grok_status_rss(xml: &str) -> anyhow::Result<Vec<IncidentNotice>> {
    let feed: GrokRss = quick_xml::de::from_str(xml)?;
    let mut seen_titles = std::collections::HashSet::new();
    let mut notices = Vec::with_capacity(INCIDENTS_PER_PROVIDER);
    for item in feed.channel.item {
        let title = item.title.trim();
        let link = item.link.trim();
        if !valid_bounded_text(title, NOTICE_TITLE_MAX_BYTES, false)
            || !valid_bounded_text(link, NOTICE_URL_MAX_BYTES, false)
        {
            continue;
        }
        // Status RSS가 같은 사건을 반복 게시하는 경우가 있어, 대소문자와 연속 공백을
        // 무시한 제목 기준으로 첫 항목만 남긴다. 원래 최신순은 그대로 보존한다.
        let dedupe_key = title
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        if !seen_titles.insert(dedupe_key) {
            continue;
        }
        let date = local_date(&item.published_at);
        if !valid_bounded_text(&date, NOTICE_DATE_MAX_BYTES, true) {
            continue;
        }
        notices.push(IncidentNotice {
            title: title.to_owned(),
            status: "update".to_owned(),
            date,
            url: link.to_owned(),
        });
        if notices.len() == INCIDENTS_PER_PROVIDER {
            break;
        }
    }
    Ok(notices)
}

/// 공지 타임스탬프 → **로컬 시간대** 날짜("YYYY-MM-DD").
///
/// 원본은 모두 UTC다(Statuspage/incident.io는 `...Z`, RSS는 `... GMT`). 예전엔 ISO
/// 문자열의 앞 10글자를 그대로 썼는데, 그러면 UTC 15시 이후 사건이 KST 기준 다음
/// 날인데도 전날로 찍힌다 — 상태 페이지엔 8월 6일로 보이는 항목이 앱에선 8월 5일이었다
/// (2026-08-08 사용자). 파싱에 실패하면 빈 문자열이라 호출측이 그 항목을 버린다.
fn local_date(value: &str) -> String {
    let Some(utc_secs) = parse_timestamp_utc_secs(value) else {
        return String::new();
    };
    civil_date(utc_secs + local_utc_offset_secs(utc_secs))
}

/// 로컬 시간대 오프셋(초). std에는 시간대 정보가 없고 이 워크스페이스는 chrono를
/// 쓰지 않는 관례라(diff_panel.rs 참조), unix에서는 이미 의존 중인 libc의
/// `localtime_r`로 OS가 계산한 값을 읽는다. 그 외 플랫폼은 UTC 그대로 둔다.
#[cfg(unix)]
fn local_utc_offset_secs(utc_secs: i64) -> i64 {
    let time = utc_secs as libc::time_t;
    let mut parts: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: `parts`는 스택에 있고 localtime_r이 채운다(전역 상태를 안 쓰는 변형).
    // 실패하면 널을 돌려주므로 그때는 UTC(0)로 떨어진다.
    if unsafe { libc::localtime_r(&time, &mut parts) }.is_null() {
        return 0;
    }
    parts.tm_gmtoff as i64
}

#[cfg(not(unix))]
fn local_utc_offset_secs(_utc_secs: i64) -> i64 {
    0
}

/// ISO 8601(`2026-08-05T23:22:45.278Z`)과 RFC 822(`Wed, 05 Aug 2026 23:22:45 GMT`)를
/// unix 초로. 두 형식 모두 UTC로만 오므로 오프셋 표기는 다루지 않는다.
fn parse_timestamp_utc_secs(value: &str) -> Option<i64> {
    let value = value.trim();
    let bytes = value.as_bytes();
    if bytes.len() >= 19 && bytes.get(4) == Some(&b'-') && bytes.get(7) == Some(&b'-') {
        let year: i64 = value.get(0..4)?.parse().ok()?;
        let month: i64 = value.get(5..7)?.parse().ok()?;
        let day: i64 = value.get(8..10)?.parse().ok()?;
        let hour: i64 = value.get(11..13)?.parse().ok()?;
        let minute: i64 = value.get(14..16)?.parse().ok()?;
        let second: i64 = value.get(17..19)?.parse().ok()?;
        return civil_to_unix(year, month, day, hour, minute, second);
    }

    // RFC 822: 요일과 쉼표를 떼고 "05 Aug 2026 23:22:45 GMT".
    let rest = value.trim_start_matches(|c: char| c.is_ascii_alphabetic() || c == ',');
    let parts: Vec<_> = rest.split_whitespace().collect();
    let [day, month, year, clock, ..] = parts.as_slice() else {
        return None;
    };
    let month = MONTH_ABBREVIATIONS.iter().position(|name| name == month)? as i64 + 1;
    let mut clock = clock.split(':');
    let hour: i64 = clock.next()?.parse().ok()?;
    let minute: i64 = clock.next()?.parse().ok()?;
    let second: i64 = clock.next().unwrap_or("0").parse().ok()?;
    civil_to_unix(
        year.parse().ok()?,
        month,
        day.parse().ok()?,
        hour,
        minute,
        second,
    )
}

const MONTH_ABBREVIATIONS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// (년,월,일,시,분,초) → unix 초. Howard Hinnant의 days_from_civil.
fn civil_to_unix(
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    if !(0..=23).contains(&hour) || !(0..=59).contains(&minute) || !(0..=60).contains(&second) {
        return None;
    }
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// unix 초 → "YYYY-MM-DD". days_from_civil의 역함수(civil_from_days).
fn civil_date(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn inert_worker_spawner() -> Arc<WorkerSpawner> {
        Arc::new(|_, _, _, _| Err(std::io::Error::other("inert test spawner")))
    }

    fn test_status_feed_channel(
        control: Arc<WorkerControl>,
    ) -> (StatusFeedSender, StatusFeedReceiver) {
        status_feed_channel_with(
            control,
            egui::Context::default(),
            Duration::from_secs(60),
            inert_worker_spawner(),
        )
    }

    fn fake_worker_spawner(
        starts: Arc<AtomicUsize>,
        fetches: Arc<AtomicUsize>,
    ) -> Arc<WorkerSpawner> {
        Arc::new(move |control, sender, egui_ctx, idle_ttl| {
            starts.fetch_add(1, Ordering::SeqCst);
            let fetches = Arc::clone(&fetches);
            std::thread::Builder::new()
                .name("status-feed-test".into())
                .spawn(move || {
                    let exit_guard = WorkerExitGuard::new(Arc::clone(&control));
                    run_status_feed_worker(
                        &control,
                        &sender,
                        idle_ttl,
                        |_, _| {
                            let number = fetches.fetch_add(1, Ordering::SeqCst) + 1;
                            Some(numbered_snapshot(number))
                        },
                        || egui_ctx.request_repaint(),
                    );
                    exit_guard.complete();
                })
        })
    }

    fn receive_eventually(receiver: &mut StatusFeedReceiver) -> StatusFeedSnapshot {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match receiver.try_recv() {
                Ok(snapshot) => return snapshot,
                Err(TryRecvError::Empty) if Instant::now() < deadline => {
                    std::thread::yield_now();
                }
                result => panic!("status feed result did not arrive: {result:?}"),
            }
        }
    }

    fn wait_until_stopped(control: &WorkerControl) {
        let state = control
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (state, timeout) = control
            .wake
            .wait_timeout_while(state, Duration::from_secs(1), |state| state.running)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(!timeout.timed_out(), "status feed worker did not stop");
        assert!(!state.running);
    }

    fn test_notice(url: &str) -> IncidentNotice {
        IncidentNotice {
            title: format!("Notice {url}"),
            status: "resolved".to_owned(),
            date: "2026-07-21".to_owned(),
            url: url.to_owned(),
        }
    }

    fn test_provider(urls: &[&str]) -> ProviderStatus {
        ProviderStatus {
            indicator: ServiceIndicator::Operational,
            description: "Operational".to_owned(),
            incidents: urls.iter().map(|url| test_notice(url)).collect(),
        }
    }

    fn numbered_snapshot(number: usize) -> StatusFeedSnapshot {
        StatusFeedSnapshot {
            openai: Some(ProviderStatus {
                indicator: ServiceIndicator::Operational,
                description: number.to_string(),
                incidents: Vec::new(),
            }),
            ..StatusFeedSnapshot::default()
        }
    }

    #[test]
    fn 결과_backlog는_하나이며_항상_최신값으로_병합한다() {
        let control = Arc::new(WorkerControl::default());
        let (tx, mut rx) = test_status_feed_channel(control);

        assert!(tx.publish(numbered_snapshot(1)));
        assert!(tx.publish(numbered_snapshot(2)));
        assert!(tx.publish(numbered_snapshot(3)));

        let snapshot = rx.try_recv().expect("최신 pending 결과");
        assert_eq!(
            snapshot
                .openai
                .as_ref()
                .map(|status| status.description.as_str()),
            Some("3")
        );
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn 결과_receiver가_사라지면_sender는_즉시_종료_신호를_받는다() {
        let control = Arc::new(WorkerControl::default());
        let (tx, rx) = test_status_feed_channel(control);
        drop(rx);

        assert!(!tx.publish(numbered_snapshot(1)));
        assert!(
            tx.latest
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_none(),
            "disconnect 뒤 secret-free snapshot도 worker sender에 남기지 않는다"
        );
    }

    #[test]
    fn 수동_refresh는_하나로_병합되고_disconnect에서_block하지_않는다() {
        let control = Arc::new(WorkerControl::default());
        let refresh = StatusFeedRefresh {
            control: Arc::clone(&control),
            egui_ctx: egui::Context::default(),
        };

        assert!(refresh.send(()).is_ok());
        assert!(refresh.send(()).is_ok(), "full 요청은 하나로 병합");
        assert!(control.state.lock().unwrap().refresh_pending);

        control.shutdown();
        assert!(refresh.send(()).is_err());
    }

    #[test]
    fn receiver_drop은_대기중_worker를_깨우고_join한다() {
        let control = Arc::new(WorkerControl::default());
        let (_tx, mut rx) = test_status_feed_channel(Arc::clone(&control));
        let (finished_tx, finished_rx) = std::sync::mpsc::sync_channel(1);
        let worker_control = Arc::clone(&control);
        rx.worker = Some(std::thread::spawn(move || {
            assert!(matches!(
                worker_control.wait_for_work(Duration::from_secs(60), Duration::from_secs(60)),
                WorkerWake::Shutdown
            ));
            finished_tx.send(()).unwrap();
        }));

        drop(rx);
        assert_eq!(finished_rx.try_recv(), Ok(()));
    }

    #[test]
    fn construction은_thread_network_timer_repaint를_시작하지_않는다() {
        let repaints = Arc::new(AtomicUsize::new(0));
        let ctx = egui::Context::default();
        let repaint_counter = Arc::clone(&repaints);
        ctx.set_request_repaint_callback(move |_| {
            repaint_counter.fetch_add(1, Ordering::SeqCst);
        });

        let (receiver, _refresh) = new(ctx);

        assert_eq!(repaints.load(Ordering::SeqCst), 0);
        assert!(receiver.worker.is_none());
        assert!(!receiver.control.state.lock().unwrap().running);
    }

    #[test]
    fn active_intent만_lazy_start하고_hidden_idle_ttl뒤_join_reap한다() {
        let starts = Arc::new(AtomicUsize::new(0));
        let fetches = Arc::new(AtomicUsize::new(0));
        let repaints = Arc::new(AtomicUsize::new(0));
        let ctx = egui::Context::default();
        let repaint_counter = Arc::clone(&repaints);
        ctx.set_request_repaint_callback(move |_| {
            repaint_counter.fetch_add(1, Ordering::SeqCst);
        });
        let (mut receiver, _refresh) = new_with_spawner(
            ctx,
            Duration::from_millis(20),
            fake_worker_spawner(Arc::clone(&starts), Arc::clone(&fetches)),
        );
        let control = Arc::clone(&receiver.control);

        receiver.set_active(true);
        let first = receive_eventually(&mut receiver);
        assert_eq!(
            first.openai.unwrap().description,
            "1",
            "first active intent fetches exactly once"
        );
        receiver.set_active(true);
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert_eq!(fetches.load(Ordering::SeqCst), 1);
        while repaints.load(Ordering::SeqCst) == 0 {
            std::thread::yield_now();
        }

        receiver.set_active(false);
        let repaints_when_hidden = repaints.load(Ordering::SeqCst);
        wait_until_stopped(&control);
        receiver.reap_finished_worker();
        assert!(receiver.worker.is_none());
        assert_eq!(fetches.load(Ordering::SeqCst), 1);
        assert_eq!(repaints.load(Ordering::SeqCst), repaints_when_hidden);

        receiver.set_active(true);
        let second = receive_eventually(&mut receiver);
        assert_eq!(second.openai.unwrap().description, "2");
        assert_eq!(starts.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn refresh_intent는_latest_one으로_병합되고_inactive_repaint를_반복하지_않는다() {
        let starts = Arc::new(AtomicUsize::new(0));
        let fetches = Arc::new(AtomicUsize::new(0));
        let repaints = Arc::new(AtomicUsize::new(0));
        let ctx = egui::Context::default();
        let repaint_counter = Arc::clone(&repaints);
        ctx.set_request_repaint_callback(move |_| {
            repaint_counter.fetch_add(1, Ordering::SeqCst);
        });
        let (mut receiver, refresh) = new_with_spawner(
            ctx,
            Duration::from_millis(20),
            fake_worker_spawner(Arc::clone(&starts), Arc::clone(&fetches)),
        );
        let control = Arc::clone(&receiver.control);

        refresh.send(()).unwrap();
        refresh.send(()).unwrap();
        assert_eq!(starts.load(Ordering::SeqCst), 0);
        assert_eq!(repaints.load(Ordering::SeqCst), 1);
        let snapshot = receive_eventually(&mut receiver);
        assert_eq!(snapshot.openai.unwrap().description, "1");
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert_eq!(fetches.load(Ordering::SeqCst), 1);
        wait_until_stopped(&control);
        receiver.reap_finished_worker();
        assert_eq!(repaints.load(Ordering::SeqCst), 1);
        assert!(receiver.worker.is_none());
    }

    #[test]
    fn idle_expiry는_return전에_stopped를게시해_reactivation을보존한다() {
        let control = WorkerControl::default();
        control.set_active(true);
        assert!(control.reserve_start());
        control.set_active(false);

        assert!(matches!(
            control.wait_for_work(Duration::from_secs(60), Duration::from_millis(1)),
            WorkerWake::IdleExpired
        ));
        assert!(
            !control
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .running
        );

        control.set_active(true);
        assert!(control.start_needed());
    }

    #[test]
    fn initial_cycle취소는_return전에_stopped를게시해_reactivation을보존한다() {
        let control = WorkerControl::default();
        control.set_active(true);
        assert!(control.reserve_start());
        control.set_active(false);

        assert!(control.take_initial_cycle().is_none());
        assert!(
            !control
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .running
        );

        control.set_active(true);
        assert!(control.start_needed());
    }

    #[test]
    fn 공지_자동_조회_주기는_4시간이다() {
        assert_eq!(INCIDENTS_INTERVAL, Duration::from_secs(4 * 60 * 60));
    }

    #[test]
    fn http_body_reader는_exact_byte_cap만_허용한다() {
        let exact = vec![b'a'; STATUS_RESPONSE_MAX_BYTES];
        assert_eq!(
            read_utf8_limited(std::io::Cursor::new(exact.clone()), exact.len()).unwrap(),
            String::from_utf8(exact).unwrap()
        );
        assert!(
            read_utf8_limited(
                std::io::Cursor::new(vec![b'b'; STATUS_RESPONSE_MAX_BYTES + 1]),
                STATUS_RESPONSE_MAX_BYTES,
            )
            .is_err()
        );
        assert!(read_utf8_limited(std::io::Cursor::new(vec![0xff]), 1).is_err());
    }

    #[test]
    fn read_state는_rotation뒤에도_item과_byte_cap을_유지한다() {
        let mut state = NoticeReadState::default();
        for index in 0..2_000 {
            let feed = StatusFeedSnapshot {
                openai: Some(test_provider(&[&format!("https://status.test/{index}")])),
                ..StatusFeedSnapshot::default()
            };
            assert!(state.reconcile(&feed, true));
        }

        assert!(state.read_ids.len() <= NOTICE_READ_IDS_MAX_ITEMS);
        assert_eq!(state.read_order.len(), state.read_ids.len());
        assert!(state.read_bytes <= NOTICE_READ_IDS_MAX_BYTES);
        assert!(!state.read_ids.contains("OpenAI\u{1f}https://status.test/0"));
        assert!(
            state
                .read_ids
                .contains("OpenAI\u{1f}https://status.test/1999")
        );
        let debug = format!("{state:?}");
        assert!(!debug.contains("status.test"));
    }

    #[test]
    fn oversized_read_state_file은_deserialize전에_거부한다() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "deppy-notice-read-oversized-{}-{unique}.json",
            std::process::id()
        ));
        let file = std::fs::File::create(&path).unwrap();
        file.set_len((NOTICE_READ_STATE_FILE_MAX_BYTES + 1) as u64)
            .unwrap();
        assert!(NoticeReadState::load(&path).is_err());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn 첫_성공_목록은_기준화하고_그_다음_url만_새_공지로_센다() {
        let mut state = NoticeReadState::default();
        let mut feed = StatusFeedSnapshot {
            openai: Some(test_provider(&["https://status.openai.com/old"])),
            ..StatusFeedSnapshot::default()
        };

        assert!(state.reconcile(&feed, false));
        assert_eq!(state.unread_count(&feed), 0, "첫 조회는 기존 공지 기준");

        feed.openai = Some(test_provider(&[
            "https://status.openai.com/new",
            "https://status.openai.com/old",
        ]));
        assert!(!state.reconcile(&feed, false));
        assert_eq!(state.unread_count(&feed), 1);

        assert!(state.reconcile(&feed, true));
        assert_eq!(
            state.unread_count(&feed),
            0,
            "Home을 열면 현재 공지를 읽음 처리"
        );
    }

    #[test]
    fn 늦게_처음_성공한_공급자도_과거_공지를_새_알림으로_만들지_않는다() {
        let mut state = NoticeReadState::default();
        let mut feed = StatusFeedSnapshot {
            openai: Some(test_provider(&["openai-old"])),
            ..StatusFeedSnapshot::default()
        };
        state.reconcile(&feed, false);

        feed.claude = Some(test_provider(&["claude-old"]));
        assert!(state.reconcile(&feed, false));
        assert_eq!(state.unread_count(&feed), 0);
    }

    #[test]
    fn 읽음_url은_재시작_뒤에도_유지되고_목록_재진입을_다시_세지_않는다() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "deppy-notice-read-state-{}-{unique}.json",
            std::process::id()
        ));
        let mut state = NoticeReadState::default();
        let feed = StatusFeedSnapshot {
            grok: Some(test_provider(&["grok-a", "grok-b"])),
            ..StatusFeedSnapshot::default()
        };
        state.reconcile(&feed, false);
        state.save(&path).unwrap();

        let loaded = NoticeReadState::load(&path).unwrap();
        assert_eq!(loaded.initialized_providers, state.initialized_providers);
        assert_eq!(loaded.read_ids, state.read_ids);
        assert_eq!(loaded.unread_count(&feed), 0);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn parse_status는_indicator와_설명을_뽑는다() {
        let json = r#"{"page":{"name":"Claude"},"status":{"indicator":"none","description":"All Systems Operational"}}"#;
        let (indicator, description) = parse_status(json).unwrap();
        assert_eq!(indicator, ServiceIndicator::Operational);
        assert_eq!(description, "All Systems Operational");
        let json = r#"{"status":{"indicator":"major","description":"Partial outage"}}"#;
        assert_eq!(parse_status(json).unwrap().0, ServiceIndicator::Major);
        // 미지의 indicator는 Unknown — 표시 쪽에서 회색 점.
        let json = r#"{"status":{"indicator":"weird","description":""}}"#;
        assert_eq!(parse_status(json).unwrap().0, ServiceIndicator::Unknown);

        let oversized = serde_json::json!({
            "status": {
                "indicator": "none",
                "description": "x".repeat(PROVIDER_DESCRIPTION_MAX_BYTES + 1)
            }
        });
        assert!(parse_status(&oversized.to_string()).is_err());
    }

    #[test]
    fn parse_incidents는_최신_5건과_링크_폴백을_처리한다() {
        // Claude형(shortlink 있음)과 OpenAI형(shortlink 없음) 5건 + 초과 1건.
        let json = r#"{"incidents":[
            {"name":"A","status":"resolved","created_at":"2026-07-17T18:32:32.629Z","updated_at":"2026-07-17T18:32:32.629Z","shortlink":"https://stspg.io/a"},
            {"name":"B","status":"investigating","created_at":"2026-07-17T06:47:54.909Z"},
            {"name":"C","status":"resolved","created_at":"2026-07-16T22:54:01Z","id":"abc123"},
            {"name":"D","status":"resolved","created_at":"2026-07-15T00:00:00Z"},
            {"name":"E","status":"resolved","created_at":"2026-07-14T00:00:00Z"},
            {"name":"F","status":"resolved","created_at":"2026-07-13T00:00:00Z"}
        ]}"#;
        let notices = parse_incidents(json, "https://status.openai.com").unwrap();
        assert_eq!(notices.len(), 5, "최신 5건만");
        assert_eq!(notices[0].url, "https://stspg.io/a");
        // UTC 18:32은 어느 시간대에서도 같은 날이거나 다음 날이다 — 머신 시간대에
        // 의존하지 않도록 날짜 형식만 확인하고, 변환 자체는 아래 순수 함수 테스트가 본다.
        assert_eq!(notices[0].date.len(), 10, "YYYY-MM-DD 형식");
        assert!(notices[0].date.starts_with("2026-07-1"));
        assert_eq!(
            notices[1].url, "https://status.openai.com",
            "shortlink·id 둘 다 없으면 베이스"
        );
        assert_eq!(
            notices[2].url, "https://status.openai.com/incidents/abc123",
            "shortlink 없으면 id로 조립"
        );

        let invalid = serde_json::json!({
            "incidents": [{
                "name": "x".repeat(NOTICE_TITLE_MAX_BYTES + 1),
                "status": "resolved",
                "shortlink": "https://status.test/oversized"
            }]
        });
        assert!(
            parse_incidents(&invalid.to_string(), "https://status.test")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn hugging_face_trending_models를_최신_5건으로_변환한다() {
        let json = r#"[
            {"id":"org/model-a","createdAt":"2026-07-14T13:23:14.000Z"},
            {"modelId":"org/model-b","createdAt":"2026-07-13T00:00:00.000Z"},
            {"id":"org/model-c","createdAt":"2026-07-12T00:00:00.000Z"},
            {"id":"org/model-d","createdAt":"2026-07-11T00:00:00.000Z"},
            {"id":"org/model-e","createdAt":"2026-07-10T00:00:00.000Z"},
            {"id":"org/model-f","createdAt":"2026-07-09T00:00:00.000Z"}
        ]"#;
        let notices = parse_hugging_face_models(json).unwrap();
        assert_eq!(notices.len(), 5);
        assert_eq!(notices[0].title, "org/model-a");
        assert_eq!(notices[0].url, "https://huggingface.co/org/model-a");
        assert_eq!(notices[0].status, "trending");
        assert_eq!(notices[0].date, "2026-07-14");
    }

    #[test]
    fn grok_공식_rss를_중복없이_최신_5건으로_변환한다() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <rss version="2.0"><channel>
          <item><title><![CDATA[Grok Web unavailable]]></title><link>https://status.x.ai/incidents/one</link><pubDate>Mon, 20 Jul 2026 10:20:00 +0000</pubDate></item>
          <item><title>API latency</title><link>https://status.x.ai/incidents/two</link><pubDate>2026-07-19T08:00:00Z</pubDate></item>
          <item><title> api   LATENCY </title><link>https://status.x.ai/incidents/two-duplicate</link><pubDate>2026-07-19T07:00:00Z</pubDate></item>
          <item><title>Grok in X</title><link>https://status.x.ai/incidents/three</link><pubDate>Sat, 18 Jul 2026 03:00:00 +0000</pubDate></item>
          <item><title>Older incident</title><link>https://status.x.ai/incidents/four</link><pubDate>Fri, 17 Jul 2026 03:00:00 +0000</pubDate></item>
          <item><title>Account issue</title><link>https://status.x.ai/incidents/five</link><pubDate>Thu, 16 Jul 2026 03:00:00 +0000</pubDate></item>
          <item><title>Developer API</title><link>https://status.x.ai/incidents/six</link><pubDate>Wed, 15 Jul 2026 03:00:00 +0000</pubDate></item>
        </channel></rss>"#;
        let notices = parse_grok_status_rss(xml).unwrap();
        assert_eq!(notices.len(), 5);
        assert_eq!(notices[0].title, "Grok Web unavailable");
        assert_eq!(notices[0].url, "https://status.x.ai/incidents/one");
        assert_eq!(notices[0].status, "update");
        assert_eq!(notices[0].date, "2026-07-20");
        assert_eq!(notices[1].date, "2026-07-19");
        assert_eq!(notices[4].title, "Account issue");
        assert_eq!(
            notices.iter().filter(|n| n.title == "API latency").count(),
            1
        );
    }

    #[test]
    fn grok_rss는_빈_제목과_링크를_제외한다() {
        let xml = r#"<rss><channel>
          <item><title></title><link>https://status.x.ai/incidents/one</link></item>
          <item><title>Valid</title><link></link></item>
          <item><title>Visible</title><link>https://status.x.ai/incidents/three</link></item>
        </channel></rss>"#;
        let notices = parse_grok_status_rss(xml).unwrap();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].title, "Visible");
        assert!(notices[0].date.is_empty());
    }

    #[test]
    fn production_fetch_source에는_unbounded_into_string이_없다() {
        let source = include_str!("status_feed.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("production source");
        assert!(!production.contains(".into_string()"));
        assert!(production.contains("read_response_limited"));
        assert!(production.contains("NOTICE_READ_IDS_MAX_ITEMS"));
        assert!(production.contains("NOTICE_READ_IDS_MAX_BYTES"));
        assert!(!production.contains(&["{", "error:#}"].concat()));
        assert!(!production.contains("error_kind ="));
    }

    /// 2026-08-08: 공지 날짜를 ISO 문자열 앞 10글자로 잘라 쓰다가, UTC 15시 이후
    /// 사건이 KST 기준 다음 날인데도 전날로 찍혔다. 상태 페이지엔 8월 6일로 보이는
    /// 항목이 앱에선 8월 5일이었다. 파싱·환산은 순수 함수라 머신 시간대와 무관하게 본다.
    #[test]
    fn 타임스탬프는_iso와_rfc822를_모두_unix초로_읽는다() {
        // 사용자가 지목한 그 incident의 실제 값.
        let iso = parse_timestamp_utc_secs("2026-08-05T23:22:45Z").expect("ISO");
        let rfc = parse_timestamp_utc_secs("Wed, 05 Aug 2026 23:22:45 GMT").expect("RFC 822");
        assert_eq!(
            iso, rfc,
            "같은 시각은 형식이 달라도 같은 값이어야 정렬이 맞는다"
        );
        assert_eq!(civil_date(iso), "2026-08-05", "UTC로는 8월 5일");

        // KST(+9)로 환산하면 8월 6일 — 사용자가 상태 페이지에서 본 그 날짜.
        assert_eq!(civil_date(iso + 9 * 3_600), "2026-08-06");
        // 반대로 하와이(-10)에서는 여전히 8월 5일이라야 한다.
        assert_eq!(civil_date(iso - 10 * 3_600), "2026-08-05");

        assert_eq!(
            parse_timestamp_utc_secs("2026-08-05T13:51:30.287Z"),
            parse_timestamp_utc_secs("2026-08-05T13:51:30Z"),
            "소수점 이하 초는 날짜에 영향을 주지 않는다"
        );
        for broken in ["", "not a date", "2026-13-40T00:00:00Z", "Xyz, 99 Zzz 2026"] {
            assert_eq!(parse_timestamp_utc_secs(broken), None, "{broken}");
            assert_eq!(local_date(broken), "", "{broken}: 실패는 빈 문자열");
        }
    }

    /// civil_to_unix ↔ civil_date 왕복. 윤년·세기·연말 경계에서 깨지기 쉬운 부분이다.
    #[test]
    fn 날짜_변환은_윤년과_경계에서_왕복한다() {
        for (year, month, day) in [
            (1970, 1, 1),
            (2000, 2, 29),
            (2024, 2, 29),
            (2026, 12, 31),
            (2100, 3, 1),
        ] {
            let secs = civil_to_unix(year, month, day, 0, 0, 0).expect("유효한 날짜");
            assert_eq!(civil_date(secs), format!("{year:04}-{month:02}-{day:02}"));
        }
        assert_eq!(civil_to_unix(1970, 1, 1, 0, 0, 0), Some(0), "epoch");
        assert_eq!(civil_to_unix(2026, 0, 1, 0, 0, 0), None, "0월은 없다");
        assert_eq!(civil_to_unix(2026, 1, 1, 24, 0, 0), None, "24시는 없다");
    }

    /// 공급자마다 원본 형식이 다른데(ISO vs RFC 822) 홈은 날짜 문자열로 정렬한다.
    /// 같은 포맷터를 지나야 문자열 비교가 실제 시간순과 일치한다.
    #[test]
    fn 서로_다른_형식도_같은_날짜_문자열로_정렬_가능해진다() {
        let iso = local_date("2026-08-05T23:22:45Z");
        let rfc = local_date("Wed, 05 Aug 2026 23:22:45 GMT");
        assert_eq!(iso, rfc);
        assert_eq!(iso.len(), 10);

        let older = local_date("2026-07-30T16:01:11Z");
        assert!(
            older < iso,
            "문자열 비교가 시간순과 일치해야 한다: {older} < {iso}"
        );
    }
}
