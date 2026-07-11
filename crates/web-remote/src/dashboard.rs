//! 대시보드 브리지 (계획 v3.3 P2 — 킬러 기능의 데이터 계층).
//!
//! 전용 스레드 하나가 두 소스를 모아 접속 WS가 읽을 스냅샷을 발행한다:
//!   1. **활성 workspace worker 이벤트** — `subscribe_with_wake`로 구독한
//!      [`RuntimeEventReceiver`]를 drain해 세션 상태·리소스 맵을 갱신한다. wake 클로저가
//!      이 스레드를 깨우므로 **egui 프레임과 무관하게**(창이 숨겨져 리페인트가 멈춰도)
//!      상태가 흐른다. (§14.1 Warm 알림 유지 관례)
//!   2. **승인 DB 직행** — 웹 계층 자체 [`storage::Db`] 연결로 pending 승인을 1초 폴링하고,
//!      Allow/Deny를 `resolve_approval`로 되쓴다. proxy↔GUI 공유 DB IPC 관례(계획 §0.2).
//!
//! 리소스 규율:
//!   - **OFF**: 서버 미생성 → 이 스레드 없음(스레드/타이머 0).
//!   - **ON + 접속 0**: 스레드는 cvar에 park한다. 런타임 이벤트에만 깨어 경량 drain 후 재-park
//!     하며(네트워크·DB 접근 없음), **승인 DB 폴링은 완전히 정지**(타이머 없음). 유휴
//!     (이벤트 없음)엔 park 상태라 CPU 0. 유일한 상시 웨이크는 주기 리소스 샘플(~1Hz)의
//!     no-op drain으로 측정 불가 수준.
//!   - **접속 ≥1**: 1초 주기로 승인 폴링 + 이벤트 drain → 스냅샷 발행(변화 시 버전 증가).
//!
//! 스레드 경계: 브리지 스레드는 **소켓을 만지지 않는다**. 발행된 JSON 스냅샷(`published`)만
//! 갱신하고, 접속 스레드([`crate::ws_api`])가 자기 tick에서 버전을 비교해 push한다 — worker
//! 의 wake 콜백이 네트워크 I/O에 블록되지 않도록 계층을 가른다.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use runtime::{
    ProcessResourceSnapshot, RuntimeCommand, RuntimeEvent, RuntimeEventReceiver, SessionId,
    SessionStatus,
};

use crate::protocol::{ApprovalView, ResourceView, ServerMsg, SessionView};

/// 승인 DB 폴링 주기 — 접속이 있을 때만 적용된다(계획 완료기준: 상태/승인 반영 ≤1s).
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// 원격 시청 lease TTL(ms) — runtime SetRemoteViewing에 싣는다 (P5b). 갱신 주기의
/// 3배로 두어 tick 지연·일시 정체에도 시청이 끊기지 않게 한다 (runtime 상한 5분 이내).
const LEASE_TTL_MS: u32 = 45_000;
/// lease 갱신 주기 — 브리지 tick(접속 ≥1이면 ≤1s)마다 만기를 검사해 재전송한다.
const LEASE_RENEW_INTERVAL: Duration = Duration::from_secs(15);

/// web → runtime 명령 싱크 (P5b). 앱이 활성 runtime의 command_sink를 주입하고,
/// 워크스페이스 전환 시 receiver와 함께 교체한다. fire-and-forget — 실패는 로그만.
pub type CommandSink = Arc<dyn Fn(RuntimeCommand) + Send + Sync>;

/// 세션 하나의 시청 집계 (P5b) — 접속 수 + 마지막 lease 갱신 시각.
struct WatcherEntry {
    count: usize,
    last_renewal: Instant,
}

/// 갱신 주기가 지난 시청 세션들을 골라 last_renewal을 갱신한다 (순수 — 테스트 대상).
fn due_lease_renewals(
    watchers: &mut BTreeMap<u64, WatcherEntry>,
    now: Instant,
    interval: Duration,
) -> Vec<u64> {
    watchers
        .iter_mut()
        .filter(|(_, entry)| now.duration_since(entry.last_renewal) >= interval)
        .map(|(id, entry)| {
            entry.last_renewal = now;
            *id
        })
        .collect()
}

/// 시청 lease 명령을 만든다.
fn lease_command(session: u64, viewing: bool) -> RuntimeCommand {
    RuntimeCommand::SetRemoteViewing {
        session: SessionId(session),
        viewing,
        ttl_ms: if viewing { LEASE_TTL_MS } else { 0 },
    }
}

/// 세션 한 행의 경량 상태(런타임 이벤트에서 누적). 표시용 최소 필드만 유지한다.
#[derive(Debug, Clone, PartialEq)]
struct SessionEntry {
    title: String,
    status: SessionStatus,
    /// SessionExited/Restored 관측 — 완료 배지.
    exited: bool,
}

/// 앱이 웹 대시보드에 넘기는 세션 시드 한 행. 재구독(start_web·워크스페이스 전환) 직후,
/// 이벤트 이력이 없는 새 구독자가 needs_approval 등 이미 정착한 상태를 즉시 반영하도록
/// 앱이 GUI 배지용으로 추적 중인 현재 상태를 담아 넘긴다(계획 P2 리뷰: edge-trigger 유실 보정).
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSeed {
    pub id: u64,
    pub title: String,
    pub status: SessionStatus,
    pub exited: bool,
}

/// [`SessionStatus`] → snake_case 문자열(브라우저 프로토콜).
fn status_str(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::Running => "running",
        SessionStatus::Waiting => "waiting",
        SessionStatus::NeedsApproval => "needs_approval",
        SessionStatus::Idle => "idle",
        SessionStatus::Error => "error",
        SessionStatus::Done => "done",
    }
}

/// 런타임 이벤트 하나를 세션 맵/리소스에 반영한다(순수 — 테스트 용이).
///
/// 대시보드 범위는 **활성 workspace worker의 세션**이다(계획 P2 미결정: 비활성 workspace
/// 메타데이터는 후속). MuxUpdated가 세션 집합의 근거(pane에 붙은 것만 유지), 상태 이벤트가
/// 상태를 덮는다. 종료된 세션도 pane이 남아 있는 동안은(완료 배지) 목록에 둔다.
fn apply_event(
    sessions: &mut BTreeMap<u64, SessionEntry>,
    resource: &mut Option<ResourceView>,
    event: &RuntimeEvent,
) {
    match event {
        RuntimeEvent::MuxUpdated { snapshot } => {
            // pane에 붙은 세션 집합으로 목록을 정렬(추가/삭제). 상태·exited는 보존하고
            // 제목만 최신화한다 — MuxUpdated가 상태를 리셋하지 않게.
            let mut live: BTreeMap<u64, String> = BTreeMap::new();
            for tab in &snapshot.tabs {
                for pane in &tab.panes {
                    if let Some(session) = pane.session_id {
                        live.insert(session.0, pane.title.clone());
                    }
                }
            }
            sessions.retain(|id, _| live.contains_key(id));
            for (id, title) in live {
                sessions
                    .entry(id)
                    .and_modify(|entry| entry.title = title.clone())
                    .or_insert(SessionEntry {
                        title,
                        status: SessionStatus::Running,
                        exited: false,
                    });
            }
        }
        RuntimeEvent::SessionStatusChanged { session, status } => {
            sessions.entry(session.0).or_insert_with(new_entry).status = *status;
        }
        RuntimeEvent::SessionStatusViewChanged { session, view } => {
            sessions.entry(session.0).or_insert_with(new_entry).status = view.status;
        }
        RuntimeEvent::SessionExited { session, .. }
        | RuntimeEvent::SessionRestored { session, .. } => {
            let entry = sessions.entry(session.0).or_insert_with(new_entry);
            entry.exited = true;
            entry.status = SessionStatus::Done;
        }
        RuntimeEvent::ShellSpawned { session } | RuntimeEvent::AgentSpawned { session } => {
            sessions.entry(session.0).or_insert_with(new_entry);
        }
        RuntimeEvent::ResourceUsage { snapshot, .. } => {
            *resource = Some(resource_view(snapshot));
        }
        // Viewport(터미널 뷰어=P5)·PtyInputPressure·SpawnFailed 등은 대시보드 비범위.
        _ => {}
    }
}

fn new_entry() -> SessionEntry {
    SessionEntry {
        title: String::new(),
        status: SessionStatus::Running,
        exited: false,
    }
}

/// 런타임 이벤트 하나를 웹푸시 싱크로 넘긴다(P4). 세션 상태 전이만 대상 — 실제 발송 여부는
/// push 계층이 상태(입력대기/완료)와 중복 억제로 결정한다. SessionRestored(재시작 복원)는
/// "완료"가 아니므로 넘기지 않는다 — 앱 재시작 때마다 완료 알림이 쏟아지는 것을 막는다.
fn forward_to_push(push: &crate::push::PushHandle, event: &RuntimeEvent) {
    match event {
        RuntimeEvent::SessionStatusChanged { session, status } => {
            push.notify_session(session.0, *status)
        }
        RuntimeEvent::SessionStatusViewChanged { session, view } => {
            push.notify_session(session.0, view.status)
        }
        RuntimeEvent::SessionExited { session, .. } => {
            push.notify_session(session.0, SessionStatus::Done)
        }
        _ => {}
    }
}

fn resource_view(snapshot: &ProcessResourceSnapshot) -> ResourceView {
    ResourceView {
        cpu: snapshot.cpu_percent,
        rss_mb: snapshot.rss_bytes / (1024 * 1024),
    }
}

/// 세션 맵을 정렬된 표시 목록으로. id 오름차순(안정적 렌더).
fn session_views(sessions: &BTreeMap<u64, SessionEntry>) -> Vec<SessionView> {
    sessions
        .iter()
        .map(|(id, entry)| SessionView {
            id: *id,
            title: entry.title.clone(),
            status: status_str(entry.status),
            exited: entry.exited,
        })
        .collect()
}

/// 브리지 스레드 전용 가변 상태(런타임 소스 + DB + 파생 맵).
struct Inner {
    /// 새 이벤트/명령 도착 플래그(웨이크가 세운다).
    dirty: bool,
    /// 접속 등록/resolve 직후 즉시 1회 폴링을 강제한다(주기와 무관).
    force_poll: bool,
    /// 활성 workspace worker 구독. 전환 시 [`DashboardHandle::set_runtime_source`]가 교체한다.
    receiver: Option<RuntimeEventReceiver>,
    sessions: BTreeMap<u64, SessionEntry>,
    resource: Option<ResourceView>,
    last_poll: Instant,
    /// 웹푸시 발송 싱크(P4). 이벤트 drain 시 세션 상태 전이(입력대기/완료)를 넘긴다 —
    /// 승인 발송은 push가 DB를 직접 폴링하므로 여기서 넘기지 않는다. None이면 푸시 비활성.
    push_sink: Option<crate::push::PushHandle>,
    /// web → runtime 명령 싱크 (P5b). None이면 시청 lease를 보내지 않는다(뷰어 비활성).
    command_sink: Option<CommandSink>,
    /// 세션별 시청 접속 집계 (P5b). 0→1에서 lease on, 1→0에서 lease off를 보낸다.
    watchers: BTreeMap<u64, WatcherEntry>,
}

/// 접속 스레드가 소켓으로 밀어낼 발행 스냅샷. 버전이 오르면 push 대상.
#[derive(Default)]
struct Published {
    dashboard_json: String,
    dash_version: u64,
    approvals_json: String,
    appr_version: u64,
    /// 시청 세션별 최신 화면 슬롯 (P5c) — (seq, 스냅샷). 최신본만 유지(coalesce,
    /// remote.rs 슬롯 관례). 시청이 끊기면 rebind_watch가 제거한다 — 메모리 유계.
    viewports: BTreeMap<u64, (u64, Arc<runtime::TerminalViewportSnapshot>)>,
}

struct Shared {
    inner: Mutex<Inner>,
    published: Mutex<Published>,
    /// 승인 대시보드용 자체 DB 연결(없으면 승인 목록은 빈 채로 상태만 흐른다). inner와 별도
    /// 락으로 두어 승인 되쓰기/폴링의 DB I/O(busy_timeout 최대 5s)가 이벤트 drain·등록이 쓰는
    /// inner 락을 잡은 채 진행되지 않게 한다(P3 리뷰). 락 순서는 inner→db 고정(교착 방지).
    db: Mutex<Option<storage::Db>>,
    cvar: Condvar,
    /// 인증까지 마친 라이브 대시보드 WS 수 — wake/타이머 게이트.
    connections: AtomicUsize,
    stop: AtomicBool,
    /// 승인 DB 폴링 횟수(테스트: 접속 0에서 폴링 정지 검증).
    poll_count: AtomicU64,
}

/// 브리지 핸들(복제 가능 — 서버·접속 스레드가 공유). Drop 순서 위험을 피하려 wake 클로저는
/// [`Weak`]만 쥔다: worker가 죽은 구독을 정리할 때까지 wake가 [`Shared`]를 붙잡지 않게 해
/// receiver drop → worker의 subscriber 정리가 막히지 않는다(계획 P2 "wake 클로저 수명").
#[derive(Clone)]
pub struct DashboardHandle {
    shared: Arc<Shared>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

/// 접속 1건의 수명 동안 연결 수를 +1로 유지하는 RAII 가드(Drop 시 -1 + 재평가 notify).
pub struct ConnectionGuard {
    shared: Arc<Shared>,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.shared.connections.fetch_sub(1, Ordering::SeqCst);
        // 0으로 떨어졌으면 브리지가 타이머를 접고 park하도록 깨운다.
        self.shared.cvar.notify_all();
    }
}

impl DashboardHandle {
    /// 브리지 스레드를 띄운다. `db_path`가 있으면 자체 DB 연결을 열어 승인 대시보드를
    /// 활성화한다(열기 실패는 로그만 — 상태 대시보드는 계속). 반환된 JoinHandle은 서버가
    /// 소유해 shutdown 시 join한다.
    pub fn spawn(db_path: Option<PathBuf>) -> (Self, JoinHandle<()>) {
        let db = db_path.and_then(|path| match storage::Db::open(&path) {
            Ok(db) => Some(db),
            Err(e) => {
                tracing::warn!("web-remote 승인 DB 열기 실패 — 승인 대시보드 비활성: {e:#}");
                None
            }
        });
        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner {
                dirty: false,
                force_poll: false,
                receiver: None,
                sessions: BTreeMap::new(),
                resource: None,
                last_poll: Instant::now() - POLL_INTERVAL,
                push_sink: None,
                command_sink: None,
                watchers: BTreeMap::new(),
            }),
            published: Mutex::new(Published::default()),
            db: Mutex::new(db),
            cvar: Condvar::new(),
            connections: AtomicUsize::new(0),
            stop: AtomicBool::new(false),
            poll_count: AtomicU64::new(0),
        });
        // wake는 Weak만 — 죽은 구독 정리를 막지 않는다(위 주석). 접속 0이어도 세워 두어야
        // 이벤트 drain으로 durable 큐 overflow를 막는다(연결 시 콜드스타트 회피).
        let weak: Weak<Shared> = Arc::downgrade(&shared);
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            if let Some(shared) = weak.upgrade() {
                let mut inner = shared.inner.lock().expect("dashboard inner lock");
                inner.dirty = true;
                shared.cvar.notify_all();
            }
        });
        let thread = {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("web-remote-dashboard".into())
                .spawn(move || run(&shared))
                .expect("web-remote 대시보드 스레드 생성")
        };
        (DashboardHandle { shared, wake }, thread)
    }

    /// `subscribe_with_wake`에 넘길 안정적 wake 클로저(호출마다 같은 Arc 복제).
    pub fn wake_fn(&self) -> Arc<dyn Fn() + Send + Sync> {
        Arc::clone(&self.wake)
    }

    /// 웹푸시 발송 싱크를 붙인다(P4 — 서버 기동 시 1회, 접속 전). 이후 이벤트 drain에서
    /// 세션 상태 전이(입력대기/완료)를 이 싱크로 넘긴다. 승인 발송은 push가 DB를 직접 폴링한다.
    pub fn set_push_sink(&self, push: crate::push::PushHandle) {
        let mut inner = self.shared.inner.lock().expect("dashboard inner lock");
        inner.push_sink = Some(push);
    }

    /// 활성 workspace worker 구독을 붙인다(시작 + 워크스페이스 전환마다). 옛 receiver는
    /// 교체와 함께 drop되어 옛 worker가 자기 subscriber를 정리한다. 시청 lease는 새
    /// worker가 모르므로 재선언한다 (P5b — runtime 재시작/전환 시 시청 연속성).
    pub fn set_runtime_source(&self, receiver: RuntimeEventReceiver) {
        {
            let mut inner = self.shared.inner.lock().expect("dashboard inner lock");
            inner.receiver = Some(receiver);
            inner.dirty = true;
        }
        self.reassert_watch_leases();
        self.shared.cvar.notify_all();
    }

    /// web → runtime 명령 싱크를 붙인다 (P5b — receiver와 같은 시점에 교체된다).
    pub fn set_command_sink(&self, sink: CommandSink) {
        let mut inner = self.shared.inner.lock().expect("dashboard inner lock");
        inner.command_sink = Some(sink);
    }

    /// 접속의 시청 대상 전환 (P5b). `from`을 내리고 `to`를 올린다 — refcount 전이
    /// (0→1 / 1→0)에서만 runtime lease 명령이 나간다. 명령 전송은 inner 락 밖에서.
    pub fn rebind_watch(&self, from: Option<u64>, to: Option<u64>) {
        if from == to {
            return;
        }
        let mut commands: Vec<RuntimeCommand> = Vec::new();
        let mut drop_slot: Option<u64> = None;
        let sink = {
            let mut inner = self.shared.inner.lock().expect("dashboard inner lock");
            if let Some(old) = from
                && let Some(entry) = inner.watchers.get_mut(&old)
            {
                entry.count = entry.count.saturating_sub(1);
                if entry.count == 0 {
                    inner.watchers.remove(&old);
                    commands.push(lease_command(old, false));
                    drop_slot = Some(old);
                }
            }
            if let Some(new) = to {
                let entry = inner.watchers.entry(new).or_insert(WatcherEntry {
                    count: 0,
                    last_renewal: Instant::now(),
                });
                entry.count += 1;
                if entry.count == 1 {
                    entry.last_renewal = Instant::now();
                    commands.push(lease_command(new, true));
                }
            }
            inner.command_sink.clone()
        };
        // 마지막 시청자가 떠난 세션의 화면 슬롯 제거 (P5c — trailing 스냅샷이 남지 않게).
        // inner 락을 놓은 뒤 published만 잠근다 (락 순서 준수).
        if let Some(old) = drop_slot {
            self.shared
                .published
                .lock()
                .expect("published lock")
                .viewports
                .remove(&old);
        }
        if let Some(sink) = sink {
            for command in commands {
                sink(command);
            }
        }
    }

    /// 시청 중인 모든 세션의 lease를 재선언한다 (P5b — 새 worker 구독 직후). 새 worker는
    /// 이전 lease를 모르므로 viewing=true를 다시 보내고 갱신 시계를 리셋한다.
    fn reassert_watch_leases(&self) {
        let (sink, sessions) = {
            let mut inner = self.shared.inner.lock().expect("dashboard inner lock");
            let now = Instant::now();
            let sessions: Vec<u64> = inner
                .watchers
                .iter_mut()
                .map(|(id, entry)| {
                    entry.last_renewal = now;
                    *id
                })
                .collect();
            (inner.command_sink.clone(), sessions)
        };
        if let Some(sink) = sink {
            for session in sessions {
                sink(lease_command(session, true));
            }
        }
    }

    /// 현재 워크스페이스의 세션 목록/상태/제목/exited를 시드한다(start_web·워크스페이스 전환 시,
    /// 구독 등록 직후 호출). 세션 맵을 **통째로 교체**해, `SetWorkspaceState(Active)`(비동기)와
    /// 구독 등록(동기) 사이 레이스로 MuxUpdated 재발화를 놓쳐도 옛 워크스페이스 세션이 남지
    /// 않게 한다. 이후 도착하는 이벤트는 증분 갱신이며, MuxUpdated의 or_insert는 시드된 항목을
    /// and_modify(제목만)로 건드리므로 시드된 상태를 기본값(Running)으로 덮어쓰지 않는다.
    pub fn seed_sessions(&self, seeds: Vec<SessionSeed>) {
        {
            let mut inner = self.shared.inner.lock().expect("dashboard inner lock");
            inner.sessions = seeds
                .into_iter()
                .map(|seed| {
                    (
                        seed.id,
                        SessionEntry {
                            title: seed.title,
                            status: seed.status,
                            exited: seed.exited,
                        },
                    )
                })
                .collect();
            inner.dirty = true;
        }
        self.shared.cvar.notify_all();
    }

    /// 인증 완료 접속을 등록한다 — 연결 수 +1, 즉시 폴링 강제. Drop 시 자동 -1.
    pub fn register_connection(&self) -> ConnectionGuard {
        self.shared.connections.fetch_add(1, Ordering::SeqCst);
        {
            let mut inner = self.shared.inner.lock().expect("dashboard inner lock");
            inner.force_poll = true;
            inner.dirty = true;
        }
        self.shared.cvar.notify_all();
        ConnectionGuard {
            shared: Arc::clone(&self.shared),
        }
    }

    /// 승인 결정을 DB에 되쓴다(first-writer-wins — 이미 해소된 id는 조용한 no-op). 이후
    /// 즉시 재폴링을 강제해 목록에서 사라진 걸 빠르게 반영한다.
    pub fn resolve(&self, id: &str, allowed: bool, remember: bool) {
        let now = epoch_secs();
        // 재폴링 강제 플래그만 inner에서 세우고 즉시 놓는다 — DB 되쓰기(busy_timeout 최대 5s)를
        // inner 락 밖에서 수행해 브리지의 이벤트 drain·접속 등록이 막히지 않게 한다(P3 리뷰).
        {
            let mut inner = self.shared.inner.lock().expect("dashboard inner lock");
            inner.force_poll = true;
            inner.dirty = true;
        }
        // DB 되쓰기는 전용 db 락으로(inner 미보유 — 락 순서 inner→db 준수). 그 뒤 브리지를 깨워
        // 재폴링시켜 목록에서 사라진 걸 빠르게 반영한다(first-writer-wins — 해소된 id는 no-op).
        if let Some(db) = self.shared.db.lock().expect("dashboard db lock").as_ref()
            && let Err(e) = db.resolve_approval(id, allowed, remember, now)
        {
            tracing::warn!("web-remote 승인 resolve 실패: {e:#}");
        }
        self.shared.cvar.notify_all();
    }

    /// `last`보다 새 대시보드 스냅샷이 있을 때만 (버전, JSON)을 돌려준다 — tick마다 불필요한
    /// 문자열 clone을 피한다(접속 유휴 CPU 절감).
    pub fn dashboard_if_newer(&self, last: u64) -> Option<(u64, String)> {
        let published = self.shared.published.lock().expect("published lock");
        (published.dash_version > last)
            .then(|| (published.dash_version, published.dashboard_json.clone()))
    }

    /// `last`보다 새 승인 스냅샷이 있을 때만 (버전, JSON)을 돌려준다.
    pub fn approvals_if_newer(&self, last: u64) -> Option<(u64, String)> {
        let published = self.shared.published.lock().expect("published lock");
        (published.appr_version > last)
            .then(|| (published.appr_version, published.approvals_json.clone()))
    }

    /// 시청 세션의 화면 슬롯이 `last_seq`보다 새로우면 (seq, 스냅샷)을 돌려준다 (P5c).
    /// Arc 복제라 싸다 — 인코딩(keyframe/delta)은 접속 스레드가 자기 baseline으로 한다.
    pub fn viewport_if_newer(
        &self,
        session: u64,
        last_seq: u64,
    ) -> Option<(u64, Arc<runtime::TerminalViewportSnapshot>)> {
        let published = self.shared.published.lock().expect("published lock");
        published
            .viewports
            .get(&session)
            .filter(|(seq, _)| *seq > last_seq)
            .map(|(seq, snapshot)| (*seq, Arc::clone(snapshot)))
    }

    /// 지금까지의 승인 DB 폴링 횟수(테스트).
    pub fn poll_count(&self) -> u64 {
        self.shared.poll_count.load(Ordering::SeqCst)
    }

    /// 브리지 스레드에 종료를 알린다(서버 shutdown이 join 전에 호출).
    pub fn stop(&self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.shared.cvar.notify_all();
    }

    /// 테스트 전용: 런타임 receiver 없이 이벤트 하나를 세션 맵에 반영하고 브리지를 깨운다
    /// (상태 스트림 프레임을 실제 WS로 검증하기 위한 주입 시드). Viewport 이벤트는 run()의
    /// drain과 동일하게 시청 중일 때만 슬롯에 반영한다 (P5c).
    #[cfg(test)]
    pub fn inject_event(&self, event: RuntimeEvent) {
        let staged = {
            let mut inner = self.shared.inner.lock().expect("dashboard inner lock");
            let Inner {
                sessions,
                resource,
                watchers,
                ..
            } = &mut *inner;
            apply_event(sessions, resource, &event);
            let staged = match &event {
                RuntimeEvent::Viewport {
                    session, snapshot, ..
                } if watchers.contains_key(&session.0) => Some((session.0, Arc::clone(snapshot))),
                _ => None,
            };
            inner.dirty = true;
            staged
        };
        if let Some((session, snapshot)) = staged {
            let mut published = self.shared.published.lock().expect("published lock");
            let entry = published
                .viewports
                .entry(session)
                .or_insert((0, Arc::clone(&snapshot)));
            entry.0 += 1;
            entry.1 = snapshot;
        }
        self.shared.cvar.notify_all();
    }
}

/// 현재 epoch 초.
fn epoch_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 승인 행을 표시 뷰로. `arguments_preview`는 proxy가 이미 redact한 텍스트다.
fn approval_views(rows: Vec<storage::PendingApprovalRow>) -> Vec<ApprovalView> {
    rows.into_iter()
        .map(|row| ApprovalView {
            id: row.id,
            server: row.server_id,
            tool: row.tool_name,
            preview: row.arguments_preview,
            created_at: row.created_at,
        })
        .collect()
}

/// 브리지 스레드 본체. cvar 대기 → drain/폴링 → 스냅샷 발행을 반복한다.
fn run(shared: &Arc<Shared>) {
    loop {
        let mut inner = shared.inner.lock().expect("dashboard inner lock");
        // 대기 단계: stop/dirty/폴링 만기 중 하나가 올 때까지. 접속 0이면 타이머 없이 park.
        loop {
            if shared.stop.load(Ordering::SeqCst) {
                return;
            }
            let conns = shared.connections.load(Ordering::SeqCst);
            let poll_due =
                conns > 0 && (inner.force_poll || inner.last_poll.elapsed() >= POLL_INTERVAL);
            if inner.dirty || poll_due {
                break;
            }
            if conns > 0 {
                // 다음 폴링 만기까지만 잔다(승인 ≤1s 반영).
                let wait = POLL_INTERVAL.saturating_sub(inner.last_poll.elapsed());
                let (guard, _) = shared
                    .cvar
                    .wait_timeout(inner, wait)
                    .expect("dashboard cvar wait");
                inner = guard;
            } else {
                // 접속 0 — 런타임 wake/등록/stop이 깨울 때까지 무기한 park(타이머 없음 → CPU 0).
                inner = shared.cvar.wait(inner).expect("dashboard cvar wait");
            }
        }
        inner.dirty = false;

        // 1) 런타임 이벤트 drain — 접속 유무와 무관하게 처리해 durable 큐 overflow를 막는다.
        //    시청 중 세션의 Viewport는 스테이징해 두었다가 inner 락을 놓은 뒤 슬롯에 반영한다
        //    (P5c — 세션별 최신본만, coalesce).
        let mut staged_viewports: BTreeMap<u64, Arc<runtime::TerminalViewportSnapshot>> =
            BTreeMap::new();
        if let Some(receiver) = inner.receiver.as_ref() {
            let events = receiver.drain();
            if receiver.take_overflowed() {
                tracing::warn!(
                    "web-remote 대시보드 이벤트 큐 overflow — 상태가 잠시 뒤처질 수 있음"
                );
            }
            let Inner {
                sessions,
                resource,
                push_sink,
                watchers,
                ..
            } = &mut *inner;
            for event in &events {
                apply_event(sessions, resource, event);
                // 세션 상태 전이(입력대기/완료)를 웹푸시로 넘긴다 — 앱이 닫혀 있어도 알린다(P4).
                // notify_session이 Done/Waiting 외 상태는 무시하므로 여기서는 걸러내지 않는다.
                if let Some(push) = push_sink.as_ref() {
                    forward_to_push(push, event);
                }
                if let RuntimeEvent::Viewport {
                    session, snapshot, ..
                } = event
                    && watchers.contains_key(&session.0)
                {
                    staged_viewports.insert(session.0, Arc::clone(snapshot));
                }
            }
        }

        // 2) 승인 폴링 — 접속 ≥1 + (강제 or 주기 만기)에서만. 접속 0이면 완전 정지.
        let conns = shared.connections.load(Ordering::SeqCst);
        let should_poll =
            conns > 0 && (inner.force_poll || inner.last_poll.elapsed() >= POLL_INTERVAL);
        inner.force_poll = false;
        let mut approvals: Option<Vec<ApprovalView>> = None;
        if should_poll {
            inner.last_poll = Instant::now();
            shared.poll_count.fetch_add(1, Ordering::SeqCst);
            // db 락을 inner 밑에 중첩 취득한다(락 순서 inner→db). resolve는 inner를 놓고서만
            // db를 잡으므로 교착이 생기지 않는다.
            match shared.db.lock().expect("dashboard db lock").as_ref() {
                Some(db) => match db.list_pending_approvals() {
                    Ok(rows) => approvals = Some(approval_views(rows)),
                    Err(e) => tracing::warn!("web-remote 승인 목록 폴링 실패: {e:#}"),
                },
                None => approvals = Some(Vec::new()),
            }
        }

        // 2.5) 원격 시청 lease 갱신 (P5b) — 만기(45s TTL) 전에 재전송해 시청을 유지한다.
        //      시청 0이면 no-op. 전송은 inner 락을 놓은 뒤에.
        let renewals =
            due_lease_renewals(&mut inner.watchers, Instant::now(), LEASE_RENEW_INTERVAL);
        let renewal_sink = (!renewals.is_empty())
            .then(|| inner.command_sink.clone())
            .flatten();

        // 3) 발행 — 접속 0이면 JSON을 만들지 않는다(불필요 작업 회피). 접속 시 등록이
        //    force_poll+dirty를 세우므로 그때 최신 스냅샷이 만들어진다.
        if conns > 0 {
            let dash_json = ServerMsg::Dashboard {
                sessions: session_views(&inner.sessions),
                resource: inner.resource.clone(),
            }
            .encode();
            let appr_json = approvals.map(|pending| ServerMsg::Approvals { pending }.encode());
            drop(inner);
            publish(shared, dash_json, appr_json);
        } else {
            drop(inner);
        }
        // 시청 화면 슬롯 반영 (P5c) — inner 락 없이 published만 잠근다 (락 순서 준수).
        if !staged_viewports.is_empty() {
            let mut published = shared.published.lock().expect("published lock");
            for (session, snapshot) in staged_viewports {
                let entry = published
                    .viewports
                    .entry(session)
                    .or_insert((0, Arc::clone(&snapshot)));
                entry.0 += 1;
                entry.1 = snapshot;
            }
        }
        if let Some(sink) = renewal_sink {
            for session in renewals {
                sink(lease_command(session, true));
            }
        }
    }
}

/// 발행 스냅샷을 갱신한다 — 내용이 바뀐 것만 버전을 올려 접속 스레드가 재전송하게 한다.
fn publish(shared: &Arc<Shared>, dashboard_json: String, approvals_json: Option<String>) {
    let mut published = shared.published.lock().expect("published lock");
    if published.dashboard_json != dashboard_json {
        published.dashboard_json = dashboard_json;
        published.dash_version += 1;
    }
    if let Some(json) = approvals_json
        && published.approvals_json != json
    {
        published.approvals_json = json;
        published.appr_version += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use runtime::{MuxSnapshot, PaneSnapshot, SessionId, TabSnapshot};
    use std::sync::Arc as StdArc;

    fn mux_event(panes: &[(u64, &str)]) -> RuntimeEvent {
        // mux id는 String UUID 계열(core uuid_id!) — 테스트에선 세션 id를 문자열로 재사용.
        RuntimeEvent::MuxUpdated {
            snapshot: StdArc::new(MuxSnapshot {
                tabs: vec![TabSnapshot {
                    id: runtime::MuxTabId("tab-1".into()),
                    title: "tab".into(),
                    layout: runtime::LayoutNode::Pane(runtime::MuxPaneId("p-1".into())),
                    panes: panes
                        .iter()
                        .map(|(id, title)| PaneSnapshot {
                            id: runtime::MuxPaneId(id.to_string()),
                            session_id: Some(SessionId(*id)),
                            title: (*title).to_owned(),
                        })
                        .collect(),
                }],
                active_tab: Some(runtime::MuxTabId("tab-1".into())),
                focused_pane: Some(runtime::MuxPaneId("p-1".into())),
            }),
        }
    }

    #[test]
    fn mux가_세션_목록과_제목을_만든다() {
        let mut sessions = BTreeMap::new();
        let mut resource = None;
        apply_event(&mut sessions, &mut resource, &mux_event(&[(10, "claude")]));
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[&10].title, "claude");
        assert_eq!(sessions[&10].status, SessionStatus::Running);
    }

    #[test]
    fn 상태_이벤트가_세션_상태를_덮고_mux는_상태를_보존한다() {
        let mut sessions = BTreeMap::new();
        let mut resource = None;
        apply_event(&mut sessions, &mut resource, &mux_event(&[(10, "claude")]));
        apply_event(
            &mut sessions,
            &mut resource,
            &RuntimeEvent::SessionStatusChanged {
                session: SessionId(10),
                status: SessionStatus::NeedsApproval,
            },
        );
        assert_eq!(sessions[&10].status, SessionStatus::NeedsApproval);
        // 제목만 바뀌는 MuxUpdated가 상태를 리셋하지 않아야 한다
        apply_event(
            &mut sessions,
            &mut resource,
            &mux_event(&[(10, "claude-2")]),
        );
        assert_eq!(sessions[&10].title, "claude-2");
        assert_eq!(sessions[&10].status, SessionStatus::NeedsApproval);
    }

    #[test]
    fn exit는_완료_배지_mux에서_사라지면_목록에서_제거() {
        let mut sessions = BTreeMap::new();
        let mut resource = None;
        apply_event(
            &mut sessions,
            &mut resource,
            &mux_event(&[(10, "a"), (11, "b")]),
        );
        apply_event(
            &mut sessions,
            &mut resource,
            &RuntimeEvent::SessionExited {
                session: SessionId(11),
                exit_code: Some(0),
            },
        );
        assert!(sessions[&11].exited);
        assert_eq!(sessions[&11].status, SessionStatus::Done);
        // pane이 닫히면(다음 MuxUpdated에서 빠지면) 목록에서 제거
        apply_event(&mut sessions, &mut resource, &mux_event(&[(10, "a")]));
        assert!(!sessions.contains_key(&11));
    }

    #[test]
    fn resource_usage가_리소스뷰를_만든다() {
        let mut sessions = BTreeMap::new();
        let mut resource = None;
        apply_event(
            &mut sessions,
            &mut resource,
            &RuntimeEvent::ResourceUsage {
                snapshot: ProcessResourceSnapshot {
                    pid: 1,
                    sampled_at_ms: 1,
                    rss_bytes: 350 * 1024 * 1024,
                    cpu_percent: Some(9.0),
                    high_cpu: false,
                    high_rss: false,
                },
                session_usage: Vec::new(),
            },
        );
        let view = resource.unwrap();
        assert_eq!(view.rss_mb, 350);
        assert_eq!(view.cpu, Some(9.0));
    }

    #[test]
    fn status_문자열_매핑() {
        assert_eq!(status_str(SessionStatus::NeedsApproval), "needs_approval");
        assert_eq!(status_str(SessionStatus::Running), "running");
        assert_eq!(status_str(SessionStatus::Done), "done");
    }

    /// 캡처된 명령에서 시청 lease만 (세션, viewing)으로 뽑는다.
    fn lease_of(command: &RuntimeCommand) -> Option<(u64, bool)> {
        match command {
            RuntimeCommand::SetRemoteViewing {
                session, viewing, ..
            } => Some((session.0, *viewing)),
            _ => None,
        }
    }

    fn capture_sink() -> (CommandSink, StdArc<Mutex<Vec<RuntimeCommand>>>) {
        let captured: StdArc<Mutex<Vec<RuntimeCommand>>> = StdArc::default();
        let sink_cap = StdArc::clone(&captured);
        let sink: CommandSink = StdArc::new(move |command| {
            sink_cap.lock().unwrap().push(command);
        });
        (sink, captured)
    }

    #[test]
    fn rebind_watch는_refcount_전이에서만_lease를_보낸다() {
        let (handle, thread) = DashboardHandle::spawn(None);
        let (sink, captured) = capture_sink();
        handle.set_command_sink(sink);
        // conn1: watch 7 (0→1 on) / conn2: watch 7 (1→2 무전송)
        handle.rebind_watch(None, Some(7));
        handle.rebind_watch(None, Some(7));
        // conn1: 7→9 전환 (7은 2→1 무전송, 9는 0→1 on) — 재바인딩
        handle.rebind_watch(Some(7), Some(9));
        // 같은 세션으로의 재전환은 no-op
        handle.rebind_watch(Some(9), Some(9));
        // conn2 종료 (7: 1→0 off), conn1 종료 (9: 1→0 off)
        handle.rebind_watch(Some(7), None);
        handle.rebind_watch(Some(9), None);
        let got: Vec<(u64, bool)> = captured
            .lock()
            .unwrap()
            .iter()
            .filter_map(lease_of)
            .collect();
        assert_eq!(
            got,
            vec![(7, true), (9, true), (7, false), (9, false)],
            "refcount 전이 외 lease 전송이 있음"
        );
        handle.stop();
        thread.join().unwrap();
    }

    #[test]
    fn 재선언은_시청_중인_모든_세션의_lease를_다시_보낸다() {
        let (handle, thread) = DashboardHandle::spawn(None);
        let (sink, captured) = capture_sink();
        handle.set_command_sink(sink);
        handle.rebind_watch(None, Some(3));
        handle.rebind_watch(None, Some(5));
        captured.lock().unwrap().clear();
        // 새 worker 구독 직후(set_runtime_source 경로) — 모든 시청 lease 재선언
        handle.reassert_watch_leases();
        let got: Vec<(u64, bool)> = captured
            .lock()
            .unwrap()
            .iter()
            .filter_map(lease_of)
            .collect();
        assert_eq!(got, vec![(3, true), (5, true)]);
        handle.stop();
        thread.join().unwrap();
    }

    fn viewport_event(session: u64) -> RuntimeEvent {
        let cells = vec![
            runtime::TerminalCell {
                c: ' ',
                fg: [255, 255, 255],
                bg: [0, 0, 0],
                wide: false,
                wide_spacer: false,
            };
            4
        ];
        RuntimeEvent::Viewport {
            session: SessionId(session),
            snapshot: StdArc::new(runtime::TerminalViewportSnapshot {
                cols: 2,
                rows: 2,
                cursor: runtime::CursorSnapshot {
                    col: 0,
                    row: 0,
                    shape: runtime::CursorShape::Block,
                    visible: true,
                },
                visible_cells: cells.into(),
                dirty_ranges: Vec::new(),
                title: None,
                scroll_offset: 0,
                is_alt_screen: false,
            }),
            bracketed_paste: false,
        }
    }

    #[test]
    fn 시청_슬롯은_watch_중에만_쌓이고_해제되면_제거된다() {
        let (handle, thread) = DashboardHandle::spawn(None);
        let (sink, _captured) = capture_sink();
        handle.set_command_sink(sink);
        // 시청 전 Viewport — 슬롯에 쌓이지 않는다 (메모리 유계)
        handle.inject_event(viewport_event(7));
        assert!(handle.viewport_if_newer(7, 0).is_none());
        // 시청 시작 → 이벤트마다 seq 증가
        handle.rebind_watch(None, Some(7));
        handle.inject_event(viewport_event(7));
        let (seq1, _) = handle.viewport_if_newer(7, 0).expect("슬롯 없음");
        handle.inject_event(viewport_event(7));
        let (seq2, _) = handle.viewport_if_newer(7, 0).expect("슬롯 없음");
        assert!(seq2 > seq1);
        // 이미 본 seq — None (불필요 재전송 방지)
        assert!(handle.viewport_if_newer(7, seq2).is_none());
        // 다른(비시청) 세션은 여전히 없음
        handle.inject_event(viewport_event(8));
        assert!(handle.viewport_if_newer(8, 0).is_none());
        // 마지막 시청자 이탈 → 슬롯 제거 (trailing 스냅샷 없음)
        handle.rebind_watch(Some(7), None);
        assert!(handle.viewport_if_newer(7, 0).is_none());
        handle.stop();
        thread.join().unwrap();
    }

    #[test]
    fn 갱신주기가_지난_시청만_갱신_대상이_된다() {
        let mut watchers = BTreeMap::new();
        let now = Instant::now();
        watchers.insert(
            1,
            WatcherEntry {
                count: 1,
                last_renewal: now - Duration::from_secs(20),
            },
        );
        watchers.insert(
            2,
            WatcherEntry {
                count: 1,
                last_renewal: now,
            },
        );
        let due = due_lease_renewals(&mut watchers, now, Duration::from_secs(15));
        assert_eq!(due, vec![1]);
        // 갱신 직후엔 만기가 리셋돼 due가 비어야 한다 (매 tick 재전송 방지)
        assert!(due_lease_renewals(&mut watchers, now, Duration::from_secs(15)).is_empty());
    }
}
