//! web-remote — 모바일 PWA용 내장 웹서버 (mobile-pwa 계획 v3.3 PR-P1/P2).
//!
//! 계층 (방법 B(클라우드 앱 셸) 이전 대비 모듈 경계):
//!   - [`http`]: 수제 최소 HTTP/1.1 파서/응답 — 프레임워크 없음 (auth callback.rs 관례)
//!   - [`static_srv`]: 임베드 정적 셸 + 페어링 토큰 게이트 — 이전 시 이 계층만 교체
//!   - [`ws_api`]: WS 승격 + 승인/상태 대시보드 세션 (P2 — tungstenite sync, 첫 프레임 인증)
//!   - [`dashboard`]: 대시보드 브리지 — runtime 이벤트 구독 + 승인 DB 직행 (P2)
//!   - [`protocol`]: WS JSON 프로토콜 v1 (P2)
//!   - [`pairing`]: 페어링 토큰 keyring 영속 + 접속 URL
//!
//! 스레드 모델: tokio 금지 — 단일 accept 스레드 + 접속당 블로킹 스레드
//! (runtime remote.rs spawn_accept 관례). 정적 요청은 1개 처리 후 Connection: close,
//! WS 접속은 대시보드 스트림을 위해 슬롯을 장수 점유한다. 별도 대시보드 브리지 스레드
//! 하나가 이벤트 drain/승인 폴링을 담당한다([`dashboard`]).
//! **OFF(서버 미생성) = 스레드/소켓 0. ON + 접속 0 = accept 블로킹 대기 + 브리지 park
//! (승인 폴링 정지 — idle CPU 0, egui repaint 유발 없음).**
//!
//! 바인딩: serve 모드(기본) = 127.0.0.1 평문 + `tailscale serve`가 HTTPS 종단.
//! 비-loopback 평문 bind는 거부(remote-tls-delta §2.5) — cert 모드(자체 TLS)는 후속.

use std::io::{BufReader, Read};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Context;

pub mod dashboard;
pub mod http;
pub mod pairing;
pub mod protocol;
pub mod push;
pub mod relay;
pub mod relay_client;
pub mod repository;
pub mod session_core;
pub mod static_srv;
pub mod upload;
pub mod ws_api;

/// 정적 요청(비-WS) 동시 처리 상한. 초과 접속은 503으로 거부하지 않고 슬롯이 빌 때까지
/// OS backlog에서 대기시킨다 — 브라우저의 병렬 자산 요청(보통 ≤6)이 깨지지 않는다. 정적
/// 요청은 1개 처리 후 즉시 종료(Connection: close)라 슬롯이 빠르게 회전한다. WS는 이 슬롯을
/// 쓰지 않는다(업그레이드 시 [`MAX_WS_CONNECTIONS`]로 이관) — 장수 WS가 정적 자산 요청을
/// 굶기지 않게 한다(P2 리뷰).
pub const MAX_CONNECTIONS: usize = 3;

/// 동시 WS(대시보드) 접속 상한. 업그레이드 확정 시점에 정적 슬롯을 반납하고 이 카운터로
/// 이관하며, 상한 초과면 즉시 503으로 거부한다. 계획의 폰 1~2대 가정 + 여유로 3을 둔다.
pub const MAX_WS_CONNECTIONS: usize = 3;

/// 요청 head 총 수신 데드라인 겸 read/write syscall 타임아웃 — 침묵·트리클
/// peer(slowloris)의 접속 슬롯 점유에 wall-clock 상한을 둔다.
const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// `/upload` 본문(최대 10MB) 수신 데드라인 — 5초(READ_TIMEOUT)로는 느린 모바일 uplink에서
/// 대용량 사진/문서가 완주 못 해 400으로 실패한다(리뷰 P6d P2-2). 토큰 인가 후에만 적용되고
/// (아래 gate), stall(무데이터)은 여전히 5초 read syscall 타임아웃이 잡으므로 slowloris
/// 표면은 늘지 않는다. 10MB / 120s ≈ 0.7Mbps 하한.
const UPLOAD_READ_TIMEOUT: Duration = Duration::from_secs(120);

/// 서버 기동 옵션.
pub struct ServeOptions {
    /// 페어링 토큰 — `/?token=` 게이트가 상수시간 비교로 검증한다.
    pub token: String,
    /// 허용 Host(ts.net 호스트명). None/빈 값이면 loopback 계열 Host만 허용.
    pub allowed_host: Option<String>,
    /// 승인/웹푸시 persistence capability. Concrete DB and storage rows remain app-owned.
    /// None이면 상태 대시보드만 동작하고 승인/웹푸시는 비활성이다.
    pub repository: Option<Arc<dyn repository::WebRemoteRepository>>,
    /// 웹푸시(P4) VAPID 키. app이 keyring에서 get_or_create해 주입한다(SecretStore 접근이 app
    /// 소유). None이거나 `repository`가 None이면 푸시 비활성(대시보드만 동작).
    pub vapid: Option<push::VapidKey>,
    /// 모바일 파일 첨부(P6d) 저장 디렉터리. app이 `logs_base/uploads`를 주입한다. None이면
    /// `POST /upload`가 404(업로드 비활성) — 테스트/미배선 시 안전한 기본값.
    pub uploads_dir: Option<std::path::PathBuf>,
}

/// 접속 스레드들이 공유하는 불변 컨텍스트.
struct ConnCtx {
    token: String,
    /// 소문자 정규화된 허용 호스트명.
    allowed_host: Option<String>,
    /// WS 대시보드 브리지 핸들(승인 resolve·이벤트 구독·발행 스냅샷 공유).
    dashboard: dashboard::DashboardHandle,
    /// 웹푸시 핸들(P4) — POST /push/subscribe 등록·공개키 노출. 비활성 시 None.
    push: Option<push::PushHandle>,
    /// 모바일 파일 첨부(P6d) 저장 디렉터리. None이면 POST /upload가 404.
    uploads_dir: Option<std::path::PathBuf>,
    /// shutdown 신호 — 장수 WS 접속이 tick마다 확인해 즉시 종료한다.
    stop: Arc<AtomicBool>,
}

/// 살아있는 접속 하나 — shutdown 시 소켓 종료 + join 대상 (remote.rs ConnEntry 관례).
struct ConnEntry {
    stream: TcpStream,
    handle: JoinHandle<()>,
}

/// 살아있는 접속 목록 + 정적/WS 슬롯 점유 카운트. `entries`는 shutdown(소켓 종료+join)을 위해
/// 정적·WS 접속을 **모두** 담고, 두 카운터가 슬롯 점유를 따로 센다. WS 승격 시 정적 슬롯을
/// 반납하고 WS 슬롯으로 이관해, 장수 WS가 정적 자산 요청의 슬롯을 굶기지 않게 한다(P2 리뷰).
struct ConnState {
    entries: Vec<ConnEntry>,
    /// 정적 요청이 점유한 슬롯 수(≤ [`MAX_CONNECTIONS`]). accept는 이 값이 상한 미만일 때만 진행.
    static_slots: usize,
    /// WS가 점유한 슬롯 수(≤ [`MAX_WS_CONNECTIONS`]).
    ws_slots: usize,
}

/// 접속 상태 + 슬롯 반납 신호. accept 스레드는 정적 슬롯 상한 초과 시 Condvar에서 기다린다.
type ConnSet = (Mutex<ConnState>, Condvar);

/// 실행 중인 웹서버. Drop/shutdown이 accept 루프·접속 스레드·대시보드 스레드를 모두 정리한다.
pub struct WebRemoteServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    accept_thread: Option<JoinHandle<()>>,
    connections: Arc<ConnSet>,
    /// 전송 중립 대시보드 코어. 서버가 만들었을 수도, 앱이 만들어 공유했을 수도 있다.
    core: Arc<session_core::SessionCore>,
    /// 이 코어를 이 서버가 만들었는가. 공유받은 코어는 서버가 멈추지 않는다 —
    /// 그러지 않으면 Tailscale을 끄는 것만으로 Relay의 대시보드까지 죽는다.
    owns_core: bool,
}

impl WebRemoteServer {
    /// serve 모드: 평문 bind. **비-loopback 평문은 거부**(remote-tls-delta §2.5 관례) —
    /// HTTPS 종단은 `tailscale serve` 몫이고, cert 모드(자체 TLS + 비-loopback bind)는
    /// 후속 구현이다(설정 자리만 예약).
    pub fn serve(addr: SocketAddr, options: ServeOptions) -> anyhow::Result<Self> {
        Self::serve_inner(addr, options, None)
    }

    /// 앱이 소유한 코어를 공유해 서버를 띄운다. 두 전송을 함께 켤 때 쓰며, 이 서버의
    /// shutdown은 공유 코어를 멈추지 않는다.
    pub fn serve_with_core(
        addr: SocketAddr,
        options: ServeOptions,
        core: Arc<session_core::SessionCore>,
    ) -> anyhow::Result<Self> {
        Self::serve_inner(addr, options, Some(core))
    }

    fn serve_inner(
        addr: SocketAddr,
        options: ServeOptions,
        shared_core: Option<Arc<session_core::SessionCore>>,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            addr.ip().is_loopback(),
            "비-loopback 평문 bind({addr}) 거부 — HTTPS 없이는 열 수 없습니다. \
             tailscale serve(127.0.0.1 프록시)를 쓰거나 cert 모드(후속)를 기다리세요"
        );
        let listener = TcpListener::bind(addr).context("web-remote bind 실패")?;
        let addr = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let connections: Arc<ConnSet> = Arc::new((
            Mutex::new(ConnState {
                entries: Vec::new(),
                static_slots: 0,
                ws_slots: 0,
            }),
            Condvar::new(),
        ));
        // WS 대시보드 브리지 스레드. OFF(서버 미생성)면 이 스레드도 없다 — 리소스 0.
        // 공유 코어를 받았으면 새로 띄우지 않는다.
        let owns_core = shared_core.is_none();
        // 웹푸시 발송기는 **코어가 소유한다**. 공유 코어에 서버가 소유한 싱크를 심으면, 서버를
        // 끄는 순간 코어가 죽은 싱크를 가리킨다 — Tailscale을 끄는 것만으로 Relay 쪽 코어가
        // 망가지는 길이다. 공유 배치에서는 소유자가 `spawn_with_push`로 만들어 오고, 여기서는
        // 그 핸들만 빌린다. 그래도 VAPID 키를 함께 넘기면 소유권이 둘로 갈리므로 거절한다.
        anyhow::ensure!(
            shared_core.is_none() || options.vapid.is_none(),
            "공유 코어의 웹푸시는 코어 소유자가 만든다 — 서버에 VAPID 키를 함께 넘길 수 없다"
        );
        let core = match shared_core {
            Some(shared) => shared,
            None => session_core::SessionCore::spawn_with_push(
                options.repository.clone(),
                options.vapid,
            ),
        };
        let push_handle = core.push_handle();
        let ctx = Arc::new(ConnCtx {
            token: options.token,
            allowed_host: options
                .allowed_host
                .map(|host| host.trim().to_ascii_lowercase())
                .filter(|host| !host.is_empty()),
            dashboard: core.dashboard().clone(),
            push: push_handle,
            uploads_dir: options.uploads_dir,
            stop: Arc::clone(&stop),
        });
        let accept_thread =
            spawn_accept(listener, ctx, Arc::clone(&stop), Arc::clone(&connections))?;
        tracing::info!(%addr, "web-remote 서버 시작");
        Ok(Self {
            addr,
            stop,
            accept_thread: Some(accept_thread),
            connections,
            core,
            owns_core,
        })
    }

    /// 이 서버가 쓰는 전송 중립 코어. 앱이 Relay와 공유할 때 쓴다.
    pub fn core(&self) -> Arc<session_core::SessionCore> {
        Arc::clone(&self.core)
    }

    /// `subscribe_with_wake`에 넘길 안정적 wake 클로저 — app이 활성 runtime을 구독할 때 쓴다.
    pub fn dashboard_wake(&self) -> Arc<dyn Fn() + Send + Sync> {
        self.core.dashboard().wake_fn()
    }

    /// 활성 workspace worker 이벤트 구독을 대시보드에 붙인다(시작 + 워크스페이스 전환마다).
    pub fn set_runtime_source(&self, receiver: runtime::RuntimeEventReceiver) {
        self.core.dashboard().set_runtime_source(receiver);
    }

    /// web → runtime 명령 싱크를 붙인다 (P5b — 시청 lease 전송용, receiver와 같은 시점에
    /// 교체). 미설정이면 터미널 뷰어만 비활성 — 대시보드/승인은 그대로 동작한다.
    pub fn set_runtime_command_sink(&self, sink: dashboard::CommandSink) {
        self.core.dashboard().set_command_sink(sink);
    }

    /// web → app 워크스페이스 전환 싱크를 붙인다 (미러 진입 — I1b-2). app이 start_web에서
    /// 한 번 주입한다(app 레벨이라 워커별 교체 불필요). 미설정이면 폰 Switch가 무시된다.
    pub fn set_switch_sink(&self, sink: dashboard::SwitchSink) {
        self.core.dashboard().set_switch_sink(sink);
    }

    /// 폰에 띄울 일시 안내 배너를 세팅/해제한다 (미러 진입 상한 초과 등 — I1b-2).
    pub fn set_dashboard_notice(&self, notice: Option<String>) {
        self.core.dashboard().set_notice(notice);
    }

    /// 현재 활성 workspace의 세션 상태를 대시보드에 시드한다(구독 등록 직후 호출 — 재구독 시
    /// edge-trigger 상태 유실 보정). app이 GUI 배지용으로 이미 추적 중인 상태를 넘긴다.
    pub fn set_workspaces(&self, seeds: Vec<dashboard::WorkspaceSeed>) {
        self.core.dashboard().set_workspaces(seeds);
    }

    /// 재구독 시점(start_web·워크스페이스 전환)에 활성 세션의 라이브 상태를 시드한다.
    /// 매 프레임 호출하는 [`Self::set_workspaces`]와 분리돼 있다 — 리뷰 P1-1 참조.
    pub fn reseed_active_sessions(&self, sessions: &[dashboard::SessionSeed]) {
        self.core.dashboard().reseed_active_sessions(sessions);
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// 현재 살아있는 접속(정적+WS) 수 (표시/테스트용).
    pub fn active_connections(&self) -> usize {
        self.connections
            .0
            .lock()
            .expect("connections lock")
            .entries
            .len()
    }

    /// 현재 (정적 슬롯, WS 슬롯) 점유 수 (테스트용 — 슬롯 누수/이관 검증).
    pub fn slot_counts(&self) -> (usize, usize) {
        let state = self.connections.0.lock().expect("connections lock");
        (state.static_slots, state.ws_slots)
    }

    /// accept 루프와 모든 접속 스레드를 동기 종료한다.
    pub fn shutdown(self) {
        drop(self); // 정리는 Drop 한 곳에서 — 에러 경로의 drop도 같은 계약을 탄다
    }

    fn shutdown_impl(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // 접속 소켓을 먼저 모두 닫아 블록된 read를 깨운 뒤 join한다 (remote.rs 관례).
        // notify_all은 슬롯 대기 중인 accept 스레드도 깨운다.
        let (lock, cvar) = &*self.connections;
        let conns = {
            let mut state = lock.lock().expect("connections lock");
            std::mem::take(&mut state.entries)
        };
        cvar.notify_all();
        for conn in &conns {
            let _ = conn.stream.shutdown(Shutdown::Both);
        }
        for conn in conns {
            let _ = conn.handle.join();
        }
        // blocking accept를 깨운다
        let _ = TcpStream::connect(self.addr);
        if let Some(handle) = self.accept_thread.take() {
            let _ = handle.join();
        }
        // 접속 스레드가 모두 끝난 뒤(ConnectionGuard drop 완료) 대시보드 스레드를 정지·join한다.
        // 공유받은 코어라면 소유자(앱)가 멈춘다 — 여기서 멈추면 Tailscale을 끄는 것만으로
        // Relay의 대시보드까지 함께 죽는다.
        // 발송 스레드도 코어가 함께 정지·join한다(발송 중이던 요청은 타임아웃까지 이어질 수 있다).
        if self.owns_core {
            self.core.shutdown();
        }
        tracing::info!(addr = %self.addr, "web-remote 서버 정지");
    }
}

impl Drop for WebRemoteServer {
    /// shutdown()을 부르지 않는 에러 경로에서도 소켓/스레드가 정리되도록 (remote.rs 관례).
    fn drop(&mut self) {
        self.shutdown_impl();
    }
}

/// accept 루프를 스레드로 띄운다 — 접속마다 스레드를 붙이고 등록/self-remove를 관리한다
/// (remote.rs spawn_accept 골격 + 동시 접속 상한 대기).
fn spawn_accept(
    listener: TcpListener,
    ctx: Arc<ConnCtx>,
    stop: Arc<AtomicBool>,
    connections: Arc<ConnSet>,
) -> anyhow::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("web-remote-accept".into())
        .spawn(move || {
            for stream in listener.incoming() {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                let stream = match stream {
                    Ok(stream) => stream,
                    Err(e) => {
                        tracing::warn!("web-remote accept 실패: {e}");
                        continue;
                    }
                };
                // fd 레벨 shutdown용 clone — 블록된 접속 스레드를 소켓 종료로 깨운다.
                let shutdown_clone = match stream.try_clone() {
                    Ok(clone) => clone,
                    Err(e) => {
                        tracing::warn!("web-remote stream clone 실패: {e}");
                        continue;
                    }
                };
                let conn_ctx = Arc::clone(&ctx);
                let conn_conns = Arc::clone(&connections);

                let (lock, cvar) = &*connections;
                let mut state = lock.lock().expect("connections lock");
                // 정적 슬롯 상한: 슬롯이 빌 때까지 대기 — 그동안 새 접속은 OS backlog에
                // 쌓인다(연결 거부 아님). 접속 종료·WS 승격(정적 슬롯 반납)·shutdown이 깨운다.
                // WS 슬롯은 여기서 세지 않으므로 장수 WS가 정적 요청을 굶히지 않는다(P2 리뷰).
                while state.static_slots >= MAX_CONNECTIONS && !stop.load(Ordering::SeqCst) {
                    state = cvar.wait(state).expect("connections wait");
                }
                if stop.load(Ordering::SeqCst) {
                    drop(state);
                    let _ = stream.shutdown(Shutdown::Both);
                    break;
                }
                // 주의: state 락은 spawn부터 아래 슬롯 증가+push까지 계속 쥐어야 한다 — 중간에
                // 놓으면 자식의 정리(retain+슬롯 감산)가 push보다 먼저 실행돼 슬롯이 영구
                // 누수된다(P1 불변식). 락을 쥔 덕에 자식의 WS 승격(정적 슬롯 반납)도 아래
                // static_slots 증가 이후에만 진행돼 감산 순서가 어긋나지 않는다.
                let handle = match std::thread::Builder::new()
                    .name("web-remote-conn".into())
                    .spawn(move || {
                        // WS로 승격했으면 true — 정적 슬롯을 반납하고 WS 슬롯 보유 상태로 종료.
                        // 그 경우 ws_slots를, 아니면 static_slots를 반납한다.
                        let held_ws = handle_connection(stream, &conn_ctx, &conn_conns);
                        // 접속 종료 — 자기 항목을 스스로 제거하고 슬롯 반납을 알린다
                        // (자기 join은 데드락이라 remove만 — remote.rs 관례).
                        let id = std::thread::current().id();
                        let (lock, cvar) = &*conn_conns;
                        let mut state = lock.lock().expect("connections lock");
                        state
                            .entries
                            .retain(|entry| entry.handle.thread().id() != id);
                        if held_ws {
                            state.ws_slots -= 1;
                        } else {
                            state.static_slots -= 1;
                        }
                        drop(state);
                        cvar.notify_one();
                    }) {
                    Ok(handle) => handle,
                    Err(e) => {
                        drop(state); // 아직 슬롯을 늘리지 않았으므로 롤백 불필요
                        tracing::warn!("web-remote 접속 스레드 생성 실패: {e}");
                        continue;
                    }
                };
                state.static_slots += 1;
                state.entries.push(ConnEntry {
                    stream: shutdown_clone,
                    handle,
                });
            }
        })
        .context("web-remote accept 스레드 생성 실패")
}

/// head 읽기 전체에 wall-clock 데드라인을 강제하는 read 래퍼.
///
/// `set_read_timeout`은 **read syscall 단위** 타임아웃이라, 타임아웃 직전마다 1바이트씩
/// 흘리는(trickle) 피어는 매 read를 성공시키며 head 읽기를 무한정 끌 수 있다 — 그러면
/// 트리클 접속 [`MAX_CONNECTIONS`]개만으로 서버가 완전히 막힌다. 매 read 전에 총
/// 소요시간을 검사해 데드라인을 넘겼으면 TimedOut으로 끊는다(read_head_line이 Closed로
/// 분류). 마지막 read 자체는 syscall 타임아웃이 끊으므로 총 점유는 데드라인+타임아웃 이내.
struct DeadlineReader<R> {
    inner: R,
    deadline: Instant,
}

impl<R: Read> Read for DeadlineReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if Instant::now() >= self.deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "요청 head 데드라인 초과",
            ));
        }
        self.inner.read(buf)
    }
}

/// 접속 하나 = 요청 하나 (Connection: close). head 파싱 → Host 검증 → 라우팅. WS로 승격해
/// WS 슬롯 보유 상태로 끝나면 `true`를 돌려준다(호출자가 static/ws 중 어느 슬롯을 반납할지 판정).
fn handle_connection(stream: TcpStream, ctx: &ConnCtx, connections: &Arc<ConnSet>) -> bool {
    let peer = stream
        .peer_addr()
        .map(|addr| addr.to_string())
        .unwrap_or_else(|_| "?".to_owned());
    // BSD/macOS에서 accept된 소켓의 blocking 상태를 명시 복원 + read/write syscall 타임아웃
    // (auth callback.rs 관례). read 타임아웃은 syscall 단위라 트리클 피어에는 뚫린다 —
    // head 전체 wall-clock 상한은 DeadlineReader가 강제한다. write 타임아웃은 응답을
    // 읽지 않는 피어에 write_all이 무기한 블록하지 않게 한다 (read/write 방어 대칭).
    if stream.set_nonblocking(false).is_err()
        || stream.set_read_timeout(Some(READ_TIMEOUT)).is_err()
        || stream.set_write_timeout(Some(READ_TIMEOUT)).is_err()
    {
        return false;
    }
    let mut reader = BufReader::new(DeadlineReader {
        inner: stream,
        deadline: Instant::now() + READ_TIMEOUT,
    });
    let head = match http::read_request_head(&mut reader) {
        Ok(head) => head,
        Err(http::HeadError::Closed) => return false, // 침묵/트리클/절단 peer — 응답 없이 종료
        Err(http::HeadError::TooLarge) => {
            tracing::warn!(peer, "web-remote: 요청 head 상한 초과 — 431");
            respond(
                reader.into_inner().inner,
                &http::Response::plain(431, "request head too large"),
                &peer,
                "?",
            );
            return false;
        }
        Err(http::HeadError::Malformed) => {
            respond(
                reader.into_inner().inner,
                &http::Response::plain(400, "bad request"),
                &peer,
                "?",
            );
            return false;
        }
    };
    // POST 본문은 into_inner(버퍼 폐기) 전에 읽는다 — BufReader에 선입된 본문 바이트를 보존.
    // 본문을 받는 경로는 웹푸시 구독 등록(P4 — POST /push/subscribe, 작은 JSON)과 모바일 파일
    // 첨부(P6d — POST /upload, 최대 10MB)다. 경로별로 상한이 달라 여기서 먼저 정한다 —
    // Content-Length 선검사가 상한을 넘는 요청을 본문을 읽기 전에 413으로 끊어 메모리
    // 점유를 유계로 만든다(대용량 업로드도 예외 없이 이 계약을 따른다).
    // `/upload`(P6d)만 10MB 본문을 허용하는데, 이 상한을 **인가 전에** 열어 주면 미인증
    // 요청이 401 전에 10MB를 선할당·읽게 된다(리뷰 P2-1: 이전 4KB 상한의 회귀). 그래서
    // 본문을 읽기 전에 토큰을 먼저 검사한다 — 미인증이면 본문 없이 401. 인가된 요청만
    // 데드라인을 늘려(P2-2) 느린 모바일 uplink의 대용량 업로드가 완주하게 한다.
    if head.path == "/upload" {
        if !upload::authorized(&head.query, &ctx.token) {
            respond(
                reader.into_inner().inner,
                &http::Response::plain(401, "unauthorized"),
                &peer,
                &head.path,
            );
            return false;
        }
        reader.get_mut().deadline = Instant::now() + UPLOAD_READ_TIMEOUT;
    }
    let max_body = if head.path == "/upload" {
        upload::MAX_UPLOAD_BYTES
    } else {
        http::MAX_BODY_BYTES
    };
    let body: Vec<u8> = if head.method == "POST" {
        match head.content_length() {
            Some(len) if len > max_body => {
                respond(
                    reader.into_inner().inner,
                    &http::Response::plain(413, "request body too large"),
                    &peer,
                    &head.path,
                );
                return false;
            }
            Some(len) => match http::read_body(&mut reader, len) {
                Ok(body) => body,
                Err(_) => {
                    respond(
                        reader.into_inner().inner,
                        &http::Response::plain(400, "bad request body"),
                        &peer,
                        &head.path,
                    );
                    return false;
                }
            },
            None => Vec::new(),
        }
    } else {
        Vec::new()
    };

    // BufReader → raw 소켓 핸드오프: into_inner는 BufReader 내부 버퍼에 남은 바이트를 버린다.
    // WS 승격 시 파이프라이닝 클라이언트가 101 전에 선행 프레임을 보냈다면 그 프레임은 유실된다
    // — 표준 브라우저는 101 수신 전 WS 프레임을 보내지 않으므로 실무상 비발현.
    let stream = reader.into_inner().inner;

    // Host 검증 — DNS rebinding 차단 (remote-tls-delta §1.5 Origin 지침의 HTTP 적용).
    if !host_allowed(head.header("host"), ctx.allowed_host.as_deref()) {
        tracing::warn!(peer, host = ?head.header("host"), "web-remote: Host 불일치 — 403");
        respond(
            stream,
            &http::Response::plain(403, "forbidden host"),
            &peer,
            &head.path,
        );
        return false;
    }
    // WS 업그레이드(`/ws`만)는 ws_api 계층이 승격해 대시보드 세션을 처리한다(접속을 장수 점유).
    // query에 토큰이 실릴 수 있어 로그하지 않는다 — path만.
    if ws_api::is_upgrade_request(&head) {
        // Origin 심층 방어 (P6a — CSWSH 대비, 방법 B 선제). 헤더가 있으면 허용 오리진과
        // 대조하고, 부재(비브라우저 클라)는 첫 프레임 토큰 인증에 위임한다.
        if !origin_allowed(head.header("origin"), ctx.allowed_host.as_deref()) {
            tracing::warn!(peer, origin = ?head.header("origin"), "web-remote: Origin 불일치 — 403");
            respond(
                stream,
                &http::Response::plain(403, "forbidden origin"),
                &peer,
                &head.path,
            );
            return false;
        }
        // 승격 확정 시점에 정적 슬롯을 반납하고 WS 슬롯으로 이관한다 — 장수 WS가 정적 자산
        // 요청의 슬롯을 굶히지 않게(P2 리뷰). 상한 초과면 이관 없이 503으로 즉시 거부한다
        // (핸드셰이크 전이라 HTTP 503이 즉시 거부 — 브라우저 WS는 실패 후 백오프 재접속).
        if !try_acquire_ws_slot(connections) {
            tracing::warn!(peer, "web-remote: WS 슬롯 상한 초과 — 503");
            respond(
                stream,
                &http::Response::plain(503, "websocket capacity reached"),
                &peer,
                &head.path,
            );
            return false; // 정적 슬롯 유지 — 호출자가 static_slots 반납
        }
        tracing::info!(peer, path = %head.path, "web-remote WS 업그레이드");
        ws_api::serve(stream, &head, &ctx.token, &ctx.dashboard, &ctx.stop);
        return true; // WS 슬롯 보유 상태로 종료 — 호출자가 ws_slots 반납
    }
    // 웹푸시(P4) 엔드포인트 — GET /push/vapid(공개키), POST /push/subscribe(등록). 둘 다
    // 토큰 게이트. `/push/*`가 아니면 None을 돌려 정적 라우팅으로 흘려보낸다.
    if let Some(response) = push::route(&head, &body, &ctx.token, ctx.push.as_ref()) {
        respond(stream, &response, &peer, &head.path);
        return false;
    }
    // 모바일 파일 첨부(P6d) — POST /upload. 토큰 게이트 + Content-Type 화이트리스트는
    // upload::route 안에서 처리한다. uploads_dir 미배선(테스트/미설정)이면 404.
    if let Some(response) = upload::route(&head, &body, &ctx.token, ctx.uploads_dir.as_deref()) {
        respond(stream, &response, &peer, &head.path);
        return false;
    }
    if head.method != "GET" {
        respond(
            stream,
            &http::Response::plain(405, "method not allowed"),
            &peer,
            &head.path,
        );
        return false;
    }
    let response = static_srv::respond(&head.path, &head.query, &ctx.token);
    respond(stream, &response, &peer, &head.path);
    false
}

/// 정적 슬롯을 반납하고 WS 슬롯을 확보한다. WS 슬롯이 남아 있으면 `static_slots`를 1 줄이고
/// `ws_slots`를 1 늘린 뒤 `true`(정적 슬롯 반납은 대기 중인 accept를 깨운다). 상한 초과면 아무
/// 변화 없이 `false`(정적 슬롯 유지 — 호출자가 반납). 호출 시점엔 이 접속의 정적 슬롯이 이미
/// 계수돼 있어(accept가 spawn 전 증가) `static_slots >= 1`이 보장된다 — 언더플로 없음.
fn try_acquire_ws_slot(connections: &Arc<ConnSet>) -> bool {
    let (lock, cvar) = &**connections;
    let mut state = lock.lock().expect("connections lock");
    if state.ws_slots >= MAX_WS_CONNECTIONS {
        return false;
    }
    state.static_slots -= 1;
    state.ws_slots += 1;
    drop(state);
    cvar.notify_one(); // 정적 슬롯 하나 반납 — 대기 중인 accept 깨움
    true
}

/// 응답을 쓰고 접속 감사 로그를 남긴다. query는 토큰이 실리므로 **절대 로그하지 않는다**.
fn respond(mut stream: TcpStream, response: &http::Response, peer: &str, path: &str) {
    let _ = http::write_response(&mut stream, response);
    tracing::info!(peer, path, status = response.status, "web-remote 접속");
}

/// Host 헤더 허용 여부. 허용: loopback 계열(127.0.0.1 / localhost / [::1]) +
/// 설정된 ts.net 호스트명(대소문자 무시, `allowed`는 이미 소문자). 포트 suffix는 무시.
/// HTTP/1.1에서 Host는 필수 — 없으면 거부. tailscale serve가 원 Host를 보존하든
/// 127.0.0.1로 재작성하든 두 경우 모두 통과하도록 loopback 계열을 항상 허용한다.
fn host_allowed(host: Option<&str>, allowed: Option<&str>) -> bool {
    let Some(host) = host else {
        return false;
    };
    let name = strip_port(host).to_ascii_lowercase();
    if matches!(name.as_str(), "127.0.0.1" | "localhost" | "[::1]") {
        return true;
    }
    allowed.is_some_and(|allowed| name == allowed)
}

/// WS 업그레이드 Origin 심층 방어 (P6a). Origin 헤더가 **있으면** 허용 오리진
/// (ts 호스트/loopback)과 대조해 불일치·기형("null" 포함)을 거부한다. 부재는
/// 첫 프레임 토큰 인증에 위임 — 비브라우저 클라이언트·기존 테스트와 호환.
fn origin_allowed(origin: Option<&str>, allowed: Option<&str>) -> bool {
    let Some(origin) = origin else {
        return true; // 헤더 없음 — 토큰 인증에 위임
    };
    let Some((_scheme, authority)) = origin.split_once("://") else {
        return false; // "null" 등 기형 오리진
    };
    host_allowed(Some(authority), allowed)
}

/// "host:port" / "[v6]:port" / "host"에서 host 부분만 남긴다.
fn strip_port(host: &str) -> &str {
    if host.starts_with('[') {
        match host.find(']') {
            Some(end) => &host[..=end],
            None => host,
        }
    } else {
        host.split(':').next().unwrap_or(host)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::path::PathBuf;
    use std::time::Instant;
    use tungstenite::Message;
    use tungstenite::protocol::{Role, WebSocket};

    const TEST_TOKEN: &str = "0123456789abcdef0123456789abcdef";

    fn start(allowed_host: Option<&str>) -> WebRemoteServer {
        start_with_db(allowed_host, None)
    }

    fn start_with_db(allowed_host: Option<&str>, db_path: Option<PathBuf>) -> WebRemoteServer {
        let repository = db_path.as_deref().map(|path| {
            repository::StorageTestRepository::open(path)
                as Arc<dyn repository::WebRemoteRepository>
        });
        WebRemoteServer::serve(
            SocketAddr::from(([127, 0, 0, 1], 0)),
            ServeOptions {
                token: TEST_TOKEN.to_owned(),
                allowed_host: allowed_host.map(str::to_owned),
                repository,
                vapid: None,
                uploads_dir: None,
            },
        )
        .unwrap()
    }

    /// 업로드(P6d) 활성 서버 — uploads_dir을 지정해 띄운다.
    fn start_with_uploads(dir: PathBuf) -> WebRemoteServer {
        WebRemoteServer::serve(
            SocketAddr::from(([127, 0, 0, 1], 0)),
            ServeOptions {
                token: TEST_TOKEN.to_owned(),
                allowed_host: None,
                repository: None,
                vapid: None,
                uploads_dir: Some(dir),
            },
        )
        .unwrap()
    }

    /// raw 요청을 보내고 응답 전문을 돌려받는다.
    fn request(addr: SocketAddr, raw: &str) -> String {
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream.write_all(raw.as_bytes()).unwrap();
        let mut out = String::new();
        let _ = stream.read_to_string(&mut out);
        out
    }

    fn get(addr: SocketAddr, target: &str) -> String {
        request(
            addr,
            &format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"),
        )
    }

    /// PNG 등 바이너리 응답용 — read_to_string은 비UTF-8에서 실패하므로 raw 바이트로 받는다.
    fn get_raw(addr: SocketAddr, target: &str) -> Vec<u8> {
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream
            .write_all(format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes())
            .unwrap();
        let mut out = Vec::new();
        let _ = stream.read_to_end(&mut out);
        out
    }

    #[test]
    fn 토큰_일치는_앱셸_불일치는_401() {
        let server = start(None);
        let addr = server.local_addr();
        let ok = get(addr, &format!("/?token={TEST_TOKEN}"));
        assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
        assert!(ok.contains(r#"data-view="shell""#), "{ok}");

        let bad = get(addr, "/?token=wrong");
        assert!(bad.starts_with("HTTP/1.1 401"), "{bad}");
        assert!(bad.contains(r#"data-view="pairing""#), "{bad}");

        let missing = get(addr, "/");
        assert!(missing.starts_with("HTTP/1.1 401"), "{missing}");
    }

    #[test]
    fn 정적_자산은_200_화이트리스트_밖은_404() {
        let server = start(None);
        let addr = server.local_addr();
        let js = get(addr, "/app.js");
        assert!(js.starts_with("HTTP/1.1 200"), "{js}");
        assert!(js.contains("text/javascript"), "{js}");

        let manifest = get(addr, "/manifest.webmanifest");
        assert!(manifest.starts_with("HTTP/1.1 200"), "{manifest}");

        let missing = get(addr, "/unknown");
        assert!(missing.starts_with("HTTP/1.1 404"), "{missing}");
        let traversal = get(addr, "/../Cargo.toml");
        assert!(traversal.starts_with("HTTP/1.1 404"), "{traversal}");
    }

    #[test]
    fn 설치_셸_자산과_sw_헤더() {
        let server = start(None);
        let addr = server.local_addr();
        // sw.js: 버전 자리표시자 치환 + no-cache 헤더(재방문 시 새 SW 반영 보장)
        let sw = get(addr, "/sw.js");
        assert!(sw.starts_with("HTTP/1.1 200"), "{sw}");
        assert!(sw.contains("Cache-Control: no-cache"), "{sw}");
        assert!(sw.contains("deppy-shell-"), "{sw}");
        assert!(!sw.contains("__SHELL_VERSION__"), "{sw}");
        // 설치 아이콘·오프라인 셸은 바이너리를 포함할 수 있어 raw로 상태줄만 확인한다.
        for path in [
            "/icon-192.png",
            "/icon-512.png",
            "/icon-maskable-512.png",
            "/apple-touch-icon.png",
            "/offline.html",
        ] {
            let raw = get_raw(addr, path);
            assert!(raw.starts_with(b"HTTP/1.1 200"), "{path}");
        }
    }

    #[test]
    fn host_불일치는_403() {
        let server = start(Some("mac.tail.ts.net"));
        let addr = server.local_addr();
        // 허용: 설정 호스트명 (대소문자/포트 무시)
        let ok = request(
            addr,
            "GET /healthz HTTP/1.1\r\nHost: MAC.tail.TS.net:443\r\n\r\n",
        );
        assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
        // 허용: loopback 계열 (serve 프록시가 Host를 재작성해도 동작)
        let local = request(
            addr,
            "GET /healthz HTTP/1.1\r\nHost: localhost:8737\r\n\r\n",
        );
        assert!(local.starts_with("HTTP/1.1 200"), "{local}");
        // 거부: 그 외 호스트 (DNS rebinding)
        let evil = request(addr, "GET /healthz HTTP/1.1\r\nHost: evil.example\r\n\r\n");
        assert!(evil.starts_with("HTTP/1.1 403"), "{evil}");
        // 거부: Host 없음
        let none = request(addr, "GET /healthz HTTP/1.1\r\n\r\n");
        assert!(none.starts_with("HTTP/1.1 403"), "{none}");
    }

    #[test]
    fn 비루프백_평문_bind는_거부() {
        let result = WebRemoteServer::serve(
            SocketAddr::from(([0, 0, 0, 0], 0)),
            ServeOptions {
                token: TEST_TOKEN.to_owned(),
                allowed_host: None,
                repository: None,
                vapid: None,
                uploads_dir: None,
            },
        );
        // WebRemoteServer는 Debug 미구현(스레드 핸들) — unwrap_err 대신 match로 확인
        let Err(err) = result else {
            panic!("비-loopback bind가 허용됨");
        };
        assert!(format!("{err:#}").contains("비-loopback"), "{err:#}");
    }

    #[test]
    fn 종료_후_리스너가_사라지고_포트가_풀린다() {
        let server = start(None);
        let addr = server.local_addr();
        assert!(get(addr, "/healthz").starts_with("HTTP/1.1 200"));
        server.shutdown();
        // 리스너/스레드가 정리됐으므로 같은 포트에 즉시 재bind 가능
        let rebind = TcpListener::bind(addr);
        assert!(rebind.is_ok(), "{rebind:?}");
    }

    #[test]
    fn 동시_접속_상한_초과는_슬롯이_빌_때까지_대기() {
        let server = start(None);
        let addr = server.local_addr();
        // 슬롯 3개를 침묵 접속으로 점유하고, 서버가 접속 스레드를 다 붙일 때까지 기다린다
        let holders: Vec<TcpStream> = (0..MAX_CONNECTIONS)
            .map(|_| TcpStream::connect(addr).unwrap())
            .collect();
        let deadline = Instant::now() + Duration::from_secs(5);
        while server.active_connections() < MAX_CONNECTIONS {
            assert!(
                Instant::now() < deadline,
                "접속 스레드 {MAX_CONNECTIONS}개가 생기지 않음"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        // 4번째 요청은 슬롯이 없어 아직 처리되지 않는다 (backlog 대기)
        let mut fourth = TcpStream::connect(addr).unwrap();
        fourth
            .write_all(b"GET /healthz HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .unwrap();
        fourth
            .set_read_timeout(Some(Duration::from_millis(300)))
            .unwrap();
        let mut buf = [0u8; 64];
        assert!(
            fourth.read(&mut buf).is_err(),
            "상한 초과 접속이 즉시 처리됨"
        );
        // 슬롯 하나를 반납하면 대기 중이던 요청이 처리된다
        let mut rest = holders.into_iter();
        drop(rest.next());
        fourth
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut out = String::new();
        let _ = fourth.read_to_string(&mut out);
        assert!(out.starts_with("HTTP/1.1 200"), "{out}");
        drop(rest); // 나머지 홀더 정리
    }

    #[test]
    fn ws_핸드셰이크는_101_키없는_업그레이드는_400() {
        let server = start(None);
        let addr = server.local_addr();
        // 유효 핸드셰이크(key + version 13) → 101 + 표준 accept 키
        let ok = request_head_only(
            addr,
            "GET /ws HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
        );
        assert!(ok.starts_with("HTTP/1.1 101"), "{ok}");
        assert!(
            ok.contains("Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo="),
            "{ok}"
        );
        // 키 없는 업그레이드 → 400
        let bad = request(
            addr,
            "GET /ws HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n",
        );
        assert!(bad.starts_with("HTTP/1.1 400"), "{bad}");
    }

    /// 응답 head(\r\n\r\n)까지만 읽는다 — 101 뒤 서버가 접속을 열어 둔 채 인증을 기다리므로
    /// EOF까지 읽으면 auth 타임아웃(5s)만큼 블록된다.
    fn request_head_only(addr: SocketAddr, raw: &str) -> String {
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream.write_all(raw.as_bytes()).unwrap();
        let mut out = Vec::new();
        let mut one = [0u8; 1];
        while stream.read(&mut one).map(|n| n > 0).unwrap_or(false) {
            out.push(one[0]);
            if out.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// tungstenite 클라이언트로 핸드셰이크를 마치고 프레이밍 소켓을 돌려준다(테스트용).
    fn ws_client(addr: SocketAddr) -> WebSocket<TcpStream> {
        let mut stream = TcpStream::connect(addr).unwrap();
        // 짧은 read 타임아웃 — read_text가 데드라인까지 재시도할 수 있게.
        stream
            .set_read_timeout(Some(Duration::from_millis(300)))
            .unwrap();
        stream
            .write_all(
                b"GET /ws HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
            )
            .unwrap();
        // 101 head 소진(그 뒤엔 WS 프레임 — 서버는 인증 프레임 전까지 아무것도 안 보낸다)
        let mut one = [0u8; 1];
        let mut head = Vec::new();
        while stream.read(&mut one).map(|n| n > 0).unwrap_or(false) {
            head.push(one[0]);
            if head.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        assert!(
            String::from_utf8_lossy(&head).starts_with("HTTP/1.1 101"),
            "핸드셰이크 실패: {}",
            String::from_utf8_lossy(&head)
        );
        WebSocket::from_raw_socket(stream, Role::Client, None)
    }

    fn send_text(ws: &mut WebSocket<TcpStream>, text: &str) {
        ws.send(Message::Text(text.to_owned().into())).unwrap();
    }

    /// 데드라인 내 텍스트 프레임 하나를 돌려준다(Ping/Pong 스킵). 없으면 None.
    fn read_text(ws: &mut WebSocket<TcpStream>, timeout: Duration) -> Option<String> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match ws.read() {
                Ok(Message::Text(t)) => return Some(t.as_str().to_owned()),
                Ok(Message::Close(_)) => return None,
                Ok(_) => {}
                Err(tungstenite::Error::Io(io))
                    if io.kind() == std::io::ErrorKind::WouldBlock
                        || io.kind() == std::io::ErrorKind::TimedOut => {}
                Err(_) => return None,
            }
        }
        None
    }

    /// 데드라인 내에 특정 `type`의 프레임을 찾아 돌려준다.
    fn read_frame_of_type(
        ws: &mut WebSocket<TcpStream>,
        ty: &str,
        timeout: Duration,
    ) -> Option<String> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match read_text(ws, remaining) {
                Some(text) if text.contains(&format!(r#""type":"{ty}""#)) => return Some(text),
                Some(_) => {}
                None => return None,
            }
        }
        None
    }

    fn temp_db_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "web-remote-test-{}.db",
            uuid::Uuid::new_v4().simple()
        ))
    }

    #[test]
    fn ws_인증_성공은_welcome_실패는_close() {
        let server = start(None);
        let addr = server.local_addr();

        // 정상: 유효 토큰 → welcome + 대시보드/승인 프레임
        let mut ws = ws_client(addr);
        send_text(
            &mut ws,
            &format!(r#"{{"type":"auth","v":1,"token":"{TEST_TOKEN}"}}"#),
        );
        let welcome = read_frame_of_type(&mut ws, "welcome", Duration::from_secs(3));
        assert!(welcome.is_some(), "welcome 프레임 없음");
        assert!(
            welcome
                .unwrap()
                .contains(&format!(r#""v":{}"#, protocol::PROTOCOL_VERSION))
        );
        // 접속 없던 서버가 등록 즉시 대시보드 프레임을 발행한다(빈 세션이라도)
        assert!(
            read_frame_of_type(&mut ws, "dashboard", Duration::from_secs(3)).is_some(),
            "대시보드 프레임 없음"
        );

        // 거부: 잘못된 토큰 → error + close
        let mut bad = ws_client(addr);
        send_text(&mut bad, r#"{"type":"auth","v":1,"token":"wrong"}"#);
        // error 프레임 또는 close 중 하나로 종료됨
        let got = read_text(&mut bad, Duration::from_secs(3));
        assert!(
            got.as_deref()
                .map(|t| t.contains("unauthorized"))
                .unwrap_or(true),
            "인증 실패인데 close/error가 아님: {got:?}"
        );
    }

    /// P5b: WS watch/전환/절단이 브리지 refcount를 거쳐 runtime lease 명령으로
    /// 정확히 재바인딩되는지 — 전송 계층까지 포함한 검증.
    #[test]
    fn ws_watch_전환과_절단이_lease를_재바인딩한다() {
        let server = start(None);
        let captured: Arc<Mutex<Vec<runtime::RuntimeCommand>>> = Arc::default();
        let sink_cap = Arc::clone(&captured);
        server.set_runtime_command_sink(Arc::new(move |command| {
            sink_cap.lock().unwrap().push(command);
        }));
        // UUID↔u64 매핑 — 실경로에서는 MuxUpdated가 채운다 (I1). 이게 없으면 서버가
        // UUID를 u64로 변환하지 못해 명령을 만들지 않는다(앨리어싱 차단의 핵심).
        seed_ids(&server, &[7, 9]);
        let leases = |captured: &Arc<Mutex<Vec<runtime::RuntimeCommand>>>| -> Vec<(u64, bool)> {
            captured
                .lock()
                .unwrap()
                .iter()
                .filter_map(|command| match command {
                    runtime::RuntimeCommand::SetRemoteViewing {
                        session, viewing, ..
                    } => Some((session.0, *viewing)),
                    _ => None,
                })
                .collect()
        };
        let wait_leases = |expect: &[(u64, bool)]| {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let got = leases(&captured);
                if got == expect {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "lease 시퀀스 불일치: {got:?} != {expect:?}"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        };

        let mut ws = ws_client(server.local_addr());
        send_text(
            &mut ws,
            &format!(r#"{{"type":"auth","v":1,"token":"{TEST_TOKEN}"}}"#),
        );
        assert!(
            read_frame_of_type(&mut ws, "welcome", Duration::from_secs(3)).is_some(),
            "welcome 없음"
        );
        // 시청 시작 → 7 on
        send_text(&mut ws, r#"{"type":"watch","session":"uuid-7"}"#);
        wait_leases(&[(7, true)]);
        // 전환 → 7 off + 9 on (재바인딩)
        send_text(&mut ws, r#"{"type":"watch","session":"uuid-9"}"#);
        wait_leases(&[(7, true), (7, false), (9, true)]);
        // 절단 → 9 off (Drop 경로 정리)
        drop(ws);
        wait_leases(&[(7, true), (7, false), (9, true), (9, false)]);
        server.shutdown();
    }

    /// P5c: watch → keyframe, 변경 → 해당 행만 delta, request_keyframe → 재동기화.
    #[test]
    fn ws_viewport는_keyframe_delta_재동기화를_거친다() {
        let server = start(None);
        let captured: Arc<Mutex<Vec<runtime::RuntimeCommand>>> = Arc::default();
        let sink_cap = Arc::clone(&captured);
        server.set_runtime_command_sink(Arc::new(move |command| {
            sink_cap.lock().unwrap().push(command);
        }));
        // UUID↔u64 매핑 — 실경로에서는 MuxUpdated가 채운다 (I1). 이게 없으면 서버가
        // UUID를 u64로 변환하지 못해 명령을 만들지 않는다(앨리어싱 차단의 핵심).
        seed_ids(&server, &[7, 9]);

        let make_snapshot = |first_char: char| {
            let mut cells = vec![
                runtime::TerminalCell::new(' ', [255, 255, 255], [0, 0, 0], false, false, Default::default());
                20 // 10×2
            ];
            cells[0].c = first_char;
            runtime::RuntimeEvent::Viewport {
                session: runtime::SessionId(7),
                snapshot: Arc::new(runtime::TerminalViewportSnapshot {
                    cols: 10,
                    rows: 2,
                    cursor: runtime::CursorSnapshot {
                        col: 0,
                        row: 0,
                        shape: runtime::CursorShape::Block,
                        visible: true,
                    },
                    visible_cells: cells.into(),
                    graphemes: Default::default(),
                    dirty_ranges: Vec::new(),
                    title: None,
                    scroll_offset: 0,
                    is_alt_screen: false,
                }),
                bracketed_paste: false,
            }
        };

        let mut ws = ws_client(server.local_addr());
        send_text(
            &mut ws,
            &format!(r#"{{"type":"auth","v":1,"token":"{TEST_TOKEN}"}}"#),
        );
        assert!(read_frame_of_type(&mut ws, "welcome", Duration::from_secs(3)).is_some());

        // watch → 브리지에 lease가 등록될 때까지 대기 (이후 inject가 슬롯에 반영된다)
        send_text(&mut ws, r#"{"type":"watch","session":"uuid-7"}"#);
        let deadline = Instant::now() + Duration::from_secs(5);
        while captured.lock().unwrap().is_empty() {
            assert!(Instant::now() < deadline, "watch lease가 등록되지 않음");
            std::thread::sleep(Duration::from_millis(20));
        }

        // 첫 프레임 = keyframe (전체 2행)
        server.core.dashboard().inject_event(make_snapshot('a'));
        let first = read_frame_of_type(&mut ws, "viewport", Duration::from_secs(3))
            .expect("viewport 프레임 없음");
        assert!(first.contains(r#""keyframe":true"#), "{first}");
        assert_eq!(first.matches(r#""runs":"#).count(), 2, "{first}");

        // 1행만 변경 → delta에 그 행만
        server.core.dashboard().inject_event(make_snapshot('b'));
        let delta = read_frame_of_type(&mut ws, "viewport", Duration::from_secs(3))
            .expect("delta 프레임 없음");
        assert!(delta.contains(r#""keyframe":false"#), "{delta}");
        assert_eq!(delta.matches(r#""runs":"#).count(), 1, "{delta}");
        assert!(delta.contains(r#""t":"b"#), "{delta}");

        // 재동기화 요청 — 새 스냅샷 없이도 같은 슬롯에서 keyframe 재전송
        send_text(&mut ws, r#"{"type":"request_keyframe"}"#);
        let resync = read_frame_of_type(&mut ws, "viewport", Duration::from_secs(3))
            .expect("재동기화 keyframe 없음");
        assert!(resync.contains(r#""keyframe":true"#), "{resync}");
        assert_eq!(resync.matches(r#""runs":"#).count(), 2, "{resync}");

        drop(ws);
        server.shutdown();
    }

    /// P5d: 제어 키는 시청 중 세션에만 WriteInput으로 전달되고, 비시청 세션·
    /// 미지 키는 무시된다.
    #[test]
    fn ws_key는_시청_중_세션에만_writeinput을_보낸다() {
        let server = start(None);
        let captured: Arc<Mutex<Vec<runtime::RuntimeCommand>>> = Arc::default();
        let sink_cap = Arc::clone(&captured);
        server.set_runtime_command_sink(Arc::new(move |command| {
            sink_cap.lock().unwrap().push(command);
        }));
        // UUID↔u64 매핑 — 실경로에서는 MuxUpdated가 채운다 (I1). 이게 없으면 서버가
        // UUID를 u64로 변환하지 못해 명령을 만들지 않는다(앨리어싱 차단의 핵심).
        seed_ids(&server, &[7, 9]);
        let mut ws = ws_client(server.local_addr());
        send_text(
            &mut ws,
            &format!(r#"{{"type":"auth","v":1,"token":"{TEST_TOKEN}"}}"#),
        );
        assert!(read_frame_of_type(&mut ws, "welcome", Duration::from_secs(3)).is_some());
        send_text(&mut ws, r#"{"type":"watch","session":"uuid-7"}"#);
        // 비시청 세션 키 + 미지 키 + 유효 키 순서로 보낸다
        send_text(
            &mut ws,
            r#"{"type":"key","session":"uuid-9","key":"ctrl_c"}"#,
        );
        send_text(
            &mut ws,
            r#"{"type":"key","session":"uuid-7","key":"rm_rf"}"#,
        );
        send_text(
            &mut ws,
            r#"{"type":"key","session":"uuid-7","key":"ctrl_c"}"#,
        );
        send_text(
            &mut ws,
            r#"{"type":"key","session":"uuid-7","key":"enter"}"#,
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let inputs: Vec<(u64, Vec<u8>)> = captured
                .lock()
                .unwrap()
                .iter()
                .filter_map(|command| match command {
                    runtime::RuntimeCommand::WriteInput { session, bytes } => {
                        Some((session.0, bytes.clone()))
                    }
                    _ => None,
                })
                .collect();
            if inputs.len() >= 2 {
                assert_eq!(
                    inputs,
                    vec![(7, b"\x03".to_vec()), (7, b"\r".to_vec())],
                    "비시청/미지 키가 통과했거나 매핑이 틀림"
                );
                break;
            }
            assert!(Instant::now() < deadline, "WriteInput이 도착하지 않음");
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(ws);
        server.shutdown();
    }

    #[test]
    fn origin_검사는_허용_오리진과_부재만_통과시킨다() {
        let allowed = Some("jr.ts.net");
        // 부재 — 비브라우저 클라, 토큰 인증에 위임
        assert!(origin_allowed(None, allowed));
        // 허용 호스트/loopback (포트·스킴 무관)
        assert!(origin_allowed(Some("https://jr.ts.net"), allowed));
        assert!(origin_allowed(Some("http://localhost:5173"), allowed));
        assert!(origin_allowed(Some("http://127.0.0.1:8737"), allowed));
        // 불일치·기형은 거부
        assert!(!origin_allowed(Some("https://evil.example"), allowed));
        assert!(!origin_allowed(Some("null"), allowed));
        assert!(!origin_allowed(Some("https://jr.ts.net.evil.com"), allowed));
    }

    /// P6a: Origin 불일치 업그레이드는 403으로 거부된다 (심층 방어).
    #[test]
    fn ws_origin_불일치는_403() {
        let server = start(None);
        let mut stream = TcpStream::connect(server.local_addr()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream
            .write_all(
                b"GET /ws HTTP/1.1\r\nHost: 127.0.0.1\r\nOrigin: https://evil.example\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
            )
            .unwrap();
        // 상태줄이 여러 read로 쪼개질 수 있다 — 연결 종료까지 모아 읽는다.
        let mut raw = Vec::new();
        let mut buf = [0u8; 256];
        while let Ok(n) = stream.read(&mut buf) {
            if n == 0 {
                break;
            }
            raw.extend_from_slice(&buf[..n]);
            if raw.len() > 4096 {
                break;
            }
        }
        let head = String::from_utf8_lossy(&raw);
        assert!(head.starts_with("HTTP/1.1 403"), "{head}");
        server.shutdown();
    }

    /// P6a: 자유 입력은 시청 중 세션에만 WriteInput으로 전달된다 (제어문자 strip 포함).
    #[test]
    fn ws_input은_시청_중_세션에만_전달된다() {
        let server = start(None);
        let captured: Arc<Mutex<Vec<runtime::RuntimeCommand>>> = Arc::default();
        let sink_cap = Arc::clone(&captured);
        server.set_runtime_command_sink(Arc::new(move |command| {
            sink_cap.lock().unwrap().push(command);
        }));
        // UUID↔u64 매핑 — 실경로에서는 MuxUpdated가 채운다 (I1). 이게 없으면 서버가
        // UUID를 u64로 변환하지 못해 명령을 만들지 않는다(앨리어싱 차단의 핵심).
        seed_ids(&server, &[7, 9]);
        let mut ws = ws_client(server.local_addr());
        send_text(
            &mut ws,
            &format!(r#"{{"type":"auth","v":1,"token":"{TEST_TOKEN}"}}"#),
        );
        assert!(read_frame_of_type(&mut ws, "welcome", Duration::from_secs(3)).is_some());
        send_text(&mut ws, r#"{"type":"watch","session":"uuid-7"}"#);
        // 비시청 세션(무시) → 시청 세션 전송(제어문자 포함 — strip 검증)
        send_text(
            &mut ws,
            r#"{"type":"input","session":"uuid-9","text":"evil","submit":true}"#,
        );
        send_text(
            &mut ws,
            r#"{"type":"input","session":"uuid-7","text":"echo\u001b[31m hi","submit":true}"#,
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let inputs: Vec<(u64, Vec<u8>)> = captured
                .lock()
                .unwrap()
                .iter()
                .filter_map(|command| match command {
                    runtime::RuntimeCommand::WriteInput { session, bytes } => {
                        Some((session.0, bytes.clone()))
                    }
                    _ => None,
                })
                .collect();
            if !inputs.is_empty() {
                assert_eq!(
                    inputs,
                    vec![(7, b"echo[31m hi\r".to_vec())],
                    "비시청 input이 통과했거나 제어문자 strip이 틀림"
                );
                break;
            }
            assert!(
                Instant::now() < deadline,
                "WriteInput이 도착하지 않음 — captured: {:?}",
                captured.lock().unwrap()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(ws);
        server.shutdown();
    }

    /// 스크롤백 열람: scroll은 시청 중 세션에만 Scroll 커맨드로 전달되고,
    /// 비정상 delta는 캡된다.
    #[test]
    fn ws_scroll은_시청_중_세션에만_전달되고_캡된다() {
        let server = start(None);
        let captured: Arc<Mutex<Vec<runtime::RuntimeCommand>>> = Arc::default();
        let sink_cap = Arc::clone(&captured);
        server.set_runtime_command_sink(Arc::new(move |command| {
            sink_cap.lock().unwrap().push(command);
        }));
        // UUID↔u64 매핑 — 실경로에서는 MuxUpdated가 채운다 (I1). 이게 없으면 서버가
        // UUID를 u64로 변환하지 못해 명령을 만들지 않는다(앨리어싱 차단의 핵심).
        seed_ids(&server, &[7, 9]);
        let mut ws = ws_client(server.local_addr());
        send_text(
            &mut ws,
            &format!(r#"{{"type":"auth","v":1,"token":"{TEST_TOKEN}"}}"#),
        );
        assert!(read_frame_of_type(&mut ws, "welcome", Duration::from_secs(3)).is_some());
        send_text(&mut ws, r#"{"type":"watch","session":"uuid-7"}"#);
        // 비시청 세션(무시) → 정상 delta → 오버사이즈 delta(캡) 순서
        send_text(&mut ws, r#"{"type":"scroll","session":"uuid-9","delta":5}"#);
        send_text(&mut ws, r#"{"type":"scroll","session":"uuid-7","delta":3}"#);
        send_text(
            &mut ws,
            r#"{"type":"scroll","session":"uuid-7","delta":2000000000}"#,
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let scrolls: Vec<(u64, i32)> = captured
                .lock()
                .unwrap()
                .iter()
                .filter_map(|command| match command {
                    runtime::RuntimeCommand::Scroll { session, delta } => Some((session.0, *delta)),
                    _ => None,
                })
                .collect();
            if scrolls.len() >= 2 {
                assert_eq!(
                    scrolls,
                    vec![(7, 3), (7, 100_000)],
                    "비시청 스크롤이 통과했거나 캡이 틀림"
                );
                break;
            }
            assert!(Instant::now() < deadline, "Scroll 커맨드가 도착하지 않음");
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(ws);
        server.shutdown();
    }

    #[test]
    fn ws_인증_전_비auth_첫프레임은_거부() {
        let server = start(None);
        let mut ws = ws_client(server.local_addr());
        // 첫 프레임이 resolve(비-auth) → 인증 실패로 close
        send_text(&mut ws, r#"{"type":"resolve","id":"x","allowed":true}"#);
        let got = read_text(&mut ws, Duration::from_secs(3));
        assert!(
            got.as_deref()
                .map(|t| t.contains("unauthorized"))
                .unwrap_or(true),
            "비-auth 첫 프레임이 거부되지 않음: {got:?}"
        );
    }

    #[test]
    fn ws_승인_목록_수신과_resolve_왕복() {
        let db_path = temp_db_path();
        // 사전에 pending 승인 하나 삽입(별도 연결 — 브리지는 자기 연결로 폴링)
        {
            let db = storage::Db::open(&db_path).unwrap();
            db.insert_pending_approval(
                "appr-1",
                "github",
                "create_issue",
                "{\"title\":\"x\"}",
                None,
                1720,
                None,
            )
            .unwrap();
        }
        let server = start_with_db(None, Some(db_path.clone()));
        let mut ws = ws_client(server.local_addr());
        send_text(
            &mut ws,
            &format!(r#"{{"type":"auth","v":1,"token":"{TEST_TOKEN}"}}"#),
        );
        // 승인 프레임에 삽입한 항목이 보인다
        let approvals = read_frame_of_type(&mut ws, "approvals", Duration::from_secs(3))
            .expect("승인 프레임 없음");
        assert!(approvals.contains("appr-1"), "{approvals}");
        assert!(approvals.contains("create_issue"), "{approvals}");

        // Deny 결정을 보낸다 → DB에 resolved로 반영(더 이상 pending 아님)
        send_text(
            &mut ws,
            r#"{"type":"resolve","id":"appr-1","allowed":false,"remember":false}"#,
        );
        // 브리지가 재폴링해 빈 목록을 발행할 때까지 대기
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut resolved = false;
        while Instant::now() < deadline {
            let db = storage::Db::open(&db_path).unwrap();
            if db.list_pending_approvals().unwrap().is_empty() {
                resolved = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(resolved, "resolve가 DB에 반영되지 않음");
        let _ = std::fs::remove_file(&db_path);
    }

    #[test]
    fn ws_상태_스트림_프레임에_주입_상태가_반영된다() {
        let server = start(None);
        let mut ws = ws_client(server.local_addr());
        send_text(
            &mut ws,
            &format!(r#"{{"type":"auth","v":1,"token":"{TEST_TOKEN}"}}"#),
        );
        // 접속 등록 후 세션 상태 이벤트를 주입 → 대시보드 프레임으로 흘러야 한다
        assert!(read_frame_of_type(&mut ws, "welcome", Duration::from_secs(3)).is_some());
        seed_ids(&server, &[42]); // UUID 매핑(MuxUpdated) — 폰에는 UUID만 노출된다 (I1)
        server
            .core
            .dashboard()
            .inject_event(runtime::RuntimeEvent::SessionStatusChanged {
                session: runtime::SessionId(42),
                status: runtime::SessionStatus::NeedsApproval,
            });
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut seen = false;
        while Instant::now() < deadline {
            if let Some(frame) = read_frame_of_type(&mut ws, "dashboard", Duration::from_secs(1))
                && frame.contains(r#""status":"needs_approval""#)
                && frame.contains(r#""id":"uuid-42""#)
            {
                seen = true;
                break;
            }
        }
        assert!(seen, "주입한 상태가 대시보드 프레임에 반영되지 않음");
    }

    #[test]
    fn 접속_0에서는_승인_폴링이_정지한다() {
        let db_path = temp_db_path();
        let server = start_with_db(None, Some(db_path.clone()));
        // 접속이 없으면 폴링 타이머가 없다 — 잠시 기다려도 poll_count 0
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(
            server.core.dashboard().poll_count(),
            0,
            "접속 0인데 승인 폴링이 돌았다"
        );

        // 접속이 생기면 즉시 폴링(force) → 카운트 증가
        let mut ws = ws_client(server.local_addr());
        send_text(
            &mut ws,
            &format!(r#"{{"type":"auth","v":1,"token":"{TEST_TOKEN}"}}"#),
        );
        let deadline = Instant::now() + Duration::from_secs(3);
        while server.core.dashboard().poll_count() == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            server.core.dashboard().poll_count() >= 1,
            "접속 후에도 폴링이 안 돌았다"
        );
        drop(ws);
        let _ = std::fs::remove_file(&db_path);
    }

    #[test]
    fn get_이외_메서드는_405() {
        let server = start(None);
        let response = request(
            server.local_addr(),
            "POST /healthz HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 0\r\n\r\n",
        );
        assert!(response.starts_with("HTTP/1.1 405"), "{response}");
    }

    #[test]
    fn 기형_요청은_400() {
        let server = start(None);
        let response = request(server.local_addr(), "NOT-HTTP\r\n\r\n");
        assert!(response.starts_with("HTTP/1.1 400"), "{response}");
    }

    /// 매 read가 (syscall 타임아웃 전에) 1바이트씩 성공하는 트리클 피어 시뮬레이션 —
    /// 개행 없이 무한 공급해 read_line이 스스로 끝나지 않게 한다.
    struct TrickleReader;

    impl Read for TrickleReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            std::thread::sleep(Duration::from_millis(2));
            buf[0] = b'a';
            Ok(1)
        }
    }

    #[test]
    fn 트리클_피어는_head_총_데드라인에서_컷된다() {
        let deadline = Duration::from_millis(50);
        let start = Instant::now();
        let mut reader = BufReader::new(DeadlineReader {
            inner: TrickleReader,
            deadline: start + deadline,
        });
        let err = http::read_request_head(&mut reader).unwrap_err();
        let elapsed = start.elapsed();
        // 타임아웃은 응답 없는 종료(Closed)로 분류된다
        assert_eq!(err, http::HeadError::Closed);
        // syscall 단위 read는 계속 성공하지만 총 소요시간 데드라인이 끊는다 —
        // head 예산(8KB × 2ms ≈ 16초) 소진보다 훨씬 먼저.
        assert!(elapsed >= deadline, "{elapsed:?}");
        assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
    }

    #[test]
    fn host_허용_판정_단위() {
        // loopback 계열은 항상 허용
        assert!(host_allowed(Some("127.0.0.1"), None));
        assert!(host_allowed(Some("127.0.0.1:8737"), None));
        assert!(host_allowed(Some("localhost:80"), None));
        assert!(host_allowed(Some("[::1]:8737"), None));
        // 설정 호스트명 (대소문자/포트 무시)
        assert!(host_allowed(
            Some("Mac.Tail.ts.NET:443"),
            Some("mac.tail.ts.net")
        ));
        // 불일치/미설정/부재는 거부
        assert!(!host_allowed(Some("mac.tail.ts.net"), None));
        assert!(!host_allowed(Some("evil.example"), Some("mac.tail.ts.net")));
        assert!(!host_allowed(None, Some("mac.tail.ts.net")));
    }

    /// pane→세션 목록만 담은 MuxUpdated(제목 갱신용). 상태는 담지 않는다.
    fn mux_event(panes: &[(u64, &str)]) -> runtime::RuntimeEvent {
        runtime::RuntimeEvent::MuxUpdated {
            snapshot: Arc::new(runtime::MuxSnapshot {
                tabs: vec![runtime::TabSnapshot {
                    id: runtime::MuxTabId("tab-1".into()),
                    title: "tab".into(),
                    layout: runtime::LayoutNode::Pane(runtime::MuxPaneId("p-1".into())),
                    panes: panes
                        .iter()
                        .map(|(id, title)| runtime::PaneSnapshot {
                            id: runtime::MuxPaneId(id.to_string()),
                            session_id: Some(runtime::SessionId(*id)),
                            title: (*title).to_owned(),
                            persistent_session_id: Some(format!("uuid-{id}")),
                        })
                        .collect(),
                }],
                active_tab: Some(runtime::MuxTabId("tab-1".into())),
                focused_pane: Some(runtime::MuxPaneId("p-1".into())),
            }),
        }
    }

    /// 데드라인 내에 `needle`을 포함한 대시보드 프레임을 찾아 그 전문을 돌려준다.
    fn wait_dashboard_frame(ws: &mut WebSocket<TcpStream>, needle: &str) -> Option<String> {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if let Some(frame) = read_frame_of_type(ws, "dashboard", Duration::from_secs(1))
                && frame.contains(needle)
            {
                return Some(frame);
            }
        }
        None
    }

    /// 인증까지 마쳐 WS 슬롯을 잡은 클라이언트(welcome 수신으로 등록 완료 확인).
    fn ws_client_authed(addr: SocketAddr) -> WebSocket<TcpStream> {
        let mut ws = ws_client(addr);
        send_text(
            &mut ws,
            &format!(r#"{{"type":"auth","v":1,"token":"{TEST_TOKEN}"}}"#),
        );
        assert!(
            read_frame_of_type(&mut ws, "welcome", Duration::from_secs(3)).is_some(),
            "welcome 프레임 없음"
        );
        ws
    }

    /// (정적, WS) 슬롯이 기대치가 될 때까지 대기한다(비동기 등록/해제 반영).
    fn wait_slots(server: &WebRemoteServer, want: (usize, usize)) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if server.slot_counts() == want {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    /// 앱이 push한 워크스페이스 스냅샷 하나(활성) — 테스트 헬퍼.
    fn active_seed(sessions: Vec<dashboard::SessionSeed>) -> Vec<dashboard::WorkspaceSeed> {
        vec![dashboard::WorkspaceSeed {
            id: "ws-1".to_owned(),
            name: "프로젝트".to_owned(),
            current_directory: None,
            state: dashboard::WorkspaceState::Active,
            sessions,
        }]
    }

    /// 브리지에 UUID↔u64 매핑을 심는다 — 실경로에서는 MuxUpdated가 채운다 (I1).
    /// 세션 u64 → "uuid-N".
    fn seed_ids(server: &WebRemoteServer, sessions: &[u64]) {
        let panes: Vec<(u64, &str)> = sessions.iter().map(|s| (*s, "p")).collect();
        server.core.dashboard().inject_event(mux_event(&panes));
    }

    /// 앱의 재구독 경로(start_web/rebind)와 동일하게 상태 시딩 + 표시 스냅샷을 적용한다.
    /// 매 프레임 경로는 set_workspaces만 부른다 — 상태를 덮지 않는다(리뷰 P1-1).
    fn apply_seed(server: &WebRemoteServer, seeds: Vec<dashboard::WorkspaceSeed>) {
        if let Some(active) = seeds
            .iter()
            .find(|ws| ws.state == dashboard::WorkspaceState::Active)
        {
            // 활성 세션의 UUID 매핑 — 실경로에서는 MuxUpdated가 채운다 (I1).
            let ids: Vec<u64> = active.sessions.iter().filter_map(|s| s.id).collect();
            if !ids.is_empty() {
                seed_ids(server, &ids);
            }
            server.reseed_active_sessions(&active.sessions);
        }
        server.set_workspaces(seeds);
    }

    // ── P2-1: 대시보드 상태 시드 ───────────────────────────────────────────
    #[test]
    fn ws_시드_상태가_프레임에_반영되고_mux는_제목을_덮지_않는다() {
        let server = start(None);
        let addr = server.local_addr();
        // 접속(재구독) 전에 needs_approval로 시드 — 이벤트 이력 없는 새 구독자가 즉시 반영해야 한다
        apply_seed(
            &server,
            active_seed(vec![dashboard::SessionSeed {
                id: Some(7),
                title: "claude".to_owned(),
                status: Some(runtime::SessionStatus::NeedsApproval),
                agent: Some("Claude · sonnet · high".to_owned()),
                exited: false,
            }]),
        );
        let mut ws = ws_client_authed(addr);
        let frame = wait_dashboard_frame(&mut ws, r#""id":"uuid-7""#).expect("시드 프레임 없음");
        assert!(frame.contains(r#""status":"needs_approval""#), "{frame}");
        assert!(frame.contains(r#""title":"claude""#), "{frame}");
        assert!(frame.contains(r#""state":"active""#), "{frame}");

        // MuxUpdated는 소속(멤버십)만 근거다 — raw pane 제목("workspace.spawn.shell 140")으로
        // 앱이 해석한 표시명을 덮지 않고, 시드된 상태도 Running으로 리셋하지 않는다.
        server
            .core
            .dashboard()
            .inject_event(mux_event(&[(7, "workspace.spawn.shell 140")]));
        std::thread::sleep(Duration::from_millis(400));
        let frame = wait_dashboard_frame(&mut ws, r#""id":"uuid-7""#).unwrap_or_default();
        if !frame.is_empty() {
            assert!(
                !frame.contains("workspace.spawn.shell"),
                "mux raw 제목이 표시명을 덮었다: {frame}"
            );
            assert!(
                frame.contains(r#""status":"needs_approval""#),
                "MuxUpdated가 시드된 상태를 덮었다: {frame}"
            );
        }
        drop(ws);
    }

    /// 폰에도 전체 워크스페이스가 보인다 — warm/유휴는 표시 전용(세션 id 없음).
    #[test]
    fn ws_대시보드는_전체_워크스페이스를_싣고_비활성은_표시전용이다() {
        let server = start(None);
        let addr = server.local_addr();
        apply_seed(
            &server,
            vec![
                dashboard::WorkspaceSeed {
                    id: "ws-1".to_owned(),
                    name: "deppy-sijo".to_owned(),
                    current_directory: None,
                    state: dashboard::WorkspaceState::Active,
                    sessions: vec![dashboard::SessionSeed {
                        id: Some(7),
                        title: "deppy-sijo".to_owned(),
                        status: Some(runtime::SessionStatus::Running),
                        agent: Some("Codex · gpt-5.5 · high".to_owned()),
                        exited: false,
                    }],
                },
                dashboard::WorkspaceSeed {
                    id: "ws-2".to_owned(),
                    name: "source".to_owned(),
                    current_directory: None,
                    state: dashboard::WorkspaceState::Warm,
                    sessions: vec![dashboard::SessionSeed {
                        id: None,
                        title: "deppy-mux".to_owned(),
                        status: None,
                        agent: None,
                        exited: false,
                    }],
                },
            ],
        );
        let mut ws = ws_client_authed(addr);
        let frame =
            wait_dashboard_frame(&mut ws, r#""state":"warm""#).expect("warm 워크스페이스 없음");
        assert!(frame.contains(r#""name":"deppy-sijo""#), "{frame}");
        assert!(frame.contains(r#""name":"source""#), "{frame}");
        // 활성 세션만 id를 싣는다(시청 대상) — warm 세션은 id 없이 이름만
        assert!(frame.contains(r#""id":"uuid-7""#), "{frame}");
        assert!(frame.contains(r#""title":"deppy-mux""#), "{frame}");
        drop(ws);
    }

    #[test]
    fn ws_재시드는_옛_워크스페이스_세션을_교체한다() {
        let server = start(None);
        let addr = server.local_addr();
        // 워크스페이스 A: 세션 1 = needs_approval
        apply_seed(
            &server,
            active_seed(vec![dashboard::SessionSeed {
                id: Some(1),
                title: "A".to_owned(),
                status: Some(runtime::SessionStatus::NeedsApproval),
                agent: None,
                exited: false,
            }]),
        );
        let mut ws = ws_client_authed(addr);
        assert!(
            wait_dashboard_frame(&mut ws, r#""id":"uuid-1""#).is_some(),
            "워크스페이스 A 시드 미반영"
        );
        // 전환: 워크스페이스 B로 재시드(세션 2 = error). 세션 맵 통째 교체라 A의 세션 1은 사라진다
        // — MuxUpdated 재발화를 놓쳐도 옛 워크스페이스 세션이 정체되지 않는다(전환 레이스 해소).
        apply_seed(
            &server,
            active_seed(vec![dashboard::SessionSeed {
                id: Some(2),
                title: "B".to_owned(),
                status: Some(runtime::SessionStatus::Error),
                agent: None,
                exited: false,
            }]),
        );
        let frame = wait_dashboard_frame(&mut ws, r#""id":"uuid-2""#)
            .expect("워크스페이스 B 재시드 미반영");
        assert!(frame.contains(r#""status":"error""#), "{frame}");
        assert!(
            !frame.contains(r#""id":"uuid-1""#),
            "옛 워크스페이스 세션이 남았다: {frame}"
        );
        drop(ws);
    }

    // ── P2-2: WS/정적 슬롯 분리 ────────────────────────────────────────────
    #[test]
    fn ws가_정적_슬롯을_모두_점유해도_정적_요청은_즉시_처리된다() {
        let server = start(None);
        let addr = server.local_addr();
        // 정적 슬롯 수(=MAX_CONNECTIONS)만큼 WS를 열어 둔다 — 옛 설계라면 정적 슬롯을 전부
        // 굶겼을 상황. WS는 별도 슬롯을 쓰므로 정적 풀은 그대로 비어 있어야 한다.
        let holders: Vec<WebSocket<TcpStream>> = (0..MAX_CONNECTIONS)
            .map(|_| ws_client_authed(addr))
            .collect();
        assert!(
            wait_slots(&server, (0, MAX_CONNECTIONS)),
            "WS 슬롯이 이관되지 않음: {:?}",
            server.slot_counts()
        );
        // 정적 GET이 슬롯 대기 없이 즉시 200
        let resp = get(addr, "/healthz");
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        drop(holders);
    }

    #[test]
    fn ws_슬롯_상한_초과는_503으로_거부된다() {
        let server = start(None);
        let addr = server.local_addr();
        let holders: Vec<WebSocket<TcpStream>> = (0..MAX_WS_CONNECTIONS)
            .map(|_| ws_client_authed(addr))
            .collect();
        assert!(
            wait_slots(&server, (0, MAX_WS_CONNECTIONS)),
            "WS 슬롯이 상한까지 차지 않음: {:?}",
            server.slot_counts()
        );
        // 상한 초과 WS 업그레이드는 핸드셰이크 전에 503으로 거부(브라우저는 백오프 재접속)
        let resp = request(
            addr,
            "GET /ws HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
        );
        assert!(resp.starts_with("HTTP/1.1 503"), "{resp}");
        drop(holders);
    }

    #[test]
    fn ws_종료_후_슬롯이_복원된다() {
        let server = start(None);
        let addr = server.local_addr();
        let ws = ws_client_authed(addr);
        assert!(
            wait_slots(&server, (0, 1)),
            "WS 슬롯 이관 안 됨: {:?}",
            server.slot_counts()
        );
        drop(ws); // 접속 종료 — 서버가 WS 슬롯을 반납해야 한다(누수 없음)
        assert!(
            wait_slots(&server, (0, 0)),
            "WS 종료 후 슬롯이 복원되지 않음: {:?}",
            server.slot_counts()
        );
    }

    #[test]
    fn 비_ws_경로_업그레이드는_정적_라우팅으로_흐른다() {
        // /ws 아닌 경로의 Upgrade 요청은 WS 승격 대상이 아니다(P3-3) — 정적 라우팅으로 흘러
        // 화이트리스트 밖이면 404. (WS 슬롯을 잡지 않는다.)
        let server = start(None);
        let addr = server.local_addr();
        let resp = request(
            addr,
            "GET /nope HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
        );
        assert!(resp.starts_with("HTTP/1.1 404"), "{resp}");
        assert_eq!(
            server.slot_counts().1,
            0,
            "비-/ws 업그레이드가 WS 슬롯을 잡았다"
        );
    }

    // ── P4: 웹푸시 HTTP 통합(서버 소켓 경유 — http.rs 본문 읽기 + 라우팅) ──────
    /// 공유 코어에 **서버가 소유한** 발송 싱크를 심지 못하게 한다. 심으면 서버를 끄는
    /// 순간 코어가 죽은 싱크를 가리키게 되고, Tailscale을 끄는 것만으로 Relay 쪽 코어가
    /// 망가진다. 공유 배치의 웹푸시 소유권은 아직 정해지지 않았으므로 조용히 무시하지 않고
    /// 명시적으로 거절한다.
    #[test]
    fn 공유_코어에는_서버_소유_웹푸시_싱크를_붙일_수_없다() {
        let core = session_core::SessionCore::spawn(None);
        let refused = WebRemoteServer::serve_with_core(
            SocketAddr::from(([127, 0, 0, 1], 0)),
            ServeOptions {
                token: TEST_TOKEN.to_owned(),
                allowed_host: None,
                repository: None,
                vapid: Some(push::VapidKey::generate()),
                uploads_dir: None,
            },
            Arc::clone(&core),
        );
        let error = refused.err().expect("거절돼야 한다");
        assert!(format!("{error:#}").contains("공유 코어"), "{error:#}");

        // 거절이 공유 코어를 건드리지 않았다 — 여전히 자기 브리지를 돌린다.
        let guard = core.dashboard().register_connection();
        let before = core.dashboard().dash_build_count();
        core.dashboard().set_notice(Some("untouched".to_owned()));
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut rebuilt = false;
        while Instant::now() < deadline {
            if core.dashboard().dash_build_count() > before {
                rebuilt = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(guard);
        assert!(rebuilt, "거절된 조합이 공유 코어를 멈추면 안 된다");
        core.shutdown();
    }

    /// 「둘 다 켬」 배치: 코어가 웹푸시를 소유하고 서버는 핸들만 빌린다. 서버를 꺼도 코어와
    /// 발송기는 살아 있어야 한다 — Tailscale을 끄는 것이 Relay 쪽 푸시를 죽이면 안 된다.
    #[test]
    fn 공유_코어가_웹푸시를_소유하고_서버_종료에도_살아남는다() {
        let db_path = temp_db_path();
        let repository = repository::StorageTestRepository::open(&db_path)
            as Arc<dyn repository::WebRemoteRepository>;
        let core = session_core::SessionCore::spawn_with_push(
            Some(repository.clone()),
            Some(push::VapidKey::generate()),
        );
        assert!(
            core.push_handle().is_some(),
            "코어가 발송기를 소유해야 한다"
        );

        let server = WebRemoteServer::serve_with_core(
            SocketAddr::from(([127, 0, 0, 1], 0)),
            ServeOptions {
                token: TEST_TOKEN.to_owned(),
                allowed_host: None,
                repository: Some(repository),
                // 소유권은 코어에 있으므로 서버에는 키를 넘기지 않는다.
                vapid: None,
                uploads_dir: None,
            },
            Arc::clone(&core),
        )
        .unwrap();
        assert!(
            server.core().push_handle().is_some(),
            "서버가 핸들을 빌린다"
        );

        server.shutdown();
        assert!(
            core.push_handle().is_some(),
            "서버 종료가 코어의 발송기를 죽이면 안 된다"
        );

        core.shutdown();
        assert!(
            core.push_handle().is_none(),
            "코어 소유자가 멈추면 발송기도 함께 정리된다"
        );
        let _ = std::fs::remove_file(&db_path);
    }

    /// Relay를 먼저 켜면 코어가 VAPID 키 없이 만들어진다. 그 뒤 web을 켤 때 기존 코어를
    /// 재사용하면서 발송기를 보정하지 않으면, 키가 있는데도 웹푸시가 영영 꺼진 채로 남는다.
    #[test]
    fn ensure_push는_키_없이_만들어진_코어를_나중에_보정한다() {
        let db_path = temp_db_path();
        let repository = repository::StorageTestRepository::open(&db_path)
            as Arc<dyn repository::WebRemoteRepository>;

        // Relay가 먼저 켜진 상황 — 키 없이 만들어진다.
        let core = session_core::SessionCore::spawn(Some(repository.clone()));
        assert!(core.push_handle().is_none());

        // 나중에 web이 켜지며 키를 들고 온다.
        core.ensure_push(Some(repository.clone()), Some(push::VapidKey::generate()));
        assert!(
            core.push_handle().is_some(),
            "키가 생겼는데도 푸시가 꺼진 채면 안 된다"
        );

        // 두 번 불러도 발송기를 갈아 끼우지 않는다.
        core.ensure_push(Some(repository), Some(push::VapidKey::generate()));
        assert!(core.push_handle().is_some());

        core.shutdown();
        let _ = std::fs::remove_file(&db_path);
    }

    fn start_with_push(db_path: PathBuf) -> WebRemoteServer {
        let repository = repository::StorageTestRepository::open(&db_path)
            as Arc<dyn repository::WebRemoteRepository>;
        WebRemoteServer::serve(
            SocketAddr::from(([127, 0, 0, 1], 0)),
            ServeOptions {
                token: TEST_TOKEN.to_owned(),
                allowed_host: None,
                repository: Some(repository),
                vapid: Some(push::VapidKey::generate()),
                uploads_dir: None,
            },
        )
        .unwrap()
    }

    #[test]
    fn push_vapid_공개키_get은_200_json() {
        let db_path = temp_db_path();
        let _ = storage::Db::open(&db_path).unwrap();
        let server = start_with_push(db_path.clone());
        let addr = server.local_addr();
        let ok = get(addr, &format!("/push/vapid?token={TEST_TOKEN}"));
        assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
        assert!(ok.contains(r#""key""#), "{ok}");
        // 잘못된 토큰 → 401.
        let bad = get(addr, "/push/vapid?token=wrong");
        assert!(bad.starts_with("HTTP/1.1 401"), "{bad}");
        server.shutdown();
        let _ = std::fs::remove_file(&db_path);
    }

    #[test]
    fn push_구독_등록_post는_201이고_db에_저장된다() {
        let db_path = temp_db_path();
        let _ = storage::Db::open(&db_path).unwrap();
        let server = start_with_push(db_path.clone());
        let addr = server.local_addr();
        let body = r#"{"endpoint":"https://push.example/sock","keys":{"p256dh":"k","auth":"a"}}"#;
        let req = format!(
            "POST /push/subscribe?token={TEST_TOKEN} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = request(addr, &req);
        assert!(resp.starts_with("HTTP/1.1 201"), "{resp}");
        // 소켓 경유 본문이 정확히 파싱돼 DB에 저장됐다(http.rs read_body + 핸드오프 검증).
        let db = storage::Db::open(&db_path).unwrap();
        let subs = db.list_web_push_subscriptions().unwrap();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].endpoint, "https://push.example/sock");
        server.shutdown();
        let _ = std::fs::remove_file(&db_path);
    }

    #[test]
    fn push_본문_상한_초과는_413() {
        let db_path = temp_db_path();
        let _ = storage::Db::open(&db_path).unwrap();
        let server = start_with_push(db_path.clone());
        let addr = server.local_addr();
        // 본문 없이 과대 Content-Length만 선언해도 상한에서 413(본문 읽기 전 거부).
        let req = format!(
            "POST /push/subscribe?token={TEST_TOKEN} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\n\r\n",
            http::MAX_BODY_BYTES + 1
        );
        let resp = request(addr, &req);
        assert!(resp.starts_with("HTTP/1.1 413"), "{resp}");
        server.shutdown();
        let _ = std::fs::remove_file(&db_path);
    }

    #[test]
    fn push_비활성_서버는_구독_엔드포인트가_404() {
        // vapid 없이 시작한 서버(start_with_db)는 push 비활성 — /push/*는 404.
        let db_path = temp_db_path();
        let server = start_with_db(None, Some(db_path.clone()));
        let addr = server.local_addr();
        let resp = get(addr, &format!("/push/vapid?token={TEST_TOKEN}"));
        assert!(resp.starts_with("HTTP/1.1 404"), "{resp}");
        server.shutdown();
        let _ = std::fs::remove_file(&db_path);
    }

    // ── P6d: 모바일 파일 첨부 HTTP 통합(서버 소켓 경유) ─────────────────────
    fn temp_uploads_dir() -> PathBuf {
        std::env::temp_dir().join(format!(
            "web-remote-uploads-test-{}",
            uuid::Uuid::new_v4().simple()
        ))
    }

    /// 본문이 있는 POST 요청을 보낸다(Content-Type 지정).
    fn post_upload(addr: SocketAddr, token: &str, content_type: &str, body: &[u8]) -> Vec<u8> {
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut head = format!(
            "POST /upload?token={token} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        head.extend_from_slice(body);
        stream.write_all(&head).unwrap();
        let mut out = Vec::new();
        let _ = stream.read_to_end(&mut out);
        out
    }

    #[test]
    fn 업로드는_토큰_없으면_401() {
        let dir = temp_uploads_dir();
        let server = start_with_uploads(dir.clone());
        let addr = server.local_addr();
        let resp = post_upload(addr, "wrong", "image/png", b"fake-png");
        assert!(
            String::from_utf8_lossy(&resp).starts_with("HTTP/1.1 401"),
            "{}",
            String::from_utf8_lossy(&resp)
        );
        server.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 업로드는_uploads_dir_미배선이면_404() {
        // start_with_db(uploads_dir 없음)는 업로드 비활성 — 토큰이 맞아도 404.
        let server = start_with_db(None, None);
        let addr = server.local_addr();
        let resp = post_upload(addr, TEST_TOKEN, "image/png", b"fake-png");
        assert!(
            String::from_utf8_lossy(&resp).starts_with("HTTP/1.1 404"),
            "{}",
            String::from_utf8_lossy(&resp)
        );
        server.shutdown();
    }

    #[test]
    fn 업로드는_화이트리스트_밖_타입은_415() {
        let dir = temp_uploads_dir();
        let server = start_with_uploads(dir.clone());
        let addr = server.local_addr();
        let resp = post_upload(addr, TEST_TOKEN, "application/x-sh", b"#!/bin/sh\necho hi");
        assert!(
            String::from_utf8_lossy(&resp).starts_with("HTTP/1.1 415"),
            "{}",
            String::from_utf8_lossy(&resp)
        );
        // 디렉터리에 아무 파일도 생기지 않는다(거부된 업로드는 저장하지 않는다).
        assert!(
            std::fs::read_dir(&dir)
                .map(|mut it| it.next().is_none())
                .unwrap_or(true)
        );
        server.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 업로드는_본문_상한_초과시_413() {
        let dir = temp_uploads_dir();
        let server = start_with_uploads(dir.clone());
        let addr = server.local_addr();
        // 본문 없이 과대 Content-Length만 선언해도 상한(10MB)에서 413(본문 읽기 전 거부).
        let req = format!(
            "POST /upload?token={TEST_TOKEN} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: image/png\r\nContent-Length: {}\r\n\r\n",
            upload::MAX_UPLOAD_BYTES + 1
        );
        let resp = request(addr, &req);
        assert!(resp.starts_with("HTTP/1.1 413"), "{resp}");
        server.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 업로드는_토큰_틀리면_본문_읽기_전에_401한다() {
        // 리뷰 P2-1 회귀 방지: 미인증 /upload가 10MB 본문을 선할당·읽으면 안 된다.
        // 틀린 토큰 + 상한 이내 Content-Length를 선언하되 **본문은 안 보낸다**. 토큰을
        // 본문보다 먼저 검사하면 즉시 401; 안 하면 서버가 100만 바이트를 기다리다
        // 데드라인 후 400/절단이 난다. 401이면 본문 읽기 전 거부가 확인된다.
        let dir = temp_uploads_dir();
        let server = start_with_uploads(dir.clone());
        let addr = server.local_addr();
        let req = "POST /upload?token=wrong HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: image/png\r\nContent-Length: 1000000\r\n\r\n";
        let resp = request(addr, req);
        assert!(resp.starts_with("HTTP/1.1 401"), "{resp}");
        server.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 업로드_성공은_201과_파일_존재를_보장한다() {
        let dir = temp_uploads_dir();
        let server = start_with_uploads(dir.clone());
        let addr = server.local_addr();
        let body = b"fake-png-bytes";
        let resp = post_upload(addr, TEST_TOKEN, "image/png", body);
        let text = String::from_utf8_lossy(&resp);
        assert!(text.starts_with("HTTP/1.1 201"), "{text}");
        // 헤더/본문 분리 후 JSON에서 path를 뽑는다.
        let json_start = text.find("\r\n\r\n").expect("본문 없음") + 4;
        let json: serde_json::Value = serde_json::from_str(&text[json_start..]).unwrap();
        let path = json["path"].as_str().expect("path 필드 없음");
        // 서버 생성 경로 — uploads_dir 아래 uuid.png, 절대경로, 파일이 실재하고 내용 일치.
        assert!(std::path::Path::new(path).is_absolute(), "{path}");
        assert!(path.ends_with(".png"), "{path}");
        assert_eq!(std::fs::read(path).unwrap(), body);
        server.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 업로드는_빈_본문을_거부한다() {
        let dir = temp_uploads_dir();
        let server = start_with_uploads(dir.clone());
        let addr = server.local_addr();
        let resp = post_upload(addr, TEST_TOKEN, "image/png", b"");
        assert!(
            String::from_utf8_lossy(&resp).starts_with("HTTP/1.1 400"),
            "{}",
            String::from_utf8_lossy(&resp)
        );
        server.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
