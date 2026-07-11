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

use runtime::{ProcessResourceSnapshot, RuntimeEvent, RuntimeEventReceiver, SessionStatus};

use crate::protocol::{ApprovalView, ResourceView, ServerMsg, SessionView};

/// 승인 DB 폴링 주기 — 접속이 있을 때만 적용된다(계획 완료기준: 상태/승인 반영 ≤1s).
const POLL_INTERVAL: Duration = Duration::from_secs(1);

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
}

/// 접속 스레드가 소켓으로 밀어낼 발행 스냅샷. 버전이 오르면 push 대상.
#[derive(Default)]
struct Published {
    dashboard_json: String,
    dash_version: u64,
    approvals_json: String,
    appr_version: u64,
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

    /// 활성 workspace worker 구독을 붙인다(시작 + 워크스페이스 전환마다). 옛 receiver는
    /// 교체와 함께 drop되어 옛 worker가 자기 subscriber를 정리한다.
    pub fn set_runtime_source(&self, receiver: RuntimeEventReceiver) {
        let mut inner = self.shared.inner.lock().expect("dashboard inner lock");
        inner.receiver = Some(receiver);
        inner.dirty = true;
        self.shared.cvar.notify_all();
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
    /// (상태 스트림 프레임을 실제 WS로 검증하기 위한 주입 시드).
    #[cfg(test)]
    pub fn inject_event(&self, event: RuntimeEvent) {
        {
            let mut inner = self.shared.inner.lock().expect("dashboard inner lock");
            let Inner {
                sessions, resource, ..
            } = &mut *inner;
            apply_event(sessions, resource, &event);
            inner.dirty = true;
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
        if let Some(receiver) = inner.receiver.as_ref() {
            let events = receiver.drain();
            if receiver.take_overflowed() {
                tracing::warn!(
                    "web-remote 대시보드 이벤트 큐 overflow — 상태가 잠시 뒤처질 수 있음"
                );
            }
            let Inner {
                sessions, resource, ..
            } = &mut *inner;
            for event in &events {
                apply_event(sessions, resource, event);
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
}
