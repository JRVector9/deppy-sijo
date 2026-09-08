//! 대시보드 브리지 (계획 v3.3 P2 — 킬러 기능의 데이터 계층).
//!
//! 전용 스레드 하나가 두 소스를 모아 접속 WS가 읽을 스냅샷을 발행한다:
//!   1. **활성 workspace worker 이벤트** — `subscribe_with_wake`로 구독한
//!      [`RuntimeEventReceiver`]를 drain해 세션 상태·리소스 맵을 갱신한다. wake 클로저가
//!      이 스레드를 깨우므로 **egui 프레임과 무관하게**(창이 숨겨져 리페인트가 멈춰도)
//!      상태가 흐른다. (§14.1 Warm 알림 유지 관례)
//!   2. **승인 저장소 포트** — app이 주입한 [`WebRemoteRepository`]로 pending 승인을 1초
//!      폴링하고 Allow/Deny를 되쓴다. 웹 계층은 concrete DB를 생성하거나 소유하지 않는다.
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
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use runtime::{
    ProcessResourceSnapshot, RuntimeCommand, RuntimeEvent, RuntimeEventReceiver, SessionId,
    SessionStatus,
};

use crate::protocol::{ApprovalView, ResourceView, ServerMsg, SessionView, WorkspaceView};
use crate::repository::{PENDING_APPROVAL_LIMIT, PendingApprovalRecord, WebRemoteRepository};

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

/// web → app 워크스페이스 전환 싱크 (미러 진입 — I1b-2). 명령 싱크(runtime 레벨)와 달리
/// 워크스페이스 전환은 app 레벨이라 별도 채널이 필요하다. 앱이 start_web에서 한 번만
/// 주입하고(워커별이 아니라 안정적), 폰의 Switch 메시지를 받아 워크스페이스 id를 넘긴다.
/// 앱은 이걸 egui 스레드의 전환 큐에 넣고 repaint를 요청한다. fire-and-forget.
pub type SwitchSink = Arc<dyn Fn(String) + Send + Sync>;

/// 영속 UUID ↔ worker-로컬 u64 양방향 맵 (v3.7 I1).
#[derive(Default)]
struct IdMap {
    to_session: BTreeMap<String, u64>,
    to_uuid: BTreeMap<u64, String>,
}

impl IdMap {
    fn clear(&mut self) {
        self.to_session.clear();
        self.to_uuid.clear();
    }
    fn insert(&mut self, uuid: String, session: u64) {
        self.to_session.insert(uuid.clone(), session);
        self.to_uuid.insert(session, uuid);
    }
    /// UUID → 현재 워커의 u64. **모르는 UUID면 None** — 명령이 만들어지지 않는다.
    fn session(&self, uuid: &str) -> Option<u64> {
        self.to_session.get(uuid).copied()
    }
    fn uuid(&self, session: u64) -> Option<&str> {
        self.to_uuid.get(&session).map(String::as_str)
    }
}

/// 세션 하나의 시청 집계 (P5b) — 접속 수 + 마지막 lease 갱신 시각.
struct WatcherEntry {
    count: usize,
    last_renewal: Instant,
}

/// 갱신 주기가 지난 시청 세션들을 골라 last_renewal을 갱신한다 (순수 — 테스트 대상).
fn due_lease_renewals(
    watchers: &mut BTreeMap<String, WatcherEntry>,
    now: Instant,
    interval: Duration,
) -> Vec<String> {
    watchers
        .iter_mut()
        .filter(|(_, entry)| now.duration_since(entry.last_renewal) >= interval)
        .map(|(id, entry)| {
            entry.last_renewal = now;
            id.clone()
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

/// 이 길이를 넘거나 여러 줄이면 "붙여넣기"로 간주한다 (P6a — bracketed wrap 판정).
const INPUT_PASTE_THRESHOLD: usize = 512;

/// composer 텍스트를 PTY 입력 바이트로 인코딩한다 (P6a — 순수, 테스트 대상).
///
/// 1) 개행 정규화(\r\n·\r→\n) 후 C0 제어문자 strip(\t·\n 제외) — 클라이언트發
///    이스케이프 시퀀스 주입 차단(제어 시퀀스는 named key 화이트리스트로만).
/// 2) \n→\r (터미널 Enter는 CR).
/// 3) 여러 줄이거나 512B 초과면 붙여넣기로 간주 — 세션 bracketed paste 모드가
///    켜져 있으면 `ESC[200~ … ESC[201~` wrap (에이전트가 한 블록으로 인식).
/// 4) submit이면 wrap **밖에** Enter(\r)를 덧붙인다.
///
/// 빈 결과(공백뿐 + submit 없음)는 None — 불필요한 WriteInput을 만들지 않는다.
fn encode_input(text: &str, submit: bool, bracketed: bool) -> Option<Vec<u8>> {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let cleaned: String = normalized
        .chars()
        .filter(|c| !c.is_control() || *c == '\t' || *c == '\n')
        .collect();
    let is_paste = cleaned.contains('\n') || cleaned.len() > INPUT_PASTE_THRESHOLD;
    let body = cleaned.replace('\n', "\r");
    if body.is_empty() && !submit {
        return None;
    }
    let mut bytes = Vec::with_capacity(body.len() + 16);
    if is_paste && bracketed {
        bytes.extend_from_slice(b"\x1b[200~");
        bytes.extend_from_slice(body.as_bytes());
        bytes.extend_from_slice(b"\x1b[201~");
    } else {
        bytes.extend_from_slice(body.as_bytes());
    }
    if submit {
        bytes.push(b'\r');
    }
    Some(bytes)
}

/// [`runtime::PtyInputRejectReason`] → 프로토콜 문자열.
fn pressure_reason(reason: runtime::PtyInputRejectReason) -> &'static str {
    match reason {
        runtime::PtyInputRejectReason::QueueFull => "queue_full",
        runtime::PtyInputRejectReason::SessionClosed => "closed",
        runtime::PtyInputRejectReason::WriterUnavailable => "unavailable",
        runtime::PtyInputRejectReason::PayloadTooLarge => "too_large",
    }
}

/// 세션 한 행의 경량 상태(런타임 이벤트에서 누적). 제목은 앱 스냅샷(WorkspaceSeed)이
/// 갖는다 — 여기엔 프레임 독립으로 흘러야 하는 상태만 둔다.
#[derive(Debug, Clone, PartialEq)]
struct SessionEntry {
    status: SessionStatus,
    /// SessionExited/Restored 관측 — 완료 배지.
    exited: bool,
}

/// 앱이 웹 대시보드에 넘기는 세션 한 행.
///
/// 제목은 **앱이 해석한 표시명**이다(프로젝트명 규칙 — 데스크톱 활동 패널과 동일).
/// 브리지는 MuxUpdated의 raw pane 제목("workspace.spawn.shell 140")으로 이걸 덮지
/// 않는다 — 폰에서도 사람이 읽을 수 있는 이름이 보이게 한다.
///
/// 활성 워크스페이스 세션만 `id`가 있다(시청/입력 대상). warm/유휴는 표시 전용 —
/// 세션 id가 worker-로컬이라 다른 워크스페이스 id로 시청하면 엉뚱한 세션이 잡힌다.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSeed {
    pub id: Option<u64>,
    pub title: String,
    pub status: Option<SessionStatus>,
    /// 돌고 있는 에이전트 요약("Claude · sonnet · high"). 앱의 감지 결과 — 활성
    /// 워크스페이스만 감지 워커가 돌므로 warm/유휴는 None.
    pub agent: Option<String>,
    pub exited: bool,
}

/// 워크스페이스 상태 — 데스크톱 활동 패널과 같은 3분류(§14.1).
/// - Active: 렌더 중인 활성 워커
/// - Warm(대기): 워커·에이전트 살아있음. 폰 진입 = 즉시 미러(재사용)
/// - Suspended(절전): 워커 종료·에이전트 kill됨, DB/디스크만. 폰 진입 = 깨우기 필요
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceState {
    Active,
    Warm,
    Suspended,
}

impl WorkspaceState {
    fn as_str(self) -> &'static str {
        match self {
            WorkspaceState::Active => "active",
            WorkspaceState::Warm => "warm",
            WorkspaceState::Suspended => "suspended",
        }
    }
}

/// 앱이 넘기는 워크스페이스 한 묶음 — 활성 1개 + warm/유휴 N개.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceSeed {
    pub id: String,
    pub name: String,
    pub state: WorkspaceState,
    pub sessions: Vec<SessionSeed>,
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
///
/// 반환값 = Dashboard 프레임 내용이 바뀌었을 수 있는가. Viewport/PtyInputPressure처럼
/// 대시보드 비범위 이벤트는 false — run()이 이 값으로 Dashboard JSON 재구축을 게이트해
/// 시청 스트리밍 중(초당 수십 회 Viewport wake) 불필요한 직렬화를 건너뛴다 (PR-F1).
fn apply_event(
    sessions: &mut BTreeMap<u64, SessionEntry>,
    resource: &mut Option<ResourceView>,
    ids: &mut IdMap,
    event: &RuntimeEvent,
) -> bool {
    match event {
        RuntimeEvent::MuxUpdated { snapshot } => {
            // 영속 UUID ↔ worker-로컬 u64 매핑 갱신 — 스냅샷이 진실의 원천이다 (I1).
            // pane에 붙어 있는 세션만 남긴다(전환/종료 시 옛 매핑이 남지 않게).
            ids.clear();
            for tab in &snapshot.tabs {
                for pane in &tab.panes {
                    if let (Some(session), Some(uuid)) =
                        (pane.session_id, pane.persistent_session_id.as_ref())
                    {
                        ids.insert(uuid.clone(), session.0);
                    }
                }
            }
            // pane에 붙은 세션 집합이 **소속의 근거**다(추가/삭제). 제목은 여기서 쓰지
            // 않는다 — mux의 raw 제목("workspace.spawn.shell 140")은 사람이 읽기 어렵고,
            // 표시명은 앱이 해석해 push한다(WorkspaceSeed). 상태·exited는 보존한다.
            let mut live: Vec<u64> = Vec::new();
            for tab in &snapshot.tabs {
                for pane in &tab.panes {
                    if let Some(session) = pane.session_id {
                        live.push(session.0);
                    }
                }
            }
            sessions.retain(|id, _| live.contains(id));
            for id in live {
                sessions.entry(id).or_insert_with(new_entry);
            }
            true
        }
        RuntimeEvent::SessionStatusChanged { session, status } => {
            sessions.entry(session.0).or_insert_with(new_entry).status = *status;
            true
        }
        RuntimeEvent::SessionStatusViewChanged { session, view } => {
            sessions.entry(session.0).or_insert_with(new_entry).status = view.status;
            true
        }
        RuntimeEvent::SessionExited { session, .. }
        | RuntimeEvent::SessionRestored { session, .. } => {
            let entry = sessions.entry(session.0).or_insert_with(new_entry);
            entry.exited = true;
            entry.status = SessionStatus::Done;
            true
        }
        RuntimeEvent::ShellSpawned { session } | RuntimeEvent::AgentSpawned { session } => {
            sessions.entry(session.0).or_insert_with(new_entry);
            true
        }
        RuntimeEvent::ResourceUsage { snapshot, .. } => {
            *resource = Some(resource_view(snapshot));
            true
        }
        // Viewport(터미널 뷰어=P5)·PtyInputPressure·SpawnFailed 등은 대시보드 비범위.
        _ => false,
    }
}

fn new_entry() -> SessionEntry {
    SessionEntry {
        status: SessionStatus::Running,
        exited: false,
    }
}

/// 런타임 이벤트 하나를 웹푸시 싱크로 넘긴다(P4). 세션 상태 전이만 대상 — 실제 발송 여부는
/// push 계층이 상태(입력대기/완료)와 중복 억제로 결정한다. SessionRestored(재시작 복원)는
/// "완료"가 아니므로 넘기지 않는다 — 앱 재시작 때마다 완료 알림이 쏟아지는 것을 막는다.
/// 딥링크가 재시작 후에도 같은 세션을 가리키도록 **영속 UUID**를 넘긴다 (I1).
/// UUID를 모르는 세션(persist 없음)은 알림을 보내지 않는다 — 열어봐야 엉뚱한 곳이다.
fn forward_to_push(push: &crate::push::PushHandle, ids: &IdMap, event: &RuntimeEvent) {
    let (session, status) = match event {
        RuntimeEvent::SessionStatusChanged { session, status } => (session.0, *status),
        RuntimeEvent::SessionStatusViewChanged { session, view } => (session.0, view.status),
        RuntimeEvent::SessionExited { session, .. } => (session.0, SessionStatus::Done),
        _ => return,
    };
    if let Some(uuid) = ids.uuid(session) {
        push.notify_session(uuid.to_owned(), status);
    }
}

fn resource_view(snapshot: &ProcessResourceSnapshot) -> ResourceView {
    ResourceView {
        cpu: snapshot.cpu_percent,
        rss_mb: snapshot.rss_bytes / (1024 * 1024),
    }
}

/// 세션 맵을 정렬된 표시 목록으로. id 오름차순(안정적 렌더).
/// 앱 스냅샷(제목·워크스페이스 구성)과 런타임 이벤트 맵(활성 세션의 라이브 상태)을
/// 합쳐 표시 목록을 만든다.
///
/// - **활성 워크스페이스**: 소속·상태는 이벤트 맵이 근거(프레임 독립 — 창이 숨겨져도
///   흐른다). 제목은 앱 스냅샷에서 가져오고(없으면 "세션 N" 폴백), 시청 가능(id 포함).
/// - **warm/유휴**: 앱 스냅샷 그대로 — 표시 전용(id 없음).
fn workspace_views(
    workspaces: &[WorkspaceSeed],
    live: &BTreeMap<u64, SessionEntry>,
    ids: &IdMap,
) -> Vec<WorkspaceView> {
    // 앱 스냅샷이 아직 없는데(웹서버 기동 직후 첫 프레임 전) 라이브 세션이 있으면,
    // 세션이 통째로 안 보이는 것보다 이름 없는 활성 워크스페이스로라도 보여준다.
    if workspaces.is_empty() && !live.is_empty() {
        return vec![WorkspaceView {
            id: String::new(),
            name: String::new(),
            state: WorkspaceState::Active.as_str(),
            sessions: live
                .iter()
                .map(|(id, entry)| SessionView {
                    // UUID가 없으면 표시 전용 — 폰은 u64를 모른다 (I1).
                    id: ids.uuid(*id).map(str::to_owned),
                    title: format!("세션 {id}"),
                    status: Some(status_str(entry.status)),
                    agent: None,
                    exited: entry.exited,
                })
                .collect(),
        }];
    }
    workspaces
        .iter()
        .map(|ws| {
            let sessions = if ws.state == WorkspaceState::Active {
                let by_id: BTreeMap<u64, &SessionSeed> = ws
                    .sessions
                    .iter()
                    .filter_map(|s| s.id.map(|id| (id, s)))
                    .collect();
                live.iter()
                    .map(|(id, entry)| {
                        let seed = by_id.get(id);
                        SessionView {
                            // **영속 UUID만 노출**한다 — 폰이 u64를 모르므로 워크스페이스
                            // 전환·재시작 후 옛 id로 엉뚱한 세션을 잡는 일이 구조적으로
                            // 불가능하다 (I1). UUID가 없는 세션(persist 없음/레거시)은
                            // 표시 전용으로 강등된다.
                            id: ids.uuid(*id).map(str::to_owned),
                            title: seed
                                .map(|s| s.title.clone())
                                .unwrap_or_else(|| format!("세션 {id}")),
                            status: Some(status_str(entry.status)),
                            agent: seed.and_then(|s| s.agent.clone()),
                            exited: entry.exited,
                        }
                    })
                    .collect()
            } else {
                ws.sessions
                    .iter()
                    .map(|s| SessionView {
                        // 표시 전용 — 이 워크스페이스의 워커가 없어 시청할 수 없다.
                        id: None,
                        title: s.title.clone(),
                        status: s.status.map(status_str),
                        agent: s.agent.clone(),
                        exited: s.exited,
                    })
                    .collect()
            };
            WorkspaceView {
                id: ws.id.clone(),
                name: ws.name.clone(),
                state: ws.state.as_str(),
                sessions,
            }
        })
        .collect()
}

/// 브리지 스레드 전용 가변 상태(런타임 소스 + DB + 파생 맵).
struct Inner {
    /// 새 이벤트/명령 도착 플래그(웨이크가 세운다).
    dirty: bool,
    /// Dashboard 프레임 재구축 필요 플래그 (PR-F1). wake는 dirty만 세우고, 대시보드
    /// 내용이 바뀌는 경로(apply_event 관련 이벤트·워크스페이스/notice/시드 setter·접속
    /// 등록)만 이 플래그를 세운다 — 시청 스트리밍의 Viewport wake가 매번 Dashboard
    /// JSON을 재직렬화하지 않게 한다.
    dashboard_dirty: bool,
    /// 접속 등록/resolve 직후 즉시 1회 폴링을 강제한다(주기와 무관).
    force_poll: bool,
    /// 활성 workspace worker 구독. 전환 시 [`DashboardHandle::set_runtime_source`]가 교체한다.
    receiver: Option<RuntimeEventReceiver>,
    /// 활성 워크스페이스 세션의 **라이브 상태**(런타임 이벤트 유래 — egui 프레임과 무관).
    sessions: BTreeMap<u64, SessionEntry>,
    /// 앱이 push한 워크스페이스 구성(활성+warm+유휴)과 세션 표시명. 상태가 바뀔 때만
    /// 갱신된다 — 폰이 전체 워크스페이스를 보고, 이름이 프로젝트명으로 뜨는 근거.
    workspaces: Vec<WorkspaceSeed>,
    resource: Option<ResourceView>,
    last_poll: Instant,
    /// 웹푸시 발송 싱크(P4). 이벤트 drain 시 세션 상태 전이(입력대기/완료)를 넘긴다 —
    /// 승인 발송은 push가 DB를 직접 폴링하므로 여기서 넘기지 않는다. None이면 푸시 비활성.
    push_sink: Option<crate::push::PushHandle>,
    /// web → runtime 명령 싱크 (P5b). None이면 시청 lease를 보내지 않는다(뷰어 비활성).
    command_sink: Option<CommandSink>,
    /// web → app 워크스페이스 전환 싱크 (미러 진입 — I1b-2). None이면 Switch를 무시한다.
    switch_sink: Option<SwitchSink>,
    /// 일시 안내 배너(미러 진입 상한 초과 등 — I1b-2). 앱이 세팅/해제하고 Dashboard 프레임에
    /// 실려 폰에 전달된다.
    notice: Option<String>,
    /// 세션별 시청 접속 집계 (P5b). 0→1에서 lease on, 1→0에서 lease off를 보낸다.
    watchers: BTreeMap<String, WatcherEntry>,
    /// 시청 세션별 최신 bracketed paste 모드 (P6a — Viewport 이벤트에서 캐시).
    /// send_input의 wrap 판정에 쓴다. 시청 종료 시 함께 정리된다.
    bracketed: BTreeMap<String, bool>,
    /// **영속 UUID ↔ worker-로컬 u64** (v3.7 I1). MuxUpdated가 진실의 원천이다.
    /// 폰에는 UUID만 노출하고(u64는 아예 안 보낸다), 명령을 만들 때 여기서 변환한다 —
    /// 워커가 모르는 UUID는 변환되지 않아 명령 자체가 만들어지지 않는다(앨리어싱 차단).
    ids: IdMap,
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
    viewports: BTreeMap<String, (u64, Arc<runtime::TerminalViewportSnapshot>)>,
    /// 시청 세션별 최신 입력 큐 압박 (P6a) — (버전, 인코딩된 InputPressure JSON).
    /// 최신본만 유지, 시청 종료 시 제거 — 메모리 유계.
    input_pressure: BTreeMap<String, (u64, String)>,
}

struct Shared {
    inner: Mutex<Inner>,
    published: Mutex<Published>,
    /// 승인 대시보드용 app-owned 저장소 포트(없으면 승인 목록은 빈 채로 상태만 흐른다).
    /// 구현체가 concrete connection 직렬화를 소유한다. 브리지는 inner를 잡은 채 포트를
    /// 호출하므로 resolve는 inner를 놓고 호출하며, 구현체는 이 브리지로 재진입하면 안 된다.
    repository: Option<Arc<dyn WebRemoteRepository>>,
    cvar: Condvar,
    /// 인증까지 마친 라이브 대시보드 WS 수 — wake/타이머 게이트.
    connections: AtomicUsize,
    stop: AtomicBool,
    /// 승인 DB 폴링 횟수(테스트: 접속 0에서 폴링 정지 검증).
    poll_count: AtomicU64,
    /// Dashboard JSON 재구축 횟수(테스트: Viewport wake만으로는 늘지 않음 검증 — PR-F1).
    dash_build_count: AtomicU64,
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
    /// 브리지 스레드를 띄운다. 저장소 포트가 있으면 승인 대시보드를 활성화한다. 반환된
    /// JoinHandle은 서버가 소유해 shutdown 시 join한다.
    pub fn spawn(repository: Option<Arc<dyn WebRemoteRepository>>) -> (Self, JoinHandle<()>) {
        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner {
                dirty: false,
                dashboard_dirty: false,
                force_poll: false,
                receiver: None,
                sessions: BTreeMap::new(),
                workspaces: Vec::new(),
                resource: None,
                last_poll: Instant::now() - POLL_INTERVAL,
                push_sink: None,
                command_sink: None,
                switch_sink: None,
                notice: None,
                watchers: BTreeMap::new(),
                bracketed: BTreeMap::new(),
                ids: IdMap::default(),
            }),
            published: Mutex::new(Published::default()),
            repository,
            cvar: Condvar::new(),
            connections: AtomicUsize::new(0),
            stop: AtomicBool::new(false),
            poll_count: AtomicU64::new(0),
            dash_build_count: AtomicU64::new(0),
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
    /// 교체와 함께 drop되어 옛 worker가 자기 subscriber를 정리한다.
    ///
    /// 시청 상태는 **정리**한다(재선언 금지 — P5 리뷰 P2): SessionId는 worker마다 1부터
    /// 재시작하므로, 옛 워크스페이스의 시청 세션 id를 새 worker에 재선언하면 **무관한
    /// 세션**이 승격되고 Ctrl-C까지 그 세션으로 들어간다. 옛 worker의 lease는 명시
    /// 해제를 보내지 않아도 TTL(≤45s)로 원복된다(Warm 전환 직후라 유계 허용).
    /// 폰 시청자는 프레임이 멎으므로 새 세션 목록에서 다시 선택한다.
    pub fn set_runtime_source(&self, receiver: RuntimeEventReceiver) {
        {
            let mut inner = self.shared.inner.lock().expect("dashboard inner lock");
            inner.receiver = Some(receiver);
            inner.dirty = true;
            inner.dashboard_dirty = true;
            Self::clear_watch_state(&mut inner, &self.shared);
        }
        self.shared.cvar.notify_all();
    }

    /// 시청 상태(watcher refcount + 화면 슬롯)를 전부 비운다 — 새 worker 구독 시
    /// 호출. 같은 inner 임계구역에서 published를 중첩 취득해 원자화한다.
    fn clear_watch_state(inner: &mut Inner, shared: &Shared) {
        inner.watchers.clear();
        inner.bracketed.clear();
        let mut published = shared.published.lock().expect("published lock");
        published.viewports.clear();
        published.input_pressure.clear();
    }

    /// web → runtime 명령 싱크를 붙인다 (P5b — receiver와 같은 시점에 교체된다).
    pub fn set_command_sink(&self, sink: CommandSink) {
        let mut inner = self.shared.inner.lock().expect("dashboard inner lock");
        inner.command_sink = Some(sink);
    }

    /// web → app 워크스페이스 전환 싱크를 붙인다 (미러 진입 — I1b-2). app 레벨이라 워커별이
    /// 아니라 start_web에서 한 번만 주입한다(receiver·command_sink와 달리 전환마다 교체 불필요).
    pub fn set_switch_sink(&self, sink: SwitchSink) {
        let mut inner = self.shared.inner.lock().expect("dashboard inner lock");
        inner.switch_sink = Some(sink);
    }

    /// 폰의 워크스페이스 전환 요청을 앱으로 넘긴다 (미러 진입 — I1b-2). 싱크는 앱 전환 큐에
    /// push하고 repaint만 하므로(브리지 inner를 다시 잠그지 않음) 락을 놓고 호출한다 — 재진입
    /// 데드락 회피. 싱크 미설정(뷰어 전용/테스트)이면 조용히 무시한다.
    pub fn request_switch(&self, workspace: &str) {
        let sink = {
            let inner = self.shared.inner.lock().expect("dashboard inner lock");
            inner.switch_sink.clone()
        };
        if let Some(sink) = sink {
            sink(workspace.to_owned());
        }
    }

    /// 폰에 띄울 일시 안내 배너를 세팅/해제한다 (미러 진입 상한 초과 등 — I1b-2). 내용이
    /// 바뀔 때만 dirty로 표시해 다음 Dashboard 프레임에 실어 보낸다(변화 없으면 비용 0).
    pub fn set_notice(&self, notice: Option<String>) {
        {
            let mut inner = self.shared.inner.lock().expect("dashboard inner lock");
            if inner.notice == notice {
                return;
            }
            inner.notice = notice;
            inner.dirty = true;
            // None(TTL 만료 해제) 포함 — 안 세우면 배너가 폰에서 내려가지 않는다 (PR-F1).
            inner.dashboard_dirty = true;
        }
        self.shared.cvar.notify_all();
    }

    /// 접속의 시청 대상 전환 (P5b). `from`을 내리고 `to`를 올린다 — refcount 전이
    /// (0→1 / 1→0)에서만 runtime lease 명령이 나간다.
    ///
    /// 전이·슬롯 제거·명령 전송을 **모두 inner 임계구역 안에서** 수행한다 (P5 리뷰):
    /// - 슬롯 제거를 watcher 제거와 원자화 — run()의 스테이징(역시 inner 하)이 사이에
    ///   끼어 시청 0인 세션의 슬롯을 부활시키는 레이스 차단 (②-P1).
    /// - lease 명령을 락 안에서 보내 cross-thread on/off 순서 역전 차단 (공통 지적).
    ///   sink는 try_send+unpark(비블로킹)라 락 하 호출이 안전하다.
    ///
    /// `from`과 `to`는 **영속 세션 UUID**다 (I1). lease 명령은 현재 워커의 u64로 변환해서만
    /// 나간다 — 워커가 모르는 UUID(전환된 워크스페이스 등)면 명령 자체를 만들지 않는다.
    pub fn rebind_watch(&self, from: Option<&str>, to: Option<&str>) {
        if from == to {
            return;
        }
        let mut commands: Vec<RuntimeCommand> = Vec::new();
        let mut inner = self.shared.inner.lock().expect("dashboard inner lock");
        if let Some(old) = from
            && let Some(entry) = inner.watchers.get_mut(old)
        {
            entry.count = entry.count.saturating_sub(1);
            if entry.count == 0 {
                inner.watchers.remove(old);
                inner.bracketed.remove(old);
                if let Some(session) = inner.ids.session(old) {
                    commands.push(lease_command(session, false));
                }
                // 마지막 시청자 이탈 — 화면/입력압박 슬롯도 같은 임계구역에서 제거
                // (락 순서 inner→published 중첩, 역순 취득 경로 없음 — 교착 없음).
                let mut published = self.shared.published.lock().expect("published lock");
                published.viewports.remove(old);
                published.input_pressure.remove(old);
            }
        }
        if let Some(new) = to {
            let entry = inner
                .watchers
                .entry(new.to_owned())
                .or_insert(WatcherEntry {
                    count: 0,
                    last_renewal: Instant::now(),
                });
            entry.count += 1;
            if entry.count == 1 {
                entry.last_renewal = Instant::now();
                if let Some(session) = inner.ids.session(new) {
                    commands.push(lease_command(session, true));
                }
            }
        }
        if let Some(sink) = &inner.command_sink {
            for command in commands {
                sink(command);
            }
        }
    }

    /// 시청 세션에 composer 텍스트를 입력한다 (P6a). `uuid`는 **영속 세션 UUID**다 —
    /// 현재 워커의 u64로 변환되지 않으면(다른 워크스페이스/죽은 세션) 아무것도 하지 않는다.
    pub fn send_input(&self, uuid: &str, text: &str, submit: bool) {
        let (sink, session, bracketed) = {
            let inner = self.shared.inner.lock().expect("dashboard inner lock");
            if !inner.watchers.contains_key(uuid) {
                return; // 시청 중이 아니다(전환으로 정리됐을 수 있다 — 리뷰 P2-1)
            }
            let Some(session) = inner.ids.session(uuid) else {
                return; // 이 워커가 모르는 세션 — 명령 자체를 만들지 않는다 (I1)
            };
            (
                inner.command_sink.clone(),
                session,
                inner.bracketed.get(uuid).copied().unwrap_or(false),
            )
        };
        let Some(sink) = sink else { return };
        if let Some(bytes) = encode_input(text, submit, bracketed) {
            sink(RuntimeCommand::WriteInput {
                session: SessionId(session),
                bytes,
            });
        }
    }

    /// 시청 세션의 입력 큐 압박이 `last`보다 새로우면 (버전, JSON)을 돌려준다 (P6a).
    pub fn input_pressure_if_newer(&self, uuid: &str, last: u64) -> Option<(u64, String)> {
        let published = self.shared.published.lock().expect("published lock");
        published
            .input_pressure
            .get(uuid)
            .filter(|(version, _)| *version > last)
            .map(|(version, json)| (*version, json.clone()))
    }

    /// 시청 중이고 **현재 워커가 아는** 세션이면 u64를 돌려준다 — 명령 게이트.
    /// 워크스페이스 전환 후 남은 접속 바인딩(리뷰 P2-1)과 worker-로컬 id 앨리어싱(I1)을
    /// 한 지점에서 막는다.
    fn resolve_watched(&self, uuid: &str) -> Option<u64> {
        let inner = self.shared.inner.lock().expect("dashboard inner lock");
        if !inner.watchers.contains_key(uuid) {
            return None;
        }
        inner.ids.session(uuid)
    }

    /// 시청 세션의 스크롤백을 이동한다. delta 양수 = 과거로.
    pub fn send_scroll(&self, uuid: &str, delta: i32) {
        const SCROLL_DELTA_CAP: i32 = 100_000;
        let Some(session) = self.resolve_watched(uuid) else {
            return;
        };
        let delta = delta.clamp(-SCROLL_DELTA_CAP, SCROLL_DELTA_CAP);
        if delta == 0 {
            return;
        }
        let sink = self
            .shared
            .inner
            .lock()
            .expect("dashboard inner lock")
            .command_sink
            .clone();
        if let Some(sink) = sink {
            sink(RuntimeCommand::Scroll {
                session: SessionId(session),
                delta,
            });
        }
    }

    /// 시청 세션에 제어 키를 보낸다 (P5d + P6a 확장 — 화살표/Esc/Tab 등).
    /// 제어 시퀀스는 이 화이트리스트로만 생성된다(자유 텍스트의 제어문자는 strip).
    pub fn send_key(&self, uuid: &str, key: &str) {
        let bytes: &[u8] = match key {
            "ctrl_c" => b"\x03",
            "ctrl_d" => b"\x04",
            "enter" => b"\r",
            "esc" => b"\x1b",
            "tab" => b"\t",
            "shift_tab" => b"\x1b[Z",
            "up" => b"\x1b[A",
            "down" => b"\x1b[B",
            "right" => b"\x1b[C",
            "left" => b"\x1b[D",
            _ => return,
        };
        let Some(session) = self.resolve_watched(uuid) else {
            return;
        };
        let sink = self
            .shared
            .inner
            .lock()
            .expect("dashboard inner lock")
            .command_sink
            .clone();
        if let Some(sink) = sink {
            sink(RuntimeCommand::WriteInput {
                session: SessionId(session),
                bytes: bytes.to_vec(),
            });
        }
    }

    /// 현재 워크스페이스의 세션 목록/상태/제목/exited를 시드한다(start_web·워크스페이스 전환 시,
    /// 구독 등록 직후 호출). 세션 맵을 **통째로 교체**해, `SetWorkspaceState(Active)`(비동기)와
    /// 구독 등록(동기) 사이 레이스로 MuxUpdated 재발화를 놓쳐도 옛 워크스페이스 세션이 남지
    /// 않게 한다. 이후 도착하는 이벤트는 증분 갱신이며, MuxUpdated의 or_insert는 시드된 항목을
    /// and_modify(제목만)로 건드리므로 시드된 상태를 기본값(Running)으로 덮어쓰지 않는다.
    /// 표시 스냅샷(워크스페이스 구성 + 세션 표시명)만 갱신한다. 앱이 매 프레임 호출하며,
    /// 값이 같으면 조기 반환한다.
    ///
    /// **활성 세션의 상태(sessions 맵)는 건드리지 않는다** — 그건 런타임 이벤트가 소유하는
    /// 프레임 독립 데이터다. 여기서 앱 스냅샷으로 덮으면, 창이 숨겨져 앱의 상태 뷰가 얼어붙은
    /// 동안(ui() 스킵 → pending_events 미소비) 브리지가 이미 반영한 최신 상태(예: 완료)를
    /// **stale한 앱 값(실행 중)으로 되돌린다**. 그 이벤트는 edge-trigger라 재발화되지 않아
    /// 창이 다시 보일 때까지 폰이 영구 오표시된다 (리뷰 P1-1). 상태 시딩은 재구독 시점의
    /// [`Self::reseed_active_sessions`]만 수행한다.
    pub fn set_workspaces(&self, seeds: Vec<WorkspaceSeed>) {
        {
            let mut inner = self.shared.inner.lock().expect("dashboard inner lock");
            if inner.workspaces == seeds {
                return; // 변화 없음 — 발행/웨이크 생략(프레임마다 호출돼도 비용 0)
            }
            inner.workspaces = seeds;
            inner.dirty = true;
            inner.dashboard_dirty = true;
        }
        self.shared.cvar.notify_all();
    }

    /// 활성 워크스페이스 세션의 **라이브 상태 맵을 통째 교체**한다 — 재구독(start_web·
    /// 워크스페이스 전환) 시점에만 호출한다. 이벤트 스트림은 edge-trigger라 재구독한
    /// 브리지는 과거 이력을 모르므로, 이미 정착한 상태(needs_approval 등)를 시드해야 한다.
    /// 통째 교체라 옛 워크스페이스 세션이 남지 않는다(P2 리뷰의 전환 레이스 해소).
    pub fn reseed_active_sessions(&self, sessions: &[SessionSeed]) {
        {
            let mut inner = self.shared.inner.lock().expect("dashboard inner lock");
            inner.sessions = sessions
                .iter()
                .filter_map(|seed| {
                    seed.id.map(|id| {
                        (
                            id,
                            SessionEntry {
                                status: seed.status.unwrap_or(SessionStatus::Running),
                                exited: seed.exited,
                            },
                        )
                    })
                })
                .collect();
            inner.dirty = true;
            inner.dashboard_dirty = true;
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
            // 첫 접속(접속 0 구간)에는 발행을 건너뛰므로, 등록 시점에 최신 Dashboard를
            // 반드시 한 번 만들어 Welcome 직후 첫 프레임이 나가게 한다 (PR-F1).
            inner.dashboard_dirty = true;
        }
        self.shared.cvar.notify_all();
        ConnectionGuard {
            shared: Arc::clone(&self.shared),
        }
    }

    /// 승인 결정을 DB에 되쓴다(first-writer-wins — 이미 해소된 id는 조용한 no-op). 이후
    /// 즉시 재폴링을 강제해 목록에서 사라진 걸 빠르게 반영한다.
    pub fn resolve(&self, id: &str, allowed: bool, remember: bool) {
        let now = deppy_core::time::unix_secs_i64();
        // 재폴링 강제 플래그만 inner에서 세우고 즉시 놓는다 — DB 되쓰기(busy_timeout 최대 5s)를
        // inner 락 밖에서 수행해 브리지의 이벤트 drain·접속 등록이 막히지 않게 한다(P3 리뷰).
        {
            let mut inner = self.shared.inner.lock().expect("dashboard inner lock");
            inner.force_poll = true;
            inner.dirty = true;
        }
        // 저장소 호출은 inner 미보유 상태에서 실행한다. 그 뒤 브리지를 깨워 재폴링시켜 목록에서
        // 사라진 걸 빠르게 반영한다(first-writer-wins — 해소된 id는 no-op).
        if let Some(repository) = self.shared.repository.as_ref()
            && let Err(e) = repository.resolve_approval(id, allowed, remember, now)
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
        uuid: &str,
        last_seq: u64,
    ) -> Option<(u64, Arc<runtime::TerminalViewportSnapshot>)> {
        let published = self.shared.published.lock().expect("published lock");
        published
            .viewports
            .get(uuid)
            .filter(|(seq, _)| *seq > last_seq)
            .map(|(seq, snapshot)| (*seq, Arc::clone(snapshot)))
    }

    /// 지금까지의 승인 DB 폴링 횟수(테스트).
    pub fn poll_count(&self) -> u64 {
        self.shared.poll_count.load(Ordering::SeqCst)
    }

    /// 지금까지의 Dashboard JSON 재구축 횟수(테스트 — PR-F1 게이트 검증).
    pub fn dash_build_count(&self) -> u64 {
        self.shared.dash_build_count.load(Ordering::SeqCst)
    }

    /// 브리지 스레드에 종료를 알린다(서버 shutdown이 join 전에 호출).
    pub fn stop(&self) {
        let _inner = self.shared.inner.lock().expect("dashboard inner lock");
        self.shared.stop.store(true, Ordering::SeqCst);
        self.shared.cvar.notify_all();
    }

    /// 테스트 전용: 런타임 receiver 없이 이벤트 하나를 세션 맵에 반영하고 브리지를 깨운다
    /// (상태 스트림 프레임을 실제 WS로 검증하기 위한 주입 시드). Viewport 이벤트는 run()의
    /// drain과 동일하게 시청 중일 때만 슬롯에 반영한다 (P5c).
    #[cfg(test)]
    pub fn inject_event(&self, event: RuntimeEvent) {
        {
            let mut inner = self.shared.inner.lock().expect("dashboard inner lock");
            let Inner {
                sessions,
                resource,
                watchers,
                bracketed,
                ids,
                ..
            } = &mut *inner;
            let relevant = apply_event(sessions, resource, ids, &event);
            // 실경로(run)와 동일하게 inner 임계구역 안에서 published를 중첩 취득해
            // 슬롯을 반영한다 (P5 리뷰 ②-P1 — 락 순서 inner→published). 키는 UUID (I1).
            if let RuntimeEvent::Viewport {
                session,
                snapshot,
                bracketed_paste,
            }
            | RuntimeEvent::ViewportTracked {
                session,
                snapshot,
                bracketed_paste,
                ..
            } = &event
                && let Some(uuid) = ids.uuid(session.0)
                && watchers.contains_key(uuid)
            {
                let uuid = uuid.to_owned();
                bracketed.insert(uuid.clone(), *bracketed_paste);
                let mut published = self.shared.published.lock().expect("published lock");
                let entry = published
                    .viewports
                    .entry(uuid)
                    .or_insert((0, Arc::clone(snapshot)));
                entry.0 += 1;
                entry.1 = Arc::clone(snapshot);
            }
            inner.dirty = true;
            // 실경로(run)와 동일 게이트 — 대시보드 관련 이벤트만 재구축을 켠다 (PR-F1).
            inner.dashboard_dirty |= relevant;
        }
        self.shared.cvar.notify_all();
    }
}

/// 승인 행을 표시 뷰로. `arguments_preview`는 proxy가 이미 redact한 텍스트다.
///
/// 세션 해석(I2 딥링크): `pane_id`(= 런타임 세션 키 `{ws}:{u64}`)를 파싱해 u64를 얻고,
/// `ids`로 폰이 아는 영속 UUID로 바꾼다. **DB 조인으로는 못 찾는다** — mux_panes.id는
/// 다른 식별자 공간이라 예전 조인은 매칭된 적이 없다(2026-07-17 실측, 그래서 이 필드가
/// 늘 None이었고 "화면 보기" 버튼이 아예 안 떴다).
///
/// `ids`는 **활성 워커**의 mux에서 만들어지므로 다른 워크스페이스의 승인은 None으로
/// 남는다 — 그게 맞다: u64는 워크스페이스마다 1부터라 남의 번호로 조회하면 엉뚱한
/// 세션이 잡힌다(모듈 상단 앨리어싱 경고).
fn approval_views(rows: Vec<PendingApprovalRecord>, ids: &IdMap) -> Vec<ApprovalView> {
    rows.into_iter()
        .map(|row| {
            let session = row
                .pane_id
                .as_deref()
                .and_then(deppy_core::parse_session_key)
                .and_then(|(_, session)| ids.uuid(session.0).map(str::to_owned));
            ApprovalView {
                id: row.id,
                server: row.server_id,
                tool: row.tool_name,
                preview: row.arguments_preview,
                created_at: row.created_at,
                session,
                // 제목은 폰이 이미 받은 세션 목록에서 찾는다(중복 전송 불필요) —
                // 목록에 없을 때만 쓰는 폴백 자리라 지금은 비운다.
                session_title: None,
            }
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
        // Dashboard 재구축 게이트 (PR-F1) — setter/등록이 세운 플래그를 이번 회차로 가져오고,
        // drain 중 대시보드 관련 이벤트(apply_event=true)가 있으면 함께 켠다.
        let mut rebuild_dashboard = std::mem::take(&mut inner.dashboard_dirty);

        // 1) 런타임 이벤트 drain — 접속 유무와 무관하게 처리해 durable 큐 overflow를 막는다.
        //    시청 중 세션의 Viewport/입력압박은 스테이징해 두었다가 슬롯에 반영한다
        //    (P5c/P6a — 세션별 최신본만, coalesce).
        let mut staged_viewports: BTreeMap<String, Arc<runtime::TerminalViewportSnapshot>> =
            BTreeMap::new();
        let mut staged_pressure: BTreeMap<String, String> = BTreeMap::new();
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
                bracketed,
                ids,
                ..
            } = &mut *inner;
            for event in &events {
                rebuild_dashboard |= apply_event(sessions, resource, ids, event);
                // 세션 상태 전이(입력대기/완료)를 웹푸시로 넘긴다 — 앱이 닫혀 있어도 알린다(P4).
                // notify_session이 Done/Waiting 외 상태는 무시하므로 여기서는 걸러내지 않는다.
                if let Some(push) = push_sink.as_ref() {
                    forward_to_push(push, ids, event);
                }
                // 슬롯 키는 **영속 UUID**다(폰이 u64를 모른다). UUID를 모르는 세션은
                // 애초에 시청 대상이 아니다(watchers는 UUID 키) — 자연스럽게 걸러진다.
                match event {
                    RuntimeEvent::Viewport {
                        session,
                        snapshot,
                        bracketed_paste,
                    }
                    | RuntimeEvent::ViewportTracked {
                        session,
                        snapshot,
                        bracketed_paste,
                        ..
                    } => {
                        if let Some(uuid) = ids.uuid(session.0)
                            && watchers.contains_key(uuid)
                        {
                            let uuid = uuid.to_owned();
                            staged_viewports.insert(uuid.clone(), Arc::clone(snapshot));
                            bracketed.insert(uuid, *bracketed_paste);
                        }
                    }
                    RuntimeEvent::PtyInputPressure { session, pressure } => {
                        if let Some(uuid) = ids.uuid(session.0)
                            && watchers.contains_key(uuid)
                        {
                            staged_pressure.insert(
                                uuid.to_owned(),
                                ServerMsg::InputPressure {
                                    session: uuid.to_owned(),
                                    queued: pressure.queued_bytes,
                                    reason: pressure_reason(pressure.reason),
                                }
                                .encode(),
                            );
                        }
                    }
                    _ => {}
                }
            }
        }
        // 시청 화면/입력압박 슬롯 반영 — watcher 판정과 **같은 inner 임계구역에서**
        // published를 중첩 취득해 삽입한다 (P5 리뷰 ②-P1: inner 해제 후 삽입하면 그 사이
        // 완주한 rebind_watch(마지막 이탈)의 슬롯 제거를 덮어 시청 0 슬롯이 부활·잔존한다).
        if !staged_viewports.is_empty() || !staged_pressure.is_empty() {
            let mut published = shared.published.lock().expect("published lock");
            for (session, snapshot) in std::mem::take(&mut staged_viewports) {
                let entry = published
                    .viewports
                    .entry(session)
                    .or_insert((0, Arc::clone(&snapshot)));
                entry.0 += 1;
                entry.1 = snapshot;
            }
            for (session, json) in std::mem::take(&mut staged_pressure) {
                let entry = published
                    .input_pressure
                    .entry(session)
                    .or_insert((0, String::new()));
                entry.0 += 1;
                entry.1 = json;
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
            match shared.repository.as_ref() {
                Some(repository) => match repository.list_pending_approvals(PENDING_APPROVAL_LIMIT)
                {
                    Ok(rows) => approvals = Some(approval_views(rows, &inner.ids)),
                    Err(e) => tracing::warn!("web-remote 승인 목록 폴링 실패: {e:#}"),
                },
                None => approvals = Some(Vec::new()),
            }
        }

        // 2.5) 원격 시청 lease 갱신 (P5b) — 만기(45s TTL) 전에 재전송해 시청을 유지한다.
        //      시청 0이면 no-op. inner 락 안에서 보내 rebind의 on/off와 순서를 직렬화한다
        //      (P5 리뷰 — sink는 try_send+unpark 비블로킹이라 락 하 호출 안전).
        let renewals =
            due_lease_renewals(&mut inner.watchers, Instant::now(), LEASE_RENEW_INTERVAL);
        if !renewals.is_empty() {
            let commands: Vec<RuntimeCommand> = renewals
                .iter()
                .filter_map(|uuid| inner.ids.session(uuid))
                .map(|session| lease_command(session, true))
                .collect();
            if let Some(sink) = &inner.command_sink {
                for command in commands {
                    sink(command);
                }
            }
        }

        // 3) 발행 — 접속 0이면 JSON을 만들지 않는다(불필요 작업 회피). 접속 시 등록이
        //    force_poll+dirty를 세우므로 그때 최신 스냅샷이 만들어진다.
        //    Dashboard JSON은 재구축 게이트가 켜졌거나 승인 폴링 회차일 때만 다시 만든다
        //    (PR-F1) — Viewport wake만으로는 직렬화/비교를 돌지 않는다.
        if conns > 0 {
            let dash_json = (rebuild_dashboard || should_poll).then(|| {
                shared.dash_build_count.fetch_add(1, Ordering::SeqCst);
                ServerMsg::Dashboard {
                    workspaces: workspace_views(&inner.workspaces, &inner.sessions, &inner.ids),
                    resource: inner.resource.clone(),
                    notice: inner.notice.clone(),
                }
                .encode()
            });
            let appr_json = approvals.map(|pending| ServerMsg::Approvals { pending }.encode());
            drop(inner);
            publish(shared, dash_json, appr_json);
        } else if rebuild_dashboard {
            // 접속 0 — 발행은 생략하되 재구축 필요는 보존해, 다음 접속 등록 회차에
            // 이번 변화가 반영된 프레임이 만들어지게 한다 (register_connection도 세우지만
            // 등록과 이 회차가 경합해도 유실되지 않게 되돌려 둔다).
            inner.dashboard_dirty = true;
            drop(inner);
        } else {
            drop(inner);
        }
    }
}

/// 발행 스냅샷을 갱신한다 — 내용이 바뀐 것만 버전을 올려 접속 스레드가 재전송하게 한다.
/// `dashboard_json`은 재구축 게이트가 켜진 회차에만 Some이다 (PR-F1).
fn publish(shared: &Arc<Shared>, dashboard_json: Option<String>, approvals_json: Option<String>) {
    let mut published = shared.published.lock().expect("published lock");
    if let Some(dashboard_json) = dashboard_json
        && published.dashboard_json != dashboard_json
    {
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

    /// 세션 u64 → 테스트용 영속 UUID (실제로는 sessions.id UUID).
    fn test_uuid(session: u64) -> String {
        format!("uuid-{session}")
    }

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
                            // 테스트용 결정적 UUID — 실제로는 sessions.id (I1)
                            persistent_session_id: Some(test_uuid(*id)),
                        })
                        .collect(),
                }],
                active_tab: Some(runtime::MuxTabId("tab-1".into())),
                focused_pane: Some(runtime::MuxPaneId("p-1".into())),
            }),
        }
    }

    #[test]
    fn mux는_세션_소속을_만든다() {
        // 제목은 앱 스냅샷(WorkspaceSeed) 몫 — mux는 소속(멤버십)만 정한다.
        let mut sessions = BTreeMap::new();
        let mut resource = None;
        let mut ids = IdMap::default();
        apply_event(
            &mut sessions,
            &mut resource,
            &mut ids,
            &mux_event(&[(10, "claude")]),
        );
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[&10].status, SessionStatus::Running);
    }

    #[test]
    fn durable_event_barrier_is_not_dashboard_or_session_data() {
        let mut sessions = BTreeMap::new();
        let mut resource = None;
        let mut ids = IdMap::default();

        assert!(!apply_event(
            &mut sessions,
            &mut resource,
            &mut ids,
            &RuntimeEvent::DurableEventBarrierReached { correlation_id: 1 },
        ));
        assert!(sessions.is_empty());
        assert!(resource.is_none());
        assert!(ids.to_session.is_empty());
        assert!(ids.to_uuid.is_empty());
    }

    /// 표시명은 앱 스냅샷에서 오고, mux의 raw 제목이 덮지 않는다 (폰에서 사람이 읽는 이름).
    #[test]
    fn 표시명은_앱_스냅샷에서_오고_비활성은_표시전용이다() {
        let mut live = BTreeMap::new();
        live.insert(
            10,
            SessionEntry {
                status: SessionStatus::NeedsApproval,
                exited: false,
            },
        );
        let seeds = vec![
            WorkspaceSeed {
                id: "ws-1".into(),
                name: "deppy-sijo".into(),
                state: WorkspaceState::Active,
                sessions: vec![SessionSeed {
                    id: Some(10),
                    title: "deppy-sijo".into(),
                    status: Some(SessionStatus::Running),
                    agent: Some("Claude · sonnet · high".into()),
                    exited: false,
                }],
            },
            WorkspaceSeed {
                id: "ws-2".into(),
                name: "source".into(),
                state: WorkspaceState::Warm,
                sessions: vec![SessionSeed {
                    id: Some(10), // 앱이 실수로 id를 넣어도 브리지가 표시 전용으로 만든다
                    title: "deppy-mux".into(),
                    status: None,
                    agent: None,
                    exited: false,
                }],
            },
        ];
        // I1: 활성 세션의 UUID 매핑(MuxUpdated 유래)이 있어야 시청 가능한 id가 나온다
        let mut ids = IdMap::default();
        ids.insert(test_uuid(10), 10);
        let views = workspace_views(&seeds, &live, &ids);
        assert_eq!(views.len(), 2);
        // 활성: 라이브 상태(needs_approval) + 앱이 해석한 표시명
        assert_eq!(
            views[0].sessions[0].id.as_deref(),
            Some(test_uuid(10).as_str())
        );
        assert_eq!(views[0].sessions[0].title, "deppy-sijo");
        assert_eq!(views[0].sessions[0].status, Some("needs_approval"));
        // 돌고 있는 에이전트 요약이 실린다(폰에서 "무슨 에이전트가 도는지" 확인)
        assert_eq!(
            views[0].sessions[0].agent.as_deref(),
            Some("Claude · sonnet · high")
        );
        // 비활성: 표시 전용(id 없음 — 세션 id는 worker-로컬이라 시청 불가)
        assert_eq!(views[1].state, "warm");
        assert_eq!(views[1].sessions[0].id, None);
        assert_eq!(views[1].sessions[0].title, "deppy-mux");
    }

    #[test]
    fn 상태_이벤트가_세션_상태를_덮고_mux는_상태를_보존한다() {
        let mut sessions = BTreeMap::new();
        let mut resource = None;
        let mut ids = IdMap::default();
        apply_event(
            &mut sessions,
            &mut resource,
            &mut ids,
            &mux_event(&[(10, "claude")]),
        );
        apply_event(
            &mut sessions,
            &mut resource,
            &mut ids,
            &RuntimeEvent::SessionStatusChanged {
                session: SessionId(10),
                status: SessionStatus::NeedsApproval,
            },
        );
        assert_eq!(sessions[&10].status, SessionStatus::NeedsApproval);
        // 재발화된 MuxUpdated가 상태를 리셋하지 않아야 한다
        apply_event(
            &mut sessions,
            &mut resource,
            &mut ids,
            &mux_event(&[(10, "claude-2")]),
        );
        assert_eq!(sessions[&10].status, SessionStatus::NeedsApproval);
    }

    #[test]
    fn exit는_완료_배지_mux에서_사라지면_목록에서_제거() {
        let mut sessions = BTreeMap::new();
        let mut resource = None;
        let mut ids = IdMap::default();
        apply_event(
            &mut sessions,
            &mut resource,
            &mut ids,
            &mux_event(&[(10, "a"), (11, "b")]),
        );
        apply_event(
            &mut sessions,
            &mut resource,
            &mut ids,
            &RuntimeEvent::SessionExited {
                session: SessionId(11),
                exit_code: Some(0),
            },
        );
        assert!(sessions[&11].exited);
        assert_eq!(sessions[&11].status, SessionStatus::Done);
        // pane이 닫히면(다음 MuxUpdated에서 빠지면) 목록에서 제거
        apply_event(
            &mut sessions,
            &mut resource,
            &mut ids,
            &mux_event(&[(10, "a")]),
        );
        assert!(!sessions.contains_key(&11));
    }

    #[test]
    fn resource_usage가_리소스뷰를_만든다() {
        let mut sessions = BTreeMap::new();
        let mut resource = None;
        let mut ids = IdMap::default();
        apply_event(
            &mut sessions,
            &mut resource,
            &mut ids,
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

    /// 브리지에 UUID↔u64 매핑을 심는다 — 실경로에서는 MuxUpdated가 채운다 (I1).
    fn seed_ids(handle: &DashboardHandle, sessions: &[u64]) {
        let mut inner = handle.shared.inner.lock().unwrap();
        for s in sessions {
            inner.ids.insert(test_uuid(*s), *s);
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
        seed_ids(&handle, &[1, 2, 3, 5, 7, 8, 9, 10]);
        // conn1: watch 7 (0→1 on) / conn2: watch 7 (1→2 무전송)
        handle.rebind_watch(None, Some(&test_uuid(7)));
        handle.rebind_watch(None, Some(&test_uuid(7)));
        // conn1: 7→9 전환 (7은 2→1 무전송, 9는 0→1 on) — 재바인딩
        handle.rebind_watch(Some(&test_uuid(7)), Some(&test_uuid(9)));
        // 같은 세션으로의 재전환은 no-op
        handle.rebind_watch(Some(&test_uuid(9)), Some(&test_uuid(9)));
        // conn2 종료 (7: 1→0 off), conn1 종료 (9: 1→0 off)
        handle.rebind_watch(Some(&test_uuid(7)), None);
        handle.rebind_watch(Some(&test_uuid(9)), None);
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
    fn request_switch는_스위치_싱크로_워크스페이스_id를_넘긴다() {
        // 미러 진입(I1b-2) — 폰 Switch가 앱 전환 채널로 그대로 전달돼야 한다.
        let (handle, thread) = DashboardHandle::spawn(None);
        let captured: StdArc<Mutex<Vec<String>>> = StdArc::default();
        let cap = StdArc::clone(&captured);
        let sink: SwitchSink = StdArc::new(move |ws| cap.lock().unwrap().push(ws));
        handle.set_switch_sink(sink);
        handle.request_switch("ws-2");
        handle.request_switch("ws-7");
        assert_eq!(
            *captured.lock().unwrap(),
            vec!["ws-2".to_owned(), "ws-7".to_owned()]
        );
        handle.stop();
        thread.join().unwrap();
    }

    #[test]
    fn switch_싱크_미설정이면_request_switch가_무시된다() {
        // 싱크 없이(뷰어 전용/테스트) 호출해도 패닉/블록 없이 조용히 무시한다.
        let (handle, thread) = DashboardHandle::spawn(None);
        handle.request_switch("ws-1");
        handle.stop();
        thread.join().unwrap();
    }

    #[test]
    fn 새_worker_구독은_시청을_정리하고_재선언하지_않는다() {
        // P5 리뷰 P2: SessionId는 worker-로컬이라 새 worker에 옛 시청 id를 재선언하면
        // 무관한 세션이 승격된다 — 전환 시에는 시청 상태를 정리해야 한다.
        let (handle, thread) = DashboardHandle::spawn(None);
        let (sink, captured) = capture_sink();
        handle.set_command_sink(sink);
        seed_ids(&handle, &[1, 2, 3, 5, 7, 8, 9, 10]);
        handle.rebind_watch(None, Some(&test_uuid(3)));
        handle.rebind_watch(None, Some(&test_uuid(5)));
        handle.inject_event(viewport_event(3));
        assert!(handle.viewport_if_newer(&test_uuid(3), 0).is_some());
        captured.lock().unwrap().clear();
        // 새 worker 구독 시점의 정리 경로 (set_runtime_source가 호출)
        {
            let mut inner = handle.shared.inner.lock().unwrap();
            DashboardHandle::clear_watch_state(&mut inner, &handle.shared);
        }
        // 재선언 lease가 나가면 안 되고(앨리어싱), 슬롯도 비워져야 한다
        assert!(
            captured.lock().unwrap().is_empty(),
            "정리 경로에서 lease 명령이 나감 — 세션 id 앨리어싱 위험"
        );
        assert!(
            handle.viewport_if_newer(&test_uuid(3), 0).is_none(),
            "슬롯 잔존"
        );
        // 정리 후 새 watch는 fresh 0→1로 정상 동작한다
        handle.rebind_watch(None, Some(&test_uuid(3)));
        let got: Vec<(u64, bool)> = captured
            .lock()
            .unwrap()
            .iter()
            .filter_map(lease_of)
            .collect();
        assert_eq!(got, vec![(3, true)]);
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
                attrs: Default::default(),
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
        seed_ids(&handle, &[1, 2, 3, 5, 7, 8, 9, 10]);
        // 시청 전 Viewport — 슬롯에 쌓이지 않는다 (메모리 유계)
        handle.inject_event(viewport_event(7));
        assert!(handle.viewport_if_newer(&test_uuid(7), 0).is_none());
        // 시청 시작 → 이벤트마다 seq 증가
        handle.rebind_watch(None, Some(&test_uuid(7)));
        handle.inject_event(viewport_event(7));
        let (seq1, _) = handle
            .viewport_if_newer(&test_uuid(7), 0)
            .expect("슬롯 없음");
        handle.inject_event(viewport_event(7));
        let (seq2, _) = handle
            .viewport_if_newer(&test_uuid(7), 0)
            .expect("슬롯 없음");
        assert!(seq2 > seq1);
        // 이미 본 seq — None (불필요 재전송 방지)
        assert!(handle.viewport_if_newer(&test_uuid(7), seq2).is_none());
        // 다른(비시청) 세션은 여전히 없음
        handle.inject_event(viewport_event(8));
        assert!(handle.viewport_if_newer(&test_uuid(8), 0).is_none());
        // 마지막 시청자 이탈 → 슬롯 제거 (trailing 스냅샷 없음)
        handle.rebind_watch(Some(&test_uuid(7)), None);
        assert!(handle.viewport_if_newer(&test_uuid(7), 0).is_none());
        handle.stop();
        thread.join().unwrap();
    }

    #[test]
    fn viewport_wake만으로는_dashboard를_재구축하지_않는다() {
        // PR-F1 게이트: 시청 스트리밍의 Viewport 이벤트는 화면 슬롯만 갱신하고,
        // Dashboard JSON 재구축(직렬화+비교)은 대시보드 관련 변화가 있을 때만 돈다.
        let (handle, thread) = DashboardHandle::spawn(None);
        let _conn = handle.register_connection();
        seed_ids(&handle, &[7]);
        handle.rebind_watch(None, Some(&test_uuid(7)));
        // 등록이 켠 초기 재구축이 소진될 때까지 대기 (첫 Dashboard 프레임 발행 확인).
        let deadline = Instant::now() + Duration::from_secs(2);
        while handle.dashboard_if_newer(0).is_none() {
            assert!(Instant::now() < deadline, "초기 Dashboard 미발행");
            std::thread::sleep(Duration::from_millis(5));
        }
        let counters = || (handle.dash_build_count(), handle.poll_count());
        let (base_builds, base_polls) = counters();

        // Viewport만 반복 주입 — 슬롯 seq는 증가하고 폴 외 재구축은 없어야 한다.
        // 느린 공유 CI 러너 대응(2026-08-04): 원래는 위 주석의 "승인 폴 만기(1s)가 이 구간
        // 밖"이라는 벽시계 가정으로 builds 등호를 판정했다. 폴 빌드는 poll_count와 함께
        // 오륜다(폴 카운트 증가 → 빌드 순서) — builds를 먼저 읽고 polls를 나중에 읽는
        // counters()는 Δbuilds ≤ Δpolls를 깨지 않으므로, 등호 대신 "모든 빌드가 폴로
        // 설명된다"로 판정하면 러너 속도와 무관하다.
        for _ in 0..5 {
            handle.inject_event(viewport_event(7));
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some((seq, _)) = handle.viewport_if_newer(&test_uuid(7), 0)
                && seq >= 5
            {
                break;
            }
            assert!(Instant::now() < deadline, "viewport 슬롯 미갱신");
            std::thread::sleep(Duration::from_millis(5));
        }
        let (builds, polls) = counters();
        assert!(
            builds - base_builds <= polls - base_polls,
            "Viewport wake가 Dashboard 재구축을 유발함 (PR-F1 게이트 회귀) \
             [builds {base_builds}→{builds}, polls {base_polls}→{polls}]"
        );

        // 대시보드 관련 이벤트(상태 변화)는 재구축을 켠다.
        handle.inject_event(RuntimeEvent::SessionStatusChanged {
            session: SessionId(7),
            status: SessionStatus::NeedsApproval,
        });
        let (base_builds, base_polls) = counters();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let (builds, polls) = counters();
            if builds - base_builds > polls - base_polls {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "상태 이벤트가 재구축을 켜지 않음"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        handle.stop();
        thread.join().unwrap();
    }

    #[test]
    fn encode_input은_정규화_strip_wrap_submit을_정확히_한다() {
        // 단순 한 줄 + submit → 텍스트 + \r
        assert_eq!(
            encode_input("ls -al", true, false).unwrap(),
            b"ls -al\r".to_vec()
        );
        // submit 없음 — 삽입만
        assert_eq!(
            encode_input("/tmp/photo.png ", false, false).unwrap(),
            b"/tmp/photo.png ".to_vec()
        );
        // C0 제어문자 strip(\t 제외) — 클라이언트發 이스케이프 주입 차단
        assert_eq!(
            encode_input("a\x1b[31mb\x07c\td", true, false).unwrap(),
            b"a[31mbc\td\r".to_vec()
        );
        // 개행 정규화: \r\n·\r → \n → \r, 여러 줄 = paste지만 모드 off면 wrap 없음
        assert_eq!(
            encode_input("one\r\ntwo\rthree", false, false).unwrap(),
            b"one\rtwo\rthree".to_vec()
        );
        // 여러 줄 + bracketed on → wrap, submit의 \r는 wrap 밖
        assert_eq!(
            encode_input("one\ntwo", true, true).unwrap(),
            b"\x1b[200~one\rtwo\x1b[201~\r".to_vec()
        );
        // 한 줄이라도 512B 초과면 paste 취급
        let long = "x".repeat(600);
        let encoded = encode_input(&long, false, true).unwrap();
        assert!(encoded.starts_with(b"\x1b[200~") && encoded.ends_with(b"\x1b[201~"));
        // 빈 입력 + submit 없음 → None, submit 있으면 Enter만
        assert!(encode_input("", false, true).is_none());
        assert_eq!(encode_input("", true, false).unwrap(), b"\r".to_vec());
    }

    #[test]
    fn send_input은_시청_세션의_bracketed_모드를_반영한다() {
        let (handle, thread) = DashboardHandle::spawn(None);
        let (sink, captured) = capture_sink();
        handle.set_command_sink(sink);
        seed_ids(&handle, &[1, 2, 3, 5, 7, 8, 9, 10]);
        handle.rebind_watch(None, Some(&test_uuid(7)));
        // bracketed on인 Viewport 주입 → 캐시 갱신
        let RuntimeEvent::Viewport {
            session, snapshot, ..
        } = viewport_event(7)
        else {
            unreachable!()
        };
        handle.inject_event(RuntimeEvent::Viewport {
            session,
            snapshot,
            bracketed_paste: true,
        });
        captured.lock().unwrap().clear();
        handle.send_input(&test_uuid(7), "a\nb", true);
        let inputs: Vec<Vec<u8>> = captured
            .lock()
            .unwrap()
            .iter()
            .filter_map(|command| match command {
                RuntimeCommand::WriteInput { bytes, .. } => Some(bytes.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(inputs, vec![b"\x1b[200~a\rb\x1b[201~\r".to_vec()]);
        handle.stop();
        thread.join().unwrap();
    }

    /// 리뷰 P2-1 회귀: 워크스페이스 전환으로 시청이 정리되면(clear_watch_state) 접속이
    /// 옛 세션 id를 들고 있어도 명령이 나가지 않는다 — 새 워커의 동명 id 세션(worker-로컬
    /// 카운터라 재배정됨)에 입력이 주입되는 것을 브리지가 최종 차단한다.
    #[test]
    fn 시청이_정리되면_입력_키_스크롤이_모두_차단된다() {
        let (handle, thread) = DashboardHandle::spawn(None);
        let (sink, captured) = capture_sink();
        handle.set_command_sink(sink);
        seed_ids(&handle, &[1, 2, 3, 5, 7, 8, 9, 10]);
        handle.rebind_watch(None, Some(&test_uuid(7)));
        handle.send_input(&test_uuid(7), "before", true);
        assert!(
            captured
                .lock()
                .unwrap()
                .iter()
                .any(|c| matches!(c, RuntimeCommand::WriteInput { .. })),
            "시청 중인데 입력이 안 나갔다"
        );
        // 워크스페이스 전환 — 브리지가 시청 집계를 비운다
        {
            let mut inner = handle.shared.inner.lock().unwrap();
            DashboardHandle::clear_watch_state(&mut inner, &handle.shared);
        }
        captured.lock().unwrap().clear();
        // 접속(ws_api)은 여전히 watched=Some(7)이라 이 함수들을 부른다 — 전부 차단돼야 한다
        handle.send_input(&test_uuid(7), "yes", true);
        handle.send_key(&test_uuid(7), "enter");
        handle.send_scroll(&test_uuid(7), 5);
        assert!(
            captured.lock().unwrap().is_empty(),
            "시청 정리 후에도 명령이 나갔다 — 새 워커의 동명 세션에 입력이 주입된다"
        );
        handle.stop();
        thread.join().unwrap();
    }

    /// I1 핵심 계약: **워커가 모르는 UUID로는 명령이 만들어지지 않는다.**
    /// 폰은 u64를 아예 모르므로(프로토콜이 UUID만 노출) 워크스페이스 전환·재시작 후
    /// 옛 식별자로 엉뚱한 세션을 잡는 일이 구조적으로 불가능하다.
    #[test]
    fn 모르는_uuid로는_시청도_입력도_나가지_않는다() {
        let (handle, thread) = DashboardHandle::spawn(None);
        let (sink, captured) = capture_sink();
        handle.set_command_sink(sink);
        // 워커가 아는 세션은 10뿐 — 다른 워크스페이스의 UUID는 매핑에 없다
        seed_ids(&handle, &[10]);
        let stranger = "uuid-from-another-workspace";
        handle.rebind_watch(None, Some(stranger));
        handle.send_input(stranger, "yes", true);
        handle.send_key(stranger, "enter");
        handle.send_scroll(stranger, 3);
        assert!(
            captured.lock().unwrap().is_empty(),
            "모르는 UUID로 명령이 나갔다 — 앨리어싱 차단 실패"
        );
        // 아는 UUID는 정상 동작한다(게이트가 과잉 차단하지 않는다)
        handle.rebind_watch(None, Some(&test_uuid(10)));
        handle.send_input(&test_uuid(10), "ls", true);
        assert!(
            captured.lock().unwrap().iter().any(
                |c| matches!(c, RuntimeCommand::WriteInput { session, .. } if session.0 == 10)
            ),
            "아는 UUID인데 입력이 안 나갔다"
        );
        handle.stop();
        thread.join().unwrap();
    }

    #[test]
    fn 확장_키_화이트리스트는_시퀀스로_매핑되고_미지_키는_무시된다() {
        let (handle, thread) = DashboardHandle::spawn(None);
        let (sink, captured) = capture_sink();
        handle.set_command_sink(sink);
        seed_ids(&handle, &[1, 2, 3, 5, 7, 8, 9, 10]);
        handle.rebind_watch(None, Some(&test_uuid(7))); // 시청 중이어야 명령이 나간다(P2-1 게이트)
        captured.lock().unwrap().clear();
        for key in ["up", "esc", "shift_tab", "rm_rf"] {
            handle.send_key(&test_uuid(7), key);
        }
        let inputs: Vec<Vec<u8>> = captured
            .lock()
            .unwrap()
            .iter()
            .filter_map(|command| match command {
                RuntimeCommand::WriteInput { bytes, .. } => Some(bytes.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            inputs,
            vec![b"\x1b[A".to_vec(), b"\x1b".to_vec(), b"\x1b[Z".to_vec()]
        );
        handle.stop();
        thread.join().unwrap();
    }

    /// 리뷰 P1-1 회귀: 매 프레임 표시 스냅샷 push가 **런타임 이벤트 상태를 덮지 않는다**.
    /// (앱의 상태 뷰는 창이 숨겨지면 얼어붙는데, 그 stale 값이 브리지의 최신 상태를
    /// 되돌리면 폰이 영구 오표시된다 — 이벤트는 edge-trigger라 재발화되지 않는다.)
    #[test]
    fn 표시_스냅샷_push는_런타임_상태를_되돌리지_않는다() {
        let (handle, thread) = DashboardHandle::spawn(None);
        let active = |status: Option<SessionStatus>| WorkspaceSeed {
            id: "ws-1".into(),
            name: "p".into(),
            state: WorkspaceState::Active,
            sessions: vec![SessionSeed {
                id: Some(7),
                title: "deppy-sijo".into(),
                status,
                agent: None,
                exited: false,
            }],
        };
        // 재구독 시점: Running으로 시드
        handle.reseed_active_sessions(&active(Some(SessionStatus::Running)).sessions);
        handle.set_workspaces(vec![active(Some(SessionStatus::Running))]);
        // 런타임 이벤트: 완료 도착 (브리지가 즉시 반영 — 프레임 독립)
        handle.inject_event(RuntimeEvent::SessionStatusChanged {
            session: SessionId(7),
            status: SessionStatus::Done,
        });
        // 앱이 stale 상태(Running)로 표시 스냅샷을 다시 push (제목만 바뀐 상황)
        let mut stale = active(Some(SessionStatus::Running));
        stale.sessions[0].title = "deppy-sijo (2)".into();
        handle.set_workspaces(vec![stale]);
        // 상태는 여전히 done이어야 한다 — 표시명만 갱신된다
        let views = {
            let inner = handle.shared.inner.lock().unwrap();
            workspace_views(&inner.workspaces, &inner.sessions, &inner.ids)
        };
        assert_eq!(views[0].sessions[0].status, Some("done"), "상태가 되돌아감");
        assert_eq!(views[0].sessions[0].title, "deppy-sijo (2)");
        handle.stop();
        thread.join().unwrap();
    }

    #[test]
    fn 갱신주기가_지난_시청만_갱신_대상이_된다() {
        let mut watchers = BTreeMap::new();
        let now = Instant::now();
        watchers.insert(
            test_uuid(1),
            WatcherEntry {
                count: 1,
                last_renewal: now - Duration::from_secs(20),
            },
        );
        watchers.insert(
            test_uuid(2),
            WatcherEntry {
                count: 1,
                last_renewal: now,
            },
        );
        let due = due_lease_renewals(&mut watchers, now, Duration::from_secs(15));
        assert_eq!(due, vec![test_uuid(1)]);
        // 갱신 직후엔 만기가 리셋돼 due가 비어야 한다 (매 tick 재전송 방지)
        assert!(due_lease_renewals(&mut watchers, now, Duration::from_secs(15)).is_empty());
    }
    /// 2026-07-17 회귀: 승인 카드의 "화면 보기"(I2 딥링크)가 만들어진 이후 한 번도
    /// 동작한 적이 없었다 — `pending_approvals.pane_id`(런타임 세션 키 `{ws}:{u64}`)를
    /// `mux_panes.id`(UUID)와 조인해 세션 UUID를 채우려 했는데 두 값이 다른 식별자
    /// 공간이라 늘 None이었고, 폰은 `session`이 없으면 버튼 자체를 안 만든다.
    /// 이제 세션 키를 파싱해 IdMap으로 UUID를 찾는다.
    #[test]
    fn 승인의_세션키가_폰이_watch할_uuid로_해석된다() {
        let mut sessions = BTreeMap::new();
        let mut resource = None;
        let mut ids = IdMap::default();
        // 활성 워커의 mux가 IdMap을 채운다(u64 7 ↔ uuid-7).
        apply_event(
            &mut sessions,
            &mut resource,
            &mut ids,
            &mux_event(&[(7, "claude")]),
        );

        let row = |id: &str, pane: Option<&str>| PendingApprovalRecord {
            id: id.to_owned(),
            server_id: "srv".to_owned(),
            tool_name: "write_file".to_owned(),
            arguments_preview: "{}".to_owned(),
            created_at: 0,
            pane_id: pane.map(str::to_owned),
        };
        let views = approval_views(
            vec![
                // proxy가 싣는 실제 형식 — 워크스페이스 uuid + 런타임 세션 번호.
                row("a1", Some("315f68b6-333f-409f-a2c5-922b9eacfd7e:7")),
                // 세션 불명(pane_id 없음) → 딥링크 없음.
                row("a2", None),
                // 다른 워크스페이스의 번호 9 — IdMap(활성 워커)에 없으니 None이어야 한다.
                // (u64는 워크스페이스마다 1부터라 남의 번호를 그대로 쓰면 오시청이 된다.)
                row("a3", Some("other-workspace-uuid:9")),
                // mux_panes.id를 넣어도(옛 조인이 기대하던 값) 세션 키가 아니라 None.
                row("a4", Some("130d9017-be25-469e-8f8d-984abacae701")),
            ],
            &ids,
        );
        let by_id = |id: &str| views.iter().find(|v| v.id == id).unwrap();
        assert_eq!(
            by_id("a1").session.as_deref(),
            Some(test_uuid(7).as_str()),
            "폰이 watch에 쓰는 영속 UUID로 해석돼야 딥링크가 뜬다"
        );
        assert_eq!(by_id("a2").session, None);
        assert_eq!(by_id("a3").session, None);
        assert_eq!(by_id("a4").session, None);
    }
}
