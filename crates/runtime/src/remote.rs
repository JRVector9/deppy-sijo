//! Remote Transport (설계문서 PR-19 스켈레톤 + v1 auth/바이너리 프레이밍).
//! localhost-only attach — InProcessRuntimeClient와 **같은 명령/이벤트 모델**을
//! length-prefixed 바이너리 프레임([u32 LE len][postcard])으로 loopback TCP에
//! 실어 나른다 (JSON Vec<u8> 숫자 배열 팽창 해소).
//! 인증/핸드셰이크(v2, protocol.rs): 클라이언트가 첫 프레임으로 ClientHello
//! (매직/버전/features/토큰)를 보내고 서버가 ServerHello(버전/협상된 features)로
//! 응답한다 — 토큰은 실행마다 생성, §1.5 "auth required by default". 매직/버전/토큰
//! 불일치·무응답(5s)은 즉시 종료. **단계 B: 양측이 delta viewport를 광고하므로
//! loopback은 Codec::Delta로 협상**된다 — pump는 세션별 last_sent 대비 viewport를
//! keyframe/delta로, reader는 recon 상태로 재구성한다(§4). 2-스레드 구조(서버
//! reader+pump / 클라이언트 writer+reader)는 그대로. delta 미협상 접속(Plain)은
//! 여전히 v1과 바이트 동일.
//! public remote는 아직 아니다: bind는 127.0.0.1 고정, attach는 loopback만 허용.
//! (TLS/delta 스트림은 v1+ — §8.2 "Viewport는 terminal delta로 대체되는 자리")
//!
//! 신뢰 경계 주의 (public remote 전 필수 — codex 리뷰): SpawnAgent가 command/args/
//! env/credential_id를 그대로 실어 나르므로, 이 프로토콜을 공개 네트워크에 내놓기
//! 전에 인증·권한(capability) 레이어나 제한된 원격 명령 스키마가 반드시 선행해야 한다.
//! 지금은 loopback 강제가 그 경계다. 와이어 값 검증(validate_command/validate_event)은
//! 기형 peer 방어일 뿐 권한 통제가 아니다.

use std::collections::{HashMap, HashSet};
use std::io::{BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use anyhow::{Context, bail};
use deppy_core::SessionId;
use terminal::TerminalViewportSnapshot;

use crate::client::{RuntimeCommandSink, RuntimeEventReceiver, RuntimeEventStream};
use crate::command::RuntimeCommand;
use crate::event::RuntimeEvent;
use crate::in_process::InProcessRuntimeClient;
use crate::protocol::{
    CLIENT_FEATURES, ClientHello, Codec, DecodedCommand, DecodedEvent, PROTO_MAGIC, PROTO_VERSION,
    SERVER_FEATURES, ServerHello, WireMsg, diff_viewport, encode_request_keyframe, encode_wire_msg,
    try_apply_delta,
};

/// 이벤트 pump 폴링 주기 — worker의 output batch와 별개인 전송 주기.
const PUMP_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);
/// heartbeat 주기 — 이 시간 동안 보낼 이벤트가 없으면 길이 0 프레임(keepalive)을
/// 보내 half-open(죽은) peer를 조기에 감지한다. write 실패 = 죽은 peer로 접속 정리.
const HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);
/// 프레임 payload 상한 — 바이너리라 팽창이 없으므로(postcard) 원본 크기 기준.
/// 대형 붙여넣기(수 MB)와 큰 viewport 스냅샷이 여유 있게 들어간다.
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
/// 원격 명령의 scrollback 상한 — 무제한 usize로 과대 할당을 요구하지 못하게.
const MAX_SCROLLBACK_LINES: usize = 100_000;
/// 인증 프레임 대기 상한 — 접속만 열고 침묵하는 peer가 서버를 잡아두지 못하게.
const AUTH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// 프레임 하나를 쓴다: [u32 LE 길이][payload].
/// 상한은 송신측에서도 강제 — 초과분을 보내 놓고 peer가 끊는 것보다
/// 로컬에서 즉시 실패하는 쪽이 진단 가능하다 (codex 리뷰).
fn write_frame(stream: &mut impl Write, payload: &[u8]) -> std::io::Result<()> {
    if payload.len() > MAX_FRAME_BYTES {
        return Err(std::io::Error::other(format!(
            "frame이 상한({MAX_FRAME_BYTES}B)을 초과: {}B",
            payload.len()
        )));
    }
    let len = u32::try_from(payload.len())
        .map_err(|_| std::io::Error::other("frame이 u32 길이를 초과"))?;
    stream.write_all(&len.to_le_bytes())?;
    stream.write_all(payload)
}

/// 프레임 하나를 읽는다. 상한 초과/EOF/IO 에러는 None — 호출측은 접속을 끝낸다.
fn read_frame(reader: &mut impl Read) -> Option<Vec<u8>> {
    let mut len_bytes = [0u8; 4];
    reader.read_exact(&mut len_bytes).ok()?;
    let len = u32::from_le_bytes(len_bytes) as usize;
    if len > MAX_FRAME_BYTES {
        return None; // 프로토콜 위반 — 폭주 할당 방지
    }
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload).ok()?;
    Some(payload)
}

/// 상수 시간 비교 — 토큰 길이/내용의 타이밍 누설 방지.
fn token_matches(expected: &str, provided: &[u8]) -> bool {
    let expected = expected.as_bytes();
    if expected.len() != provided.len() {
        return false;
    }
    expected
        .iter()
        .zip(provided)
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

/// 서버측 핸드셰이크 v2 (§3.1): 첫 프레임 = [`ClientHello`].
/// 매직/버전 검증 → 토큰 상수시간 비교 → features 교집합 → [`ServerHello`] 회신.
/// 성공 시 협상된 접속 [`Codec`]을 반환한다. 실패(침묵/기형/구버전 프레임/토큰 불일치)는
/// 소켓을 닫고 `None` — hang 없이 조기 거부한다(v1의 "인증 실패 = 즉시 종료" 계약 유지).
fn server_handshake(stream: &TcpStream, auth_token: &str) -> Option<Codec> {
    // 침묵 peer가 서버를 잡아두지 못하게 timeout (v1의 AUTH_TIMEOUT 계약).
    let _ = stream.set_read_timeout(Some(AUTH_TIMEOUT));
    let mut hs = stream.try_clone().ok()?;
    let reject = || {
        let _ = stream.shutdown(Shutdown::Both);
    };
    let Some(frame) = read_frame(&mut hs) else {
        // 침묵/EOF/상한 초과 — 조기 종료
        reject();
        return None;
    };
    // 매직/버전 불일치 또는 기형(구버전 원시 토큰 프레임 포함)은 조기 거부 (hang 없음).
    let hello = match postcard::from_bytes::<ClientHello>(&frame) {
        Ok(hello) if hello.magic == PROTO_MAGIC && hello.proto_version == PROTO_VERSION => hello,
        _ => {
            tracing::warn!("remote 핸드셰이크 실패(매직/버전/기형) — 접속 거부");
            reject();
            return None;
        }
    };
    if !token_matches(auth_token, &hello.token) {
        tracing::warn!("remote 인증 실패 — 접속 거부");
        reject();
        return None;
    }
    // features_ack = 서버 지원 ∩ 클라이언트 요청. 양측이 delta를 광고하면 Codec::Delta.
    let features = SERVER_FEATURES & hello.features;
    let server_hello = ServerHello {
        proto_version: PROTO_VERSION,
        features,
    };
    let payload = match postcard::to_allocvec(&server_hello) {
        Ok(payload) => payload,
        Err(e) => {
            tracing::warn!("remote ServerHello 직렬화 실패: {e}");
            reject();
            return None;
        }
    };
    // ServerHello 회신 — 클라이언트 attach가 인증/협상 결과를 동기적으로 안다 (v1 ACK 계약 계승).
    if write_frame(&mut hs, &payload).is_err() {
        reject();
        return None;
    }
    Some(Codec::from_features(features))
}

/// 서버가 수신한 명령의 와이어 값 검증 (codex 리뷰: 악성/기형 클라이언트 방어).
/// 터미널 모델은 내부에서 clamp하지만 PTY 경로는 원값을 받으므로 여기서 거른다.
fn validate_command(command: &RuntimeCommand) -> Result<(), &'static str> {
    match command {
        RuntimeCommand::SpawnShell {
            cols,
            rows,
            scrollback_lines,
        }
        | RuntimeCommand::SpawnAgent {
            cols,
            rows,
            scrollback_lines,
            ..
        } => {
            if *cols == 0 || *rows == 0 {
                return Err("cols/rows는 0일 수 없음");
            }
            if *scrollback_lines > MAX_SCROLLBACK_LINES {
                return Err("scrollback_lines 상한 초과");
            }
        }
        RuntimeCommand::Resize { cols, rows, .. } => {
            if *cols == 0 || *rows == 0 {
                return Err("cols/rows는 0일 수 없음");
            }
        }
        RuntimeCommand::SplitPane {
            scrollback_lines, ..
        } if *scrollback_lines > MAX_SCROLLBACK_LINES => {
            return Err("scrollback_lines 상한 초과");
        }
        _ => {}
    }
    Ok(())
}

/// 클라이언트가 수신한 이벤트의 와이어 값 검증 — 기형 스냅샷이 렌더러에
/// 닿기 전에 거른다 (cols=0 나눗셈, 셀 수 불일치, 비정상 ratio).
fn validate_event(event: &RuntimeEvent) -> Result<(), &'static str> {
    match event {
        RuntimeEvent::Viewport { snapshot, .. } => {
            if snapshot.cols == 0 || snapshot.rows == 0 {
                return Err("viewport cols/rows가 0");
            }
            let expected = snapshot.cols as usize * snapshot.rows as usize;
            if snapshot.visible_cells.len() != expected {
                return Err("visible_cells 크기가 cols*rows와 불일치");
            }
        }
        RuntimeEvent::MuxUpdated { snapshot } => {
            for tab in &snapshot.tabs {
                if !layout_ratios_valid(&tab.layout) {
                    return Err("layout ratio가 0..=1 finite 범위 밖");
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn layout_ratios_valid(node: &crate::LayoutNode) -> bool {
    match node {
        crate::LayoutNode::Pane(_) => true,
        crate::LayoutNode::Split {
            ratio,
            first,
            second,
            ..
        } => {
            ratio.is_finite()
                && (0.0..=1.0).contains(ratio)
                && layout_ratios_valid(first)
                && layout_ratios_valid(second)
        }
    }
}

/// in-process worker를 loopback TCP로 노출하는 서버.
/// 접속마다 스레드를 띄워 여러 클라이언트를 동시에 처리한다 — 접속당
/// reader(명령 수신)와 pump(이벤트 송신 + heartbeat) 스레드가 하나씩 붙는다.
pub struct RemoteRuntimeServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    accept_thread: Option<JoinHandle<()>>,
    backend: Option<Arc<InProcessRuntimeClient>>,
    /// 살아있는 접속들 — shutdown이 전부 소켓 종료 + join할 수 있게 추적한다.
    /// 접속 스레드는 끝날 때 스스로 자기 항목을 제거한다(자기 자신을 join하면
    /// 데드락이므로 join 없이 remove만; drop되는 JoinHandle은 자동 detach라
    /// 좀비 스레드가 남지 않는다).
    connections: Arc<Mutex<Vec<ConnEntry>>>,
    /// 이 실행의 attach 토큰 — 클라이언트가 첫 프레임으로 제시해야 한다 (§1.5)
    auth_token: String,
}

/// 살아있는 접속 하나 — shutdown 시 소켓 종료 + join 대상.
struct ConnEntry {
    stream: TcpStream,
    handle: JoinHandle<()>,
}

impl RemoteRuntimeServer {
    /// 127.0.0.1에만 bind한다 (localhost-only는 함수 형태로 보장 — 주소를 받지 않는다).
    /// port 0이면 OS가 할당하고 [`Self::local_addr`]로 확인한다.
    pub fn serve(backend: InProcessRuntimeClient, port: u16) -> anyhow::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", port)).context("remote 서버 bind 실패")?;
        let addr = listener.local_addr()?;
        let backend = Arc::new(backend);
        let stop = Arc::new(AtomicBool::new(false));
        let connections: Arc<Mutex<Vec<ConnEntry>>> = Arc::default();
        // 실행마다 새 토큰 (uuid v4 ×2 ≈ 244bit 엔트로피)
        let auth_token = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let accept_token = auth_token.clone();

        let accept_backend = Arc::clone(&backend);
        let accept_stop = Arc::clone(&stop);
        let accept_conns = Arc::clone(&connections);
        let accept_thread = std::thread::Builder::new()
            .name("remote-accept".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    if accept_stop.load(Ordering::SeqCst) {
                        break;
                    }
                    match stream {
                        Ok(stream) => {
                            // 접속마다 스레드를 띄워 동시 다중 클라이언트를 처리한다.
                            // 등록과 stop 재확인을 같은 락(connections) 안에서 하므로
                            // shutdown의 drain과 순서가 어느 쪽이든 race 창이 없다:
                            // shutdown이 먼저 잠그면 여기서 stop=true를 보고 즉시 닫고,
                            // 여기가 먼저 잠그고 등록하면 shutdown이 그다음에 잠가
                            // 이 접속까지 포함해 정리한다.
                            let shutdown_clone = match stream.try_clone() {
                                Ok(s) => s,
                                Err(e) => {
                                    tracing::warn!("remote stream clone 실패: {e}");
                                    continue;
                                }
                            };
                            let conn_backend = Arc::clone(&accept_backend);
                            let conn_stop = Arc::clone(&accept_stop);
                            let conn_token = accept_token.clone();
                            let conn_conns = Arc::clone(&accept_conns);

                            let mut conns = accept_conns.lock().expect("connections lock");
                            if accept_stop.load(Ordering::SeqCst) {
                                drop(conns);
                                let _ = stream.shutdown(Shutdown::Both);
                                continue;
                            }
                            let handle = match std::thread::Builder::new()
                                .name("remote-conn".into())
                                .spawn(move || {
                                    serve_connection(
                                        stream,
                                        &conn_backend,
                                        &conn_stop,
                                        &conn_token,
                                    );
                                    // 접속 종료 — 자기 항목을 스스로 제거(자기 join은
                                    // 데드락이라 하지 않는다; drop되는 JoinHandle은
                                    // 자동 detach라 좀비로 남지 않는다).
                                    let id = std::thread::current().id();
                                    conn_conns
                                        .lock()
                                        .expect("connections lock")
                                        .retain(|c| c.handle.thread().id() != id);
                                }) {
                                Ok(handle) => handle,
                                Err(e) => {
                                    drop(conns);
                                    tracing::warn!("remote 접속 스레드 생성 실패: {e}");
                                    continue;
                                }
                            };
                            conns.push(ConnEntry {
                                stream: shutdown_clone,
                                handle,
                            });
                        }
                        Err(e) => {
                            tracing::warn!("remote accept 실패: {e}");
                        }
                    }
                }
            })
            .context("remote accept thread 생성 실패")?;

        Ok(Self {
            addr,
            stop,
            accept_thread: Some(accept_thread),
            backend: Some(backend),
            connections,
            auth_token,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// attach에 필요한 토큰. 이 프로세스 밖으로 전달하는 방법(파일/클립보드 등)은
    /// 소비자 몫 — 로그에는 찍지 말 것.
    pub fn auth_token(&self) -> &str {
        &self.auth_token
    }

    /// accept 루프를 멈추고 worker까지 동기 종료한다.
    pub fn shutdown(self) {
        drop(self); // 정리는 Drop 한 곳에서 — 에러 경로의 drop도 같은 계약을 탄다
    }

    fn shutdown_impl(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // 살아있는 접속을 모두 깨운다: 소켓을 먼저 다 닫아(유휴 reader도 EOF로
        // 깨어나게) 두고 나서 join한다 — join 시점엔 이미 종료 신호가 갔으니
        // 서로 블록하지 않는다.
        let conns = std::mem::take(&mut *self.connections.lock().expect("connections lock"));
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
        // 접속 스레드가 모두 끝났으면 유일 소유자 — worker 정리
        if let Some(backend) = self.backend.take()
            && let Ok(mut backend) = Arc::try_unwrap(backend)
        {
            backend.shutdown();
        }
    }
}

// shutdown()을 부르지 않는 에러 경로에서도 소켓/스레드/worker가 정리되도록 —
// InProcessRuntimeClient와 같은 수명 계약 (codex 리뷰 P1)
impl Drop for RemoteRuntimeServer {
    fn drop(&mut self) {
        self.shutdown_impl();
    }
}

/// 한 클라이언트 접속을 처리한다: 첫 프레임으로 인증(§1.5 auth required),
/// 통과하면 이벤트 pump 스레드를 붙이고(유휴 시 heartbeat로 half-open peer
/// 감지) 이 스레드는 명령 프레임을 읽어 worker로 넘긴다. 기형 프레임은
/// 프로토콜 위반으로 접속을 끊는다.
fn serve_connection(
    stream: TcpStream,
    backend: &Arc<InProcessRuntimeClient>,
    stop: &Arc<AtomicBool>,
    auth_token: &str,
) {
    // 핸드셰이크 v2: 첫 프레임 = ClientHello. 침묵 peer가 서버를 잡아두지 못하게 timeout.
    let Some(codec) = server_handshake(&stream, auth_token) else {
        return;
    };
    let _ = stream.set_read_timeout(None);

    let receiver = backend.subscribe();
    let pump_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("remote stream clone 실패: {e}");
            return;
        }
    };
    // 접속별 종료 신호 — idle pump는 write가 없어 소켓 닫힘을 못 보므로
    // (write 에러로만 죽는다) reader 종료 경로가 이 플래그로 깨운다 (codex 리뷰)
    let conn_done = Arc::new(AtomicBool::new(false));
    let pump_done = Arc::clone(&conn_done);
    let pump_stop = Arc::clone(stop);
    // reader→pump keyframe 재동기화 채널 (§4.4-5): reader가 RequestKeyframe를 받으면
    // 여기에 세션을 쌓고, pump는 tick마다 비우며 해당 세션의 last_sent baseline을 버려
    // 다음 viewport가 keyframe이 되게 한다. 2-스레드 구조는 그대로 — pump가 last_sent를
    // 단독 소유하므로 이 큐만 스레드 간 공유한다.
    let keyframe_requests: Arc<Mutex<Vec<SessionId>>> = Arc::default();
    let pump_keyframe_requests = Arc::clone(&keyframe_requests);
    let pump = std::thread::Builder::new()
        .name("remote-pump".into())
        .spawn(move || {
            let mut stream = pump_stream;
            let mut last_activity = std::time::Instant::now();
            // 접속별 last_sent: 세션마다 (마지막 송신 seq, 그 스냅샷). Delta diff의 기준선.
            // Plain 접속에서는 사용되지 않는다(viewport도 encode_event 경로).
            let mut last_sent: HashMap<SessionId, (u64, Arc<TerminalViewportSnapshot>)> =
                HashMap::new();
            loop {
                if pump_stop.load(Ordering::SeqCst) || pump_done.load(Ordering::SeqCst) {
                    break;
                }
                // RequestKeyframe로 온 세션은 baseline을 버려 다음 viewport가 keyframe이 되게 한다.
                for session in pump_keyframe_requests
                    .lock()
                    .expect("keyframe req lock")
                    .drain(..)
                {
                    last_sent.remove(&session);
                }
                let mut sent_event = false;
                // 이 drain 배치 안에서 종료된 세션 (codex P2). drain()은 상태 이벤트를 먼저,
                // viewport slot을 나중에 붙이므로(client.rs) 같은 배치의 SessionExited(X)가
                // 뒤따르는 Viewport(X)보다 앞선다. 그 trailing viewport를 keyframe으로
                // 인코딩하면 방금 지운 last_sent[X]가 되살아나므로, 종료된 세션의 viewport는
                // baseline을 만들지 않는 plain full(WireMsg::Event)로 보낸다. 배치마다 리셋.
                let mut exited_this_batch: HashSet<SessionId> = HashSet::new();
                for event in receiver.drain() {
                    // 접속 코덱으로 인코딩. Delta는 viewport를 keyframe/delta로, 나머지는
                    // WireMsg::Event로. Plain은 postcard(RuntimeEvent)와 바이트 동일.
                    let payload = match encode_pump_frame(
                        codec,
                        &mut last_sent,
                        &mut exited_this_batch,
                        &event,
                    ) {
                        Ok(payload) => payload,
                        Err(e) => {
                            tracing::warn!("remote event 직렬화 실패: {e}");
                            continue;
                        }
                    };
                    if write_frame(&mut stream, &payload).is_err() {
                        // 클라이언트가 떠났거나 프레임 상한 초과(로컬 에러) —
                        // 어느 쪽이든 이 접속은 더 못 쓴다. 소켓을 닫아 reader도
                        // 깨워 접속 전체를 정리한다 (codex 리뷰: pump만 죽고
                        // 클라이언트가 이벤트 없이 붙어있는 반쪽 상태 방지)
                        let _ = stream.shutdown(Shutdown::Both);
                        return;
                    }
                    sent_event = true;
                }
                if sent_event {
                    last_activity = std::time::Instant::now();
                } else if last_activity.elapsed() >= HEARTBEAT_INTERVAL {
                    // 길이 0 프레임 = heartbeat. postcard로 직렬화된 RuntimeEvent는
                    // 항상 길이 > 0이라 정상 이벤트 프레임과 명확히 구분된다.
                    // write 실패는 죽은(half-open) peer — 접속을 정리한다.
                    if write_frame(&mut stream, &[]).is_err() {
                        let _ = stream.shutdown(Shutdown::Both);
                        return;
                    }
                    last_activity = std::time::Instant::now();
                }
                std::thread::sleep(PUMP_INTERVAL);
            }
        });
    let Ok(pump) = pump else {
        return;
    };

    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            // pump가 orphan되지 않게 소켓을 닫고 join까지 마친다 (codex 리뷰)
            tracing::warn!("remote reader clone 실패: {e}");
            conn_done.store(true, Ordering::SeqCst);
            let _ = stream.shutdown(Shutdown::Both);
            let _ = pump.join();
            return;
        }
    });
    // 상한 초과/EOF/기형 프레임은 None — 접속 종료 (valid frame만 허용).
    while let Some(frame) = read_frame(&mut reader) {
        // 접속 코덱으로 디코딩. Plain은 postcard(RuntimeCommand)와 바이트 동일.
        match codec.decode_command(&frame) {
            Ok(DecodedCommand::Command(command)) => {
                if let Err(reason) = validate_command(&command) {
                    tracing::warn!("remote 명령 검증 실패({reason}), 접속 종료");
                    break;
                }
                if backend.send_command(command).is_err() {
                    break; // worker 종료됨
                }
            }
            Ok(DecodedCommand::RequestKeyframe(session)) => {
                // pump가 다음 tick에 baseline을 버리고 keyframe을 보내게 한다 (§4.4-5).
                keyframe_requests
                    .lock()
                    .expect("keyframe req lock")
                    .push(session);
            }
            Err(e) => {
                // 프로토콜 위반 — 이 접속을 신뢰하지 않는다
                tracing::warn!("remote 명령 파싱 실패, 접속 종료: {e}");
                break;
            }
        }
    }
    // reader 종료 → pump도 정리 (플래그 + 소켓 닫기 — idle이어도 다음 tick에 끝난다)
    conn_done.store(true, Ordering::SeqCst);
    let _ = stream.shutdown(Shutdown::Both);
    let _ = pump.join();
}

/// pump가 한 이벤트를 접속 코덱으로 프레임 payload로 만든다.
/// Delta 접속의 viewport만 keyframe/delta 특수 처리(last_sent 갱신)하고, 나머지는
/// 기존 [`Codec::encode_event`] 경로 그대로 — Plain은 이 경로에서도 바이트 동일.
///
/// `SessionExited`는 Delta 접속에서 그 세션의 baseline을 [`HashMap::remove`]하고
/// `exited_this_batch`에 등록한다 — 장기 연결에서 종료된 세션의 snapshot Arc가 last_sent에
/// 누적되지 않게 (codex P2). 이미 종료된 세션의 trailing Viewport(같은 drain 배치)는
/// keyframe/delta가 아니라 plain full(`WireMsg::Event`)로 보내 baseline을 되살리지 않는다 —
/// 최종 출력은 여전히 클라 slot에 전달된다.
fn encode_pump_frame(
    codec: Codec,
    last_sent: &mut HashMap<SessionId, (u64, Arc<TerminalViewportSnapshot>)>,
    exited_this_batch: &mut HashSet<SessionId>,
    event: &RuntimeEvent,
) -> anyhow::Result<Vec<u8>> {
    match (codec, event) {
        (
            Codec::Delta,
            RuntimeEvent::Viewport {
                session,
                snapshot,
                bracketed_paste,
            },
        ) => {
            if exited_this_batch.contains(session) {
                // 이 배치에서 이미 종료된 세션 — baseline을 만들지 않고 전체 스냅샷을 그대로.
                codec.encode_event(event)
            } else {
                encode_viewport_frame(last_sent, *session, snapshot, *bracketed_paste)
            }
        }
        (Codec::Delta, RuntimeEvent::SessionExited { session, .. }) => {
            exited_this_batch.insert(*session);
            last_sent.remove(session);
            codec.encode_event(event)
        }
        _ => codec.encode_event(event),
    }
}

/// Delta 접속에서 세션 viewport를 last_sent 대비 keyframe/delta로 인코딩하고 baseline을
/// 갱신한다 (§4.4/§4.7). keyframe 조건: baseline 없음(신규/재구독/RequestKeyframe로 제거됨),
/// 또는 diff가 폴백(차원·alt-screen 변경/heavy repaint)을 반환. seq는 (접속,세션)마다 단조 증가.
fn encode_viewport_frame(
    last_sent: &mut HashMap<SessionId, (u64, Arc<TerminalViewportSnapshot>)>,
    session: SessionId,
    snapshot: &Arc<TerminalViewportSnapshot>,
    bracketed_paste: bool,
) -> anyhow::Result<Vec<u8>> {
    let (wire, new_seq) = match last_sent.get(&session) {
        Some((prev_seq, prev_snap)) => {
            let seq = prev_seq + 1;
            match diff_viewport(prev_snap, snapshot) {
                Some(delta) => (
                    WireMsg::ViewportDelta {
                        session,
                        seq,
                        base_seq: *prev_seq,
                        delta,
                        bracketed_paste,
                    },
                    seq,
                ),
                // 차원/alt-screen 변경 또는 heavy repaint → keyframe 폴백.
                None => (
                    WireMsg::ViewportKeyframe {
                        session,
                        seq,
                        snapshot: Arc::clone(snapshot),
                        bracketed_paste,
                    },
                    seq,
                ),
            }
        }
        // baseline 없음 → keyframe(seq 0에서 시작).
        None => (
            WireMsg::ViewportKeyframe {
                session,
                seq: 0,
                snapshot: Arc::clone(snapshot),
                bracketed_paste,
            },
            0,
        ),
    };
    let payload = encode_wire_msg(&wire)?;
    last_sent.insert(session, (new_seq, Arc::clone(snapshot)));
    Ok(payload)
}

/// loopback의 RemoteRuntimeServer에 attach하는 클라이언트.
/// InProcessRuntimeClient와 같은 trait(RuntimeCommandSink/RuntimeEventStream)을
/// 구현한다 — UI 입장에서 교체 가능 (완료 기준).
pub struct RemoteRuntimeClient {
    /// 명령 송신 채널. reader 스레드도 seq gap 시 RequestKeyframe를 이 뮤텍스로 보내므로
    /// (send_command와 프레임이 뒤섞이지 않게 같은 락 공유) Arc다 — 2-스레드 구조는 그대로.
    writer: Arc<Mutex<TcpStream>>,
    subscribers: Arc<Mutex<Vec<RemoteSubscriber>>>,
    reader_thread: Option<JoinHandle<()>>,
    /// 핸드셰이크에서 협상된 접속 코덱 (§3.2). loopback은 Delta.
    codec: Codec,
}

struct RemoteSubscriber {
    events: std::sync::mpsc::Sender<RuntimeEvent>,
    viewports: Arc<Mutex<std::collections::HashMap<SessionId, RuntimeEvent>>>,
}

impl RemoteRuntimeClient {
    /// loopback 주소에만 attach한다 (완료 기준: localhost-only attach).
    /// `token`은 서버의 [`RemoteRuntimeServer::auth_token`] — 첫 프레임으로 제시한다.
    pub fn attach(addr: SocketAddr, token: &str) -> anyhow::Result<Self> {
        if !addr.ip().is_loopback() {
            bail!("remote attach는 localhost만 허용합니다 (public remote는 v1+): {addr}");
        }
        let mut stream =
            TcpStream::connect(addr).with_context(|| format!("remote 서버 연결 실패: {addr}"))?;
        // 핸드셰이크 v2 (§3.1): ClientHello 송신 → ServerHello 대기. 토큰은 hello 안에 실린다.
        let hello = ClientHello {
            magic: PROTO_MAGIC,
            proto_version: PROTO_VERSION,
            features: CLIENT_FEATURES,
            token: token.as_bytes().to_vec(),
        };
        let hello_payload =
            postcard::to_allocvec(&hello).context("remote ClientHello 직렬화 실패")?;
        write_frame(&mut stream, &hello_payload).context("remote 핸드셰이크 전송 실패")?;
        // ServerHello를 기다린다 — 없으면 잘못된 토큰/거부 (attach가 Ok를 반환하고
        // 나서야 끊긴 것을 아는 반쪽 상태 방지. codex 리뷰)
        stream
            .set_read_timeout(Some(AUTH_TIMEOUT))
            .context("remote 핸드셰이크 대기 설정 실패")?;
        let server_hello = read_frame(&mut stream)
            .and_then(|frame| postcard::from_bytes::<ServerHello>(&frame).ok())
            .filter(|hello| hello.proto_version == PROTO_VERSION);
        let Some(server_hello) = server_hello else {
            bail!("remote 인증 거부 — 토큰을 확인하세요");
        };
        stream
            .set_read_timeout(None)
            .context("remote 핸드셰이크 대기 해제 실패")?;
        // 협상 불변식: agreed ⊆ 클라이언트 요청(CLIENT_FEATURES). 서버가 요청하지 않은
        // bit를 ack하면 rogue/버그 서버다 — 이를 거부해 서버가 클라이언트에 미협상 코덱을
        // 강제하지 못하게 한다 (codex 검수 P2). masking으로 무시만 하면, 서버가 그 기능으로
        // 인코딩하는데 클라가 Plain으로 읽는 silent codec mismatch가 남으므로 거부가 더 방어적.
        if server_hello.features & !CLIENT_FEATURES != 0 {
            bail!("remote 서버가 미요청 feature를 ack — 협상 불변식 위반, 접속 거부");
        }
        // 위 검사로 features ⊆ CLIENT_FEATURES 보장됨 → 코덱 확정 (§3.2). loopback은 Delta.
        let codec = Codec::from_features(server_hello.features);
        let subscribers: Arc<Mutex<Vec<RemoteSubscriber>>> = Arc::default();
        let writer = Arc::new(Mutex::new(stream));

        let reader_subscribers = Arc::clone(&subscribers);
        let reader_writer = Arc::clone(&writer);
        let reader_stream = writer
            .lock()
            .expect("remote writer lock")
            .try_clone()
            .context("remote stream clone 실패")?;
        let reader_thread = std::thread::Builder::new()
            .name("remote-events".into())
            .spawn(move || {
                let mut reader = BufReader::new(reader_stream);
                // 접속별 재구성 상태 (§4.3): 세션마다 (마지막 적용 seq, 현재 재구성본).
                // reader가 TCP를 UI 소비와 무관하게 완전히 드레인하므로 delta는 여기서 유실되지 않고,
                // slot에는 항상 "재구성된 전체 스냅샷"만 담긴다 — UI 계약(전체 Viewport)은 불변.
                let mut recon: HashMap<SessionId, (u64, Arc<TerminalViewportSnapshot>)> =
                    HashMap::new();
                // seq gap으로 keyframe을 이미 요청한 세션 — keyframe 도착 전까지 delta를 조용히 버려
                // RequestKeyframe 폭주를 막는다.
                let mut pending_keyframe: HashSet<SessionId> = HashSet::new();
                while let Some(frame) = read_frame(&mut reader) {
                    if frame.is_empty() {
                        continue; // heartbeat(길이 0 프레임) — decode 전에 소비
                    }
                    // 접속 코덱으로 디코딩. Plain은 postcard(RuntimeEvent)와 바이트 동일.
                    let Ok(decoded) = codec.decode_event(&frame) else {
                        tracing::warn!("remote 이벤트 프로토콜 위반 — 접속 종료");
                        break;
                    };
                    if !handle_decoded_event(
                        decoded,
                        &reader_subscribers,
                        &reader_writer,
                        &mut recon,
                        &mut pending_keyframe,
                    ) {
                        break;
                    }
                }
                // 수신이 죽은 클라이언트가 명령 전송만 성공하는 반쪽 상태 방지 —
                // 소켓을 양방향으로 닫아 이후 send_command도 실패하게 한다 (codex 리뷰)
                let _ = reader.into_inner().shutdown(Shutdown::Both);
            })
            .context("remote reader thread 생성 실패")?;

        Ok(Self {
            writer,
            subscribers,
            reader_thread: Some(reader_thread),
            codec,
        })
    }
}

/// reader가 디코드한 이벤트 하나를 처리한다 (§4.3/§4.4). 재구성 후 항상 전체 Viewport를
/// slot에 dispatch — UI는 변함없이 전체 스냅샷만 본다. `false`를 반환하면 접속을 끊는다
/// (검증 실패/프로토콜 위반). keyframe/delta 재구성 상태(recon)와 재동기화 요청은
/// 접속 단위(reader 스레드 소유)다.
fn handle_decoded_event(
    decoded: DecodedEvent,
    subscribers: &Arc<Mutex<Vec<RemoteSubscriber>>>,
    writer: &Mutex<TcpStream>,
    recon: &mut HashMap<SessionId, (u64, Arc<TerminalViewportSnapshot>)>,
    pending_keyframe: &mut HashSet<SessionId>,
) -> bool {
    match decoded {
        DecodedEvent::Event(event) => {
            if let Err(reason) = validate_event(&event) {
                tracing::warn!("remote 이벤트 검증 실패({reason}) — 접속 종료");
                return false;
            }
            // 종료된 세션의 재구성 상태를 정리한다 — 장기 연결에서 baseline Arc/pending이
            // 누적되지 않게 (codex P2). Viewport slot은 receiver.drain()이 프레임마다 비운다.
            if let RuntimeEvent::SessionExited { session, .. } = &event {
                recon.remove(session);
                pending_keyframe.remove(session);
            }
            dispatch(subscribers, event);
        }
        DecodedEvent::Keyframe {
            session,
            seq,
            snapshot,
            bracketed_paste,
        } => {
            // 전체 기준선 — recon을 세팅하고 그대로 전체 Viewport로 emit.
            let event = RuntimeEvent::Viewport {
                session,
                snapshot: Arc::clone(&snapshot),
                bracketed_paste,
            };
            if let Err(reason) = validate_event(&event) {
                tracing::warn!("remote keyframe 검증 실패({reason}) — 접속 종료");
                return false;
            }
            recon.insert(session, (seq, snapshot));
            pending_keyframe.remove(&session);
            dispatch(subscribers, event);
        }
        DecodedEvent::Delta {
            session,
            seq,
            base_seq,
            delta,
            bracketed_paste,
        } => {
            // base_seq가 현재 재구성 seq와 일치할 때만 적용을 시도한다. try_apply_delta는
            // 적용 전에 차원/row-index/cells-len을 검증하므로(codex P1) 기형 delta도
            // 패닉 없이 Err를 돌려준다. get으로 owned 결과만 뽑아 recon 재빌림 충돌을 피한다.
            let applied = match recon.get(&session) {
                Some((cur_seq, snap)) if *cur_seq == base_seq => {
                    Some(try_apply_delta(snap, &delta))
                }
                _ => None, // seq gap(신뢰 TCP라 이론상 없지만 방어, §4.4)
            };
            match applied {
                // 정상 적용 → 재구성본을 전체 Viewport로 emit.
                Some(Ok(reconstructed)) => {
                    let reconstructed = Arc::new(reconstructed);
                    let event = RuntimeEvent::Viewport {
                        session,
                        snapshot: Arc::clone(&reconstructed),
                        bracketed_paste,
                    };
                    if let Err(reason) = validate_event(&event) {
                        tracing::warn!("remote delta 재구성 검증 실패({reason}) — 접속 종료");
                        return false;
                    }
                    recon.insert(session, (seq, reconstructed));
                    dispatch(subscribers, event);
                }
                // 기형 delta 또는 seq gap → 패닉/접속종료 대신 gap 복구 경로 재사용:
                // baseline을 버리고 keyframe 요청, 이후 delta는 keyframe 도착까지 drop.
                // 요청은 세션당 1회만(폭주 방지).
                other => {
                    if let Some(Err(reason)) = other {
                        tracing::warn!("remote 기형 delta({reason}) — keyframe 재동기화");
                    }
                    recon.remove(&session);
                    if pending_keyframe.insert(session) {
                        request_keyframe(writer, session);
                    }
                }
            }
        }
    }
    true
}

/// seq gap 재동기화(§4.4): reader 스레드가 RequestKeyframe 제어 프레임을 명령 채널로 보낸다.
/// send_command와 같은 writer 뮤텍스를 잠가 프레임이 뒤섞이지 않는다. write 실패는 무시 —
/// 접속이 죽는 중이면 reader가 곧 EOF로 정리한다.
fn request_keyframe(writer: &Mutex<TcpStream>, session: SessionId) {
    let payload = match encode_request_keyframe(session) {
        Ok(payload) => payload,
        Err(e) => {
            tracing::warn!("remote RequestKeyframe 직렬화 실패: {e}");
            return;
        }
    };
    if let Ok(mut stream) = writer.lock() {
        let _ = write_frame(&mut *stream, &payload);
    }
}

/// InProcess worker의 emit과 같은 분배·정리 규칙: Viewport는 세션별 최신본 slot,
/// 상태 이벤트는 채널. receiver가 drop된 구독자는 제거한다 — slot 경로는
/// Arc strong_count(receiver도 slot Arc를 쥔다), 채널 경로는 send 실패로 판별.
fn dispatch(subscribers: &Arc<Mutex<Vec<RemoteSubscriber>>>, event: RuntimeEvent) {
    subscribers
        .lock()
        .expect("remote subscribers lock")
        .retain(|subscriber| {
            if let RuntimeEvent::Viewport { session, .. } = &event {
                if Arc::strong_count(&subscriber.viewports) <= 1 {
                    return false;
                }
                subscriber
                    .viewports
                    .lock()
                    .expect("remote viewport slot lock")
                    .insert(*session, event.clone());
                true
            } else {
                subscriber.events.send(event.clone()).is_ok()
            }
        });
}

impl RuntimeCommandSink for RemoteRuntimeClient {
    fn send_command(&self, command: RuntimeCommand) -> anyhow::Result<()> {
        // 접속 코덱으로 인코딩. 단계 A(Plain)는 postcard(RuntimeCommand)와 바이트 동일.
        let payload = self.codec.encode_command(&command)?;
        let mut stream = self.writer.lock().expect("remote writer lock");
        write_frame(&mut *stream, &payload).context("remote 명령 전송 실패")
    }
}

impl RuntimeEventStream for RemoteRuntimeClient {
    fn subscribe(&self) -> RuntimeEventReceiver {
        let (tx, rx) = channel();
        let viewports: Arc<Mutex<std::collections::HashMap<SessionId, RuntimeEvent>>> =
            Arc::default();
        self.subscribers
            .lock()
            .expect("remote subscribers lock")
            .push(RemoteSubscriber {
                events: tx,
                viewports: Arc::clone(&viewports),
            });
        RuntimeEventReceiver {
            events: rx,
            viewports,
        }
    }
}

impl crate::client::RuntimeClient for RemoteRuntimeClient {}

impl Drop for RemoteRuntimeClient {
    fn drop(&mut self) {
        if let Ok(stream) = self.writer.lock() {
            let _ = stream.shutdown(Shutdown::Both);
        }
        if let Some(handle) = self.reader_thread.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::protocol::FEAT_DELTA_VIEWPORT;
    use std::time::{Duration, Instant};

    /// v2 핸드셰이크를 수동으로 수행한다 (raw 소켓 테스트용):
    /// ClientHello 송신 → ServerHello 디코드. 서버가 거부(소켓 종료)하면 None.
    fn v2_handshake(raw: &mut TcpStream, token: &[u8], features: u32) -> Option<ServerHello> {
        let hello = ClientHello {
            magic: PROTO_MAGIC,
            proto_version: PROTO_VERSION,
            features,
            token: token.to_vec(),
        };
        let payload = postcard::to_allocvec(&hello).unwrap();
        write_frame(raw, &payload).unwrap();
        raw.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let frame = read_frame(raw)?;
        postcard::from_bytes::<ServerHello>(&frame).ok()
    }

    fn init_mock_store() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            keyring_core::set_default_store(keyring_core::mock::Store::new().unwrap());
        });
    }

    fn test_backend(name: &str) -> InProcessRuntimeClient {
        init_mock_store();
        let logs = std::env::temp_dir().join(format!("deppy-remote-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&logs).unwrap();
        InProcessRuntimeClient::with_shell(
            5,
            Arc::new(secret::KeyringSecretStore),
            logs,
            secret::RedactionService::new(),
            pty::CommandSpec {
                program: "/bin/sh".into(),
                args: vec!["-c".into(), "echo remote-ok; sleep 5".into()],
                env: Vec::new(),
            },
            None,
        )
    }

    /// 조건이 참이 될 때까지 이벤트를 모으며 기다린다.
    fn wait_for(
        rx: &RuntimeEventReceiver,
        seen: &mut Vec<RuntimeEvent>,
        timeout: Duration,
        pred: impl Fn(&[RuntimeEvent]) -> bool,
    ) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            seen.extend(rx.drain());
            if pred(seen) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("이벤트 대기 시간 초과 — 수신: {}개", seen.len());
    }

    #[test]
    fn 서버는_loopback에만_bind() {
        let server = RemoteRuntimeServer::serve(test_backend("bind"), 0).unwrap();
        assert!(server.local_addr().ip().is_loopback());
        server.shutdown();
    }

    #[test]
    fn 비loopback_attach는_거부() {
        let Err(e) = RemoteRuntimeClient::attach("8.8.8.8:1".parse().unwrap(), "t") else {
            panic!("비loopback attach가 성공하면 안 된다");
        };
        assert!(format!("{e:#}").contains("localhost"));
    }

    /// 완료 기준: InProcess와 같은 명령/이벤트 모델로 localhost attach.
    /// SpawnShell 명령이 TCP를 건너 worker에 닿고, ShellSpawned/MuxUpdated/
    /// Viewport 이벤트가 되돌아온다.
    #[test]
    fn attach_명령_이벤트_왕복() {
        let server = RemoteRuntimeServer::serve(test_backend("roundtrip"), 0).unwrap();
        let client = RemoteRuntimeClient::attach(server.local_addr(), server.auth_token()).unwrap();
        let rx = client.subscribe();

        client
            .send_command(RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
            })
            .unwrap();

        let mut seen = Vec::new();
        wait_for(&rx, &mut seen, Duration::from_secs(10), |events| {
            let spawned = events
                .iter()
                .any(|e| matches!(e, RuntimeEvent::ShellSpawned { .. }));
            let mux = events
                .iter()
                .any(|e| matches!(e, RuntimeEvent::MuxUpdated { .. }));
            let viewport = events.iter().any(|e| {
                matches!(e, RuntimeEvent::Viewport { snapshot, .. }
                    if snapshot.visible_cells.iter().map(|c| c.c).collect::<String>().contains("remote-ok"))
            });
            spawned && mux && viewport
        });

        // 이벤트 순서 계약: MuxUpdated가 ShellSpawned보다 먼저 (emit 순서 유지 확인)
        let mux_pos = seen
            .iter()
            .position(|e| matches!(e, RuntimeEvent::MuxUpdated { .. }))
            .unwrap();
        let spawned_pos = seen
            .iter()
            .position(|e| matches!(e, RuntimeEvent::ShellSpawned { .. }))
            .unwrap();
        assert!(mux_pos < spawned_pos);

        drop(client);
        server.shutdown();
    }

    /// §1.5 auth required: 매직/버전은 맞지만 토큰만 틀린 ClientHello는 ServerHello 없이 끊긴다.
    #[test]
    fn 잘못된_토큰은_거부() {
        let server = RemoteRuntimeServer::serve(test_backend("badtoken"), 0).unwrap();
        let mut raw = TcpStream::connect(server.local_addr()).unwrap();
        assert!(
            v2_handshake(&mut raw, b"wrong-token", CLIENT_FEATURES).is_none(),
            "잘못된 토큰은 ServerHello를 받지 못하고 끊겨야 한다"
        );
        server.shutdown();
    }

    /// v2 핸드셰이크 성공 + 기능 협상 교집합. 서버 지원 집합(SERVER_FEATURES=FEAT_DELTA_VIEWPORT)
    /// 과의 교집합이므로, 클라이언트가 아무것도 요청하지 않으면 빈 집합(Plain), delta를 요청하면
    /// delta가 협상된다(Delta).
    #[test]
    fn v2_핸드셰이크_성공_및_기능_협상() {
        let server = RemoteRuntimeServer::serve(test_backend("negotiate"), 0).unwrap();

        // 요청 없음(features=0) → 빈 교집합 (Plain)
        let mut raw = TcpStream::connect(server.local_addr()).unwrap();
        let hello =
            v2_handshake(&mut raw, server.auth_token().as_bytes(), 0).expect("ServerHello 없음");
        assert_eq!(hello.proto_version, PROTO_VERSION);
        assert_eq!(hello.features, 0, "요청 없으면 협상 결과는 비어야 한다");

        // delta 요청 → 서버도 지원하므로 delta 협상됨 (Delta)
        let mut raw2 = TcpStream::connect(server.local_addr()).unwrap();
        let hello2 = v2_handshake(
            &mut raw2,
            server.auth_token().as_bytes(),
            FEAT_DELTA_VIEWPORT,
        )
        .expect("ServerHello 없음");
        assert_eq!(
            hello2.features, FEAT_DELTA_VIEWPORT,
            "양측이 지원하는 delta는 협상된다"
        );

        server.shutdown();
    }

    /// 단계 A off-path 불변 (§3.2, §8 #1): delta 미협상(Plain 코덱)에서 이벤트/명령
    /// 프레임은 봉투 없는 기존 `postcard(RuntimeCommand)`/`postcard(RuntimeEvent)`와
    /// **바이트 동일**해야 한다. Plain 코덱을 A 이전 경로와 직접 대조해 못 박는다.
    #[test]
    fn plain_코덱은_기존_postcard와_바이트_동일() {
        let command = RuntimeCommand::SpawnAgent {
            cols: 80,
            rows: 24,
            scrollback_lines: 100,
            agent_config_id: Some("cfg-1".into()),
            command: "claude".into(),
            args: vec!["--foo".into()],
            env_plain: vec![("K".into(), "V".into())],
            env_secrets: vec![("S".into(), "cred-1".into())],
            waiting_regex: None,
            approval_regex: None,
            error_regex: None,
            done_regex: None,
        };
        assert_eq!(
            Codec::Plain.encode_command(&command).unwrap(),
            postcard::to_allocvec(&command).unwrap(),
            "Plain 명령 프레임이 기존 postcard와 바이트 동일해야 한다"
        );

        let event = RuntimeEvent::ShellSpawned {
            session: SessionId(7),
        };
        assert_eq!(
            Codec::Plain.encode_event(&event).unwrap(),
            postcard::to_allocvec(&event).unwrap(),
            "Plain 이벤트 프레임이 기존 postcard와 바이트 동일해야 한다"
        );

        // 디코드도 기존 경로가 만든 프레임을 그대로 받아들인다(왕복).
        let cmd_frame = postcard::to_allocvec(&command).unwrap();
        assert!(matches!(
            Codec::Plain.decode_command(&cmd_frame).unwrap(),
            DecodedCommand::Command(RuntimeCommand::SpawnAgent { cols: 80, .. })
        ));
    }

    /// rogue/버그 서버가 클라이언트가 요청하지 않은 feature를 ack하면 클라이언트는
    /// 협상 불변식(agreed ⊆ 요청) 위반으로 접속을 거부한다 — 서버가 클라이언트에
    /// 미협상 코덱을 강제하지 못하게 (codex 검수 P2). off-path 불변의 클라이언트측 방어.
    #[test]
    fn 서버가_미요청_feature_ack하면_거부() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let rogue = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            // ClientHello 프레임 소비(내용 무시).
            let mut reader = BufReader::new(sock.try_clone().unwrap());
            let _ = read_frame(&mut reader);
            // 클라이언트가 요청하지 않은 미래 feature 비트(CLIENT_FEATURES 밖)를 ack.
            let hello = ServerHello {
                proto_version: PROTO_VERSION,
                features: 1 << 31,
            };
            let payload = postcard::to_allocvec(&hello).unwrap();
            let _ = write_frame(&mut sock, &payload);
            // 클라이언트가 거부하고 끊을 때까지 잠깐 유지.
            std::thread::sleep(Duration::from_millis(200));
        });

        let Err(err) = RemoteRuntimeClient::attach(addr, "tok") else {
            panic!("미요청 feature ack은 거부돼야 한다");
        };
        assert!(
            format!("{err:#}").contains("협상"),
            "협상 불변식 위반 메시지여야 한다: {err:#}"
        );
        rogue.join().unwrap();
    }

    /// 구버전 v1 원시 토큰 프레임(=ClientHello 아님)은 hang 없이 조기 거부된다.
    /// 매직/버전 prefix가 없어 postcard 디코드/매직 검증이 실패 → 즉시 종료(침묵 timeout 대기 아님).
    #[test]
    fn 구버전_첫_프레임은_hang없이_거부() {
        let server = RemoteRuntimeServer::serve(test_backend("oldframe"), 0).unwrap();
        let mut raw = TcpStream::connect(server.local_addr()).unwrap();
        // v1 스타일: 매직 없는 원시 토큰 바이트 (hex라 매직 "DPRT"와 절대 일치 불가).
        write_frame(&mut raw, server.auth_token().as_bytes()).unwrap();
        raw.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let start = Instant::now();
        // 서버가 ServerHello 없이 끊는다 — read가 EOF(0)/에러로 끝난다.
        let mut buf = [0u8; 16];
        loop {
            match std::io::Read::read(&mut raw, &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            assert!(start.elapsed() < Duration::from_secs(10), "거부가 hang됨");
        }
        // 조기 거부 확인 — 침묵 timeout(AUTH_TIMEOUT)까지 기다리지 않고 즉시 끊겨야 한다.
        assert!(
            start.elapsed() < AUTH_TIMEOUT,
            "구버전 프레임은 즉시 거부돼야 한다(침묵 timeout 대기 아님)"
        );
        server.shutdown();
    }

    /// 토큰 프레임 없이 침묵하는 peer는 timeout으로 정리된다 (서버 hang 없음).
    #[test]
    fn 무토큰_접속은_붙잡아두지_못한다() {
        let server = RemoteRuntimeServer::serve(test_backend("silent"), 0).unwrap();
        let _silent = TcpStream::connect(server.local_addr()).unwrap();
        // AUTH_TIMEOUT(5s) 뒤에는 다음 클라이언트가 정상 attach 가능해야 한다
        let start = Instant::now();
        let client = loop {
            match RemoteRuntimeClient::attach(server.local_addr(), server.auth_token()) {
                Ok(client) => break client,
                Err(_) => {
                    assert!(
                        start.elapsed() < Duration::from_secs(15),
                        "침묵 peer가 서버를 계속 점유"
                    );
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        };
        // attach 자체는 성공 — 명령이 실제로 통하는지까지 확인
        client
            .send_command(RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
            })
            .unwrap();
        let rx = client.subscribe();
        let mut seen = Vec::new();
        wait_for(&rx, &mut seen, Duration::from_secs(15), |events| {
            events
                .iter()
                .any(|e| matches!(e, RuntimeEvent::ShellSpawned { .. }))
        });
        drop(client);
        server.shutdown();
    }

    /// 유휴 클라이언트가 붙어 있어도 shutdown은 블록되지 않는다 (codex 리뷰 회귀).
    #[test]
    fn 접속_유지_중_shutdown() {
        let server = RemoteRuntimeServer::serve(test_backend("idle-shutdown"), 0).unwrap();
        let client = RemoteRuntimeClient::attach(server.local_addr(), server.auth_token()).unwrap();
        // 접속이 accept돼 reader가 붙을 때까지 잠깐 대기
        std::thread::sleep(Duration::from_millis(200));
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&done);
        let handle = std::thread::spawn(move || {
            server.shutdown();
            flag.store(true, Ordering::SeqCst);
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "shutdown이 유휴 접속에 블록됨");
            std::thread::sleep(Duration::from_millis(20));
        }
        handle.join().unwrap();
        drop(client);
    }

    /// 동시 접속: 두 클라이언트가 같은 runtime에 함께 attach해 있고, 한쪽이
    /// 보낸 명령의 결과 이벤트를 양쪽 다 받는다 — 이전 스켈레톤은 accept 루프가
    /// serve_connection을 인라인 호출해 한 번에 한 클라이언트만 처리했고,
    /// 두 번째 attach는 첫 접속이 끝날 때까지 인증 ACK조차 못 받았다.
    #[test]
    fn 동시_두_클라이언트_모두_이벤트_수신() {
        let server = RemoteRuntimeServer::serve(test_backend("dual"), 0).unwrap();
        let client_a = RemoteRuntimeClient::attach(server.local_addr(), server.auth_token())
            .expect("client_a attach");
        let client_b = RemoteRuntimeClient::attach(server.local_addr(), server.auth_token())
            .expect("client_b attach — 동시 다중 접속이 가능해야 한다");
        let rx_a = client_a.subscribe();
        let rx_b = client_b.subscribe();

        client_a
            .send_command(RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
            })
            .unwrap();

        let assert_spawned_and_mux = |rx: &RuntimeEventReceiver| {
            let mut seen = Vec::new();
            wait_for(rx, &mut seen, Duration::from_secs(10), |events| {
                events
                    .iter()
                    .any(|e| matches!(e, RuntimeEvent::ShellSpawned { .. }))
                    && events
                        .iter()
                        .any(|e| matches!(e, RuntimeEvent::MuxUpdated { .. }))
            });
        };
        assert_spawned_and_mux(&rx_a);
        assert_spawned_and_mux(&rx_b);

        drop(client_a);
        drop(client_b);
        server.shutdown();
    }

    /// 유휴 클라이언트 두 개가 동시에 붙어 있어도 shutdown이 블록되지 않는다
    /// (다중 접속으로 일반화된 접속_유지_중_shutdown 회귀 테스트).
    #[test]
    fn 다중_접속_유지_중_shutdown() {
        let server = RemoteRuntimeServer::serve(test_backend("multi-idle-shutdown"), 0).unwrap();
        let client_a =
            RemoteRuntimeClient::attach(server.local_addr(), server.auth_token()).unwrap();
        let client_b =
            RemoteRuntimeClient::attach(server.local_addr(), server.auth_token()).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&done);
        let handle = std::thread::spawn(move || {
            server.shutdown();
            flag.store(true, Ordering::SeqCst);
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "shutdown이 다중 접속에 블록됨");
            std::thread::sleep(Duration::from_millis(20));
        }
        handle.join().unwrap();
        drop(client_a);
        drop(client_b);
    }

    /// heartbeat: 유휴 접속에 HEARTBEAT_INTERVAL마다 길이 0 프레임이 온다 —
    /// postcard RuntimeEvent는 항상 길이 > 0이라 정상 이벤트와 구분된다.
    #[test]
    fn 유휴_접속에_heartbeat_프레임() {
        let server = RemoteRuntimeServer::serve(test_backend("heartbeat"), 0).unwrap();
        let mut raw = TcpStream::connect(server.local_addr()).unwrap();
        let hello = v2_handshake(&mut raw, server.auth_token().as_bytes(), CLIENT_FEATURES)
            .expect("ServerHello 없음");
        assert_eq!(hello.proto_version, PROTO_VERSION);

        // HEARTBEAT_INTERVAL(15s) + 15s 여유
        raw.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        let frame = read_frame(&mut raw).expect("heartbeat 프레임을 받지 못함");
        assert!(frame.is_empty(), "heartbeat 프레임은 길이 0이어야 한다");

        server.shutdown();
    }

    /// 접속이 끊기면 서버가 접속 목록에서 자기 항목을 스스로 정리한다 —
    /// 다중 접속 추적(connections)이 좀비 스레드/소켓을 남기지 않는지 확인.
    /// (진짜 half-open — FIN/RST 없이 그냥 사라지는 네트워크 단절 — 은 같은
    /// 머신 안의 유닛 테스트로 재현할 수 없다: OS는 프로세스가 죽어도 커널이
    /// 소켓 fd를 정리하며 FIN을 보낸다. heartbeat이 그 상황에서 write 실패를
    /// 일으킨다는 것은 위 유휴_접속에_heartbeat_프레임 테스트로 — 프레임이
    /// 실제로 주기적으로 나간다는 것과, write_frame 실패 시 접속을 정리하는
    /// 코드가 이벤트 프레임과 동일 경로라는 것으로 — 구조적으로 검증된다.)
    #[test]
    fn 접속_종료_후_목록에서_정리된다() {
        let server = RemoteRuntimeServer::serve(test_backend("cleanup"), 0).unwrap();
        let mut raw = TcpStream::connect(server.local_addr()).unwrap();
        let _hello = v2_handshake(&mut raw, server.auth_token().as_bytes(), CLIENT_FEATURES)
            .expect("ServerHello 없음");

        // 접속이 목록에 등록될 때까지 대기
        let reg_deadline = Instant::now() + Duration::from_secs(5);
        while server
            .connections
            .lock()
            .expect("connections lock")
            .is_empty()
        {
            assert!(Instant::now() < reg_deadline, "접속이 목록에 등록되지 않음");
            std::thread::sleep(Duration::from_millis(20));
        }

        drop(raw);

        // 접속이 끊기면 곧바로(heartbeat 대기 없이) 목록에서 빠져야 한다
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if server
                .connections
                .lock()
                .expect("connections lock")
                .is_empty()
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "종료된 접속이 목록에서 정리되지 않음"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        server.shutdown();
    }

    #[test]
    fn 와이어_검증_규칙() {
        // 명령: cols/rows 0, scrollback 상한
        assert!(
            validate_command(&RuntimeCommand::SpawnShell {
                cols: 0,
                rows: 24,
                scrollback_lines: 100
            })
            .is_err()
        );
        assert!(
            validate_command(&RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines: MAX_SCROLLBACK_LINES + 1
            })
            .is_err()
        );
        assert!(
            validate_command(&RuntimeCommand::Resize {
                session: SessionId(1),
                cols: 80,
                rows: 0
            })
            .is_err()
        );
        // 이벤트: 셀 수 불일치, 비정상 ratio
        let bad_viewport = RuntimeEvent::Viewport {
            session: SessionId(1),
            snapshot: Arc::new(terminal::TerminalViewportSnapshot {
                cols: 2,
                rows: 2,
                cursor: terminal::CursorSnapshot {
                    col: 0,
                    row: 0,
                    shape: terminal::CursorShape::Block,
                    visible: true,
                },
                visible_cells: Vec::new().into(),
                dirty_ranges: Vec::new(),
                title: None,
                scroll_offset: 0,
                is_alt_screen: false,
            }),
            bracketed_paste: false,
        };
        assert!(validate_event(&bad_viewport).is_err());
        assert!(layout_ratios_valid(&crate::LayoutNode::Pane(
            deppy_core::MuxPaneId::new()
        )));
        assert!(!layout_ratios_valid(&crate::LayoutNode::Split {
            direction: crate::SplitDirection::Horizontal,
            ratio: f32::NAN,
            first: Box::new(crate::LayoutNode::Pane(deppy_core::MuxPaneId::new())),
            second: Box::new(crate::LayoutNode::Pane(deppy_core::MuxPaneId::new())),
        }));
    }

    /// 프로토콜 위반(기형 프레임)은 접속 종료로 이어진다.
    #[test]
    fn 잘못된_명령_라인은_접속_종료() {
        let server = RemoteRuntimeServer::serve(test_backend("protocol"), 0).unwrap();
        let mut raw = TcpStream::connect(server.local_addr()).unwrap();
        // 정상 핸드셰이크 후 postcard로 해석 불가한 명령 프레임 전송
        let _hello = v2_handshake(&mut raw, server.auth_token().as_bytes(), CLIENT_FEATURES)
            .expect("ServerHello 없음");
        write_frame(&mut raw, &[0xff; 64]).unwrap();
        // 서버가 접속을 닫으면 read가 EOF(0)로 끝난다
        raw.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut buf = [0u8; 256];
        let mut deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match std::io::Read::read(&mut raw, &mut buf) {
                Ok(0) => break, // EOF — 종료 확인
                Ok(_) => {}     // 종료 전에 pump가 보낸 이벤트일 수 있음 — 계속
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => break, // reset도 종료로 취급
            }
            assert!(Instant::now() < deadline, "접속 종료 대기 시간 초과");
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = &mut deadline;
        server.shutdown();
    }

    // ---- delta viewport 스트리밍 (§4) ----

    use crate::protocol::{RowPatch, ViewportDelta, try_apply_delta};
    use terminal::{CursorShape, CursorSnapshot, TerminalCell};

    fn cell(c: char) -> TerminalCell {
        TerminalCell {
            c,
            fg: [10, 20, 30],
            bg: [0, 0, 0],
            wide: false,
            wide_spacer: false,
        }
    }

    /// cols*rows 그리드 스냅샷을 만든다. `lines[r]`의 각 문자가 셀이 되고 나머지는 공백으로 채운다.
    fn make_snapshot(
        cols: u16,
        rows: u16,
        lines: &[&str],
        alt: bool,
    ) -> Arc<TerminalViewportSnapshot> {
        let mut cells = Vec::with_capacity(cols as usize * rows as usize);
        for r in 0..rows as usize {
            let chars: Vec<char> = lines.get(r).copied().unwrap_or("").chars().collect();
            for c in 0..cols as usize {
                cells.push(cell(chars.get(c).copied().unwrap_or(' ')));
            }
        }
        Arc::new(TerminalViewportSnapshot {
            cols,
            rows,
            cursor: CursorSnapshot {
                col: 0,
                row: 0,
                shape: CursorShape::Block,
                visible: true,
            },
            visible_cells: cells.into(),
            dirty_ranges: Vec::new(),
            title: None,
            scroll_offset: 0,
            is_alt_screen: alt,
        })
    }

    /// 네트워크 없이 서버 encode(last_sent) → 클라이언트 reconstruct(recon) 파이프라인을 돈다.
    /// round_trip은 매 tick의 재구성된 전체 스냅샷을 돌려준다 — source와 == 여야 한다(§4.8).
    struct DeltaPipe {
        last_sent: HashMap<SessionId, (u64, Arc<TerminalViewportSnapshot>)>,
        recon: HashMap<SessionId, (u64, Arc<TerminalViewportSnapshot>)>,
        keyframes: usize,
        deltas: usize,
        last_frame_len: usize,
    }

    impl DeltaPipe {
        fn new() -> Self {
            Self {
                last_sent: HashMap::new(),
                recon: HashMap::new(),
                keyframes: 0,
                deltas: 0,
                last_frame_len: 0,
            }
        }

        fn round_trip(
            &mut self,
            session: SessionId,
            snap: &Arc<TerminalViewportSnapshot>,
        ) -> TerminalViewportSnapshot {
            let frame = encode_viewport_frame(&mut self.last_sent, session, snap, false).unwrap();
            self.last_frame_len = frame.len();
            match Codec::Delta.decode_event(&frame).unwrap() {
                DecodedEvent::Keyframe {
                    session,
                    seq,
                    snapshot,
                    ..
                } => {
                    self.keyframes += 1;
                    self.recon.insert(session, (seq, Arc::clone(&snapshot)));
                    (*snapshot).clone()
                }
                DecodedEvent::Delta {
                    session,
                    seq,
                    base_seq,
                    delta,
                    ..
                } => {
                    self.deltas += 1;
                    let (cur_seq, prev) = self.recon.get(&session).expect("baseline 있어야 함");
                    assert_eq!(*cur_seq, base_seq, "base_seq가 recon seq와 일치해야 한다");
                    let new = try_apply_delta(prev, &delta).expect("정상 delta는 적용돼야 한다");
                    self.recon.insert(session, (seq, Arc::new(new.clone())));
                    new
                }
                DecodedEvent::Event(_) => panic!("viewport가 Event 봉투로 인코딩됨"),
            }
        }
    }

    /// §4.8 왕복 등가성: keyframe → 타이핑 delta들 → 재구성이 매 tick source와 셀 단위 동일.
    #[test]
    fn delta_왕복_등가성() {
        let s = SessionId(1);
        let mut pipe = DeltaPipe::new();

        let snaps = [
            make_snapshot(20, 5, &["hello", "world", "", "", ""], false),
            make_snapshot(20, 5, &["hello!", "world", "", "", ""], false), // row 0 타이핑
            make_snapshot(20, 5, &["hello!", "world", "line3", "", ""], false), // row 2 타이핑
            make_snapshot(20, 5, &["hello!", "world", "line3", "", ""], false), // 변화 없음(헤더만)
        ];
        for snap in &snaps {
            let recon = pipe.round_trip(s, snap);
            assert_eq!(&recon, snap.as_ref(), "재구성이 source와 동일해야 한다");
        }
        assert_eq!(pipe.keyframes, 1, "첫 프레임만 keyframe");
        assert_eq!(pipe.deltas, 3, "이후는 delta");
    }

    /// 타이핑 delta 프레임이 keyframe 프레임보다 훨씬 작다 (§4.6 대역폭 절감).
    #[test]
    fn 타이핑_delta가_keyframe보다_작다() {
        let s = SessionId(1);
        let mut pipe = DeltaPipe::new();
        let base = make_snapshot(80, 24, &["prompt$ "], false);
        pipe.round_trip(s, &base);
        let keyframe_len = pipe.last_frame_len;

        // 한 row에 한 글자 타이핑
        let typed = make_snapshot(80, 24, &["prompt$ a"], false);
        pipe.round_trip(s, &typed);
        let delta_len = pipe.last_frame_len;

        assert_eq!(pipe.deltas, 1, "타이핑은 delta여야 한다");
        assert!(
            delta_len * 5 < keyframe_len,
            "타이핑 delta({delta_len}B)는 keyframe({keyframe_len}B)의 1/5 미만이어야 한다"
        );
    }

    /// 리사이즈(차원 변경)는 keyframe 폴백 (§4.4-2), 재구성은 새 차원으로 정확.
    #[test]
    fn 리사이즈는_keyframe_폴백() {
        let s = SessionId(1);
        let mut pipe = DeltaPipe::new();
        let a = make_snapshot(20, 5, &["hi"], false);
        assert_eq!(&pipe.round_trip(s, &a), a.as_ref());
        let b = make_snapshot(30, 5, &["hi there"], false); // cols 변경
        assert_eq!(&pipe.round_trip(s, &b), b.as_ref());
        assert_eq!(pipe.keyframes, 2, "리사이즈는 keyframe");
        assert_eq!(pipe.deltas, 0);
    }

    /// alt-screen 토글은 keyframe 폴백 (§4.4-3).
    #[test]
    fn alt_screen_토글은_keyframe() {
        let s = SessionId(1);
        let mut pipe = DeltaPipe::new();
        let a = make_snapshot(20, 5, &["main"], false);
        pipe.round_trip(s, &a);
        let b = make_snapshot(20, 5, &["vim"], true); // is_alt_screen 변경
        assert_eq!(&pipe.round_trip(s, &b), b.as_ref());
        assert_eq!(pipe.keyframes, 2, "alt-screen 토글은 keyframe");
        assert_eq!(pipe.deltas, 0);
    }

    /// heavy repaint(전체의 60% 이상 변경)는 keyframe 폴백, 소량 변경은 delta (§4.4-4).
    #[test]
    fn heavy_repaint는_keyframe_폴백() {
        let s = SessionId(1);
        let mut pipe = DeltaPipe::new();
        let base = make_snapshot(10, 10, &[""; 10], false);
        pipe.round_trip(s, &base);

        // 2/10 = 20% 변경 → delta
        let light = make_snapshot(10, 10, &["a", "b"], false);
        assert_eq!(&pipe.round_trip(s, &light), light.as_ref());
        assert_eq!(pipe.deltas, 1, "20% 변경은 delta");

        // 8/10 = 80% 변경 → keyframe 폴백
        let heavy = make_snapshot(10, 10, &["1", "2", "3", "4", "5", "6", "7", "8"], false);
        assert_eq!(&pipe.round_trip(s, &heavy), heavy.as_ref());
        assert_eq!(pipe.keyframes, 2, "80% 변경은 keyframe 폴백");
    }

    /// gap 복구(§4.4): delta 유실로 base_seq가 앞서면 클라이언트가 keyframe을 요청하고,
    /// 서버는 baseline을 버려 다음 프레임을 keyframe으로 보내 재구성이 복구된다.
    #[test]
    fn delta_gap_복구() {
        let s = SessionId(1);
        let mut last_sent: HashMap<SessionId, (u64, Arc<TerminalViewportSnapshot>)> =
            HashMap::new();
        let mut recon: HashMap<SessionId, (u64, Arc<TerminalViewportSnapshot>)> = HashMap::new();

        // keyframe(seq 0) — 클라이언트 baseline 세팅
        let s0 = make_snapshot(20, 5, &["a"], false);
        let f0 = encode_viewport_frame(&mut last_sent, s, &s0, false).unwrap();
        let DecodedEvent::Keyframe { seq, snapshot, .. } = Codec::Delta.decode_event(&f0).unwrap()
        else {
            panic!("첫 프레임은 keyframe");
        };
        recon.insert(s, (seq, snapshot));

        // 서버가 delta(seq 1)를 보내지만 유실됐다고 가정 — 클라이언트는 못 받는다.
        let s1 = make_snapshot(20, 5, &["ab"], false);
        let _lost = encode_viewport_frame(&mut last_sent, s, &s1, false).unwrap();

        // 다음 delta(seq 2, base_seq 1)가 도착 — 클라 recon seq는 0이라 gap.
        let s2 = make_snapshot(20, 5, &["abc"], false);
        let f2 = encode_viewport_frame(&mut last_sent, s, &s2, false).unwrap();
        let DecodedEvent::Delta { base_seq, .. } = Codec::Delta.decode_event(&f2).unwrap() else {
            panic!("seq 2는 delta");
        };
        let (cur_seq, _) = recon.get(&s).unwrap();
        assert_ne!(*cur_seq, base_seq, "base_seq 불일치(gap) 감지");

        // 클라이언트가 keyframe 요청 → 서버가 baseline 제거(RequestKeyframe 처리와 동일).
        recon.remove(&s);
        last_sent.remove(&s);

        // 서버의 다음 viewport → baseline 없어 keyframe → 재구성 복구.
        let s3 = make_snapshot(20, 5, &["recovered"], false);
        let f3 = encode_viewport_frame(&mut last_sent, s, &s3, false).unwrap();
        let DecodedEvent::Keyframe { snapshot, .. } = Codec::Delta.decode_event(&f3).unwrap()
        else {
            panic!("복구 프레임은 keyframe");
        };
        assert_eq!(
            snapshot.as_ref(),
            s3.as_ref(),
            "keyframe 재구성이 source와 동일"
        );
    }

    /// 클라이언트 reader가 gap을 만나면 RequestKeyframe 제어 프레임을 실제 소켓으로 보낸다.
    /// handle_decoded_event의 gap 분기(§4.4)를 소켓 페어로 검증한다.
    #[test]
    fn gap시_request_keyframe_송신() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client_sock = TcpStream::connect(addr).unwrap();
        let (server_sock, _) = listener.accept().unwrap();
        let writer = Mutex::new(client_sock);
        let subscribers: Arc<Mutex<Vec<RemoteSubscriber>>> = Arc::default();

        let s = SessionId(7);
        // 클라이언트 recon seq를 5로 세팅.
        let base = make_snapshot(10, 3, &["x"], false);
        let mut recon: HashMap<SessionId, (u64, Arc<TerminalViewportSnapshot>)> = HashMap::new();
        recon.insert(s, (5, base.clone()));
        let mut pending: HashSet<SessionId> = HashSet::new();

        // base_seq 3 ≠ recon seq 5 → gap.
        let changed = make_snapshot(10, 3, &["y"], false);
        let delta = diff_viewport(&base, &changed).unwrap();
        let cont = handle_decoded_event(
            DecodedEvent::Delta {
                session: s,
                seq: 4,
                base_seq: 3,
                delta,
                bracketed_paste: false,
            },
            &subscribers,
            &writer,
            &mut recon,
            &mut pending,
        );
        assert!(cont, "gap은 접속을 끊지 않는다");
        assert!(!recon.contains_key(&s), "gap 시 baseline을 버린다");
        assert!(pending.contains(&s), "keyframe 요청 pending");

        // 서버 소켓에서 RequestKeyframe 프레임을 읽어 확인.
        let mut r = BufReader::new(server_sock);
        let frame = read_frame(&mut r).expect("RequestKeyframe 프레임");
        match Codec::Delta.decode_command(&frame).unwrap() {
            DecodedCommand::RequestKeyframe(got) => assert_eq!(got, s),
            _ => panic!("RequestKeyframe여야 한다"),
        }
    }

    /// 소켓 페어 (클라이언트, 서버) — reader 경로 테스트에서 writer로 쓴다.
    fn socket_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).unwrap();
        let (server, _) = listener.accept().unwrap();
        (client, server)
    }

    /// try_apply_delta는 기형 delta(차원 불일치 / row 범위 밖 / cells 수 불일치)를 패닉 없이
    /// Err로 거른다 (codex P1). 어떤 입력에도 인덱스 out-of-bounds가 없어야 한다.
    #[test]
    fn try_apply_delta_기형은_err() {
        let base = make_snapshot(4, 2, &["ab", "cd"], false);
        let ok_cursor = CursorSnapshot {
            col: 0,
            row: 0,
            shape: CursorShape::Block,
            visible: true,
        };
        let mk = |cols: u16, rows: u16, rows_patch: Vec<RowPatch>| ViewportDelta {
            cols,
            rows,
            cursor: ok_cursor,
            scroll_offset: 0,
            is_alt_screen: false,
            title: None,
            changed_rows: rows_patch,
        };

        // 차원 불일치
        assert!(try_apply_delta(&base, &mk(8, 2, vec![])).is_err());
        // row 인덱스 범위 밖
        assert!(
            try_apply_delta(
                &base,
                &mk(
                    4,
                    2,
                    vec![RowPatch {
                        row: 9,
                        cells: vec![cell('z'); 4]
                    }]
                )
            )
            .is_err()
        );
        // patch cells 수가 cols와 불일치
        assert!(
            try_apply_delta(
                &base,
                &mk(
                    4,
                    2,
                    vec![RowPatch {
                        row: 0,
                        cells: vec![cell('z'); 2]
                    }]
                )
            )
            .is_err()
        );
        // 정상 delta는 Ok
        assert!(
            try_apply_delta(
                &base,
                &mk(
                    4,
                    2,
                    vec![RowPatch {
                        row: 1,
                        cells: vec![cell('z'); 4]
                    }]
                )
            )
            .is_ok()
        );
    }

    /// 원격 peer가 보낸 기형 delta를 reader가 받으면 패닉/접속종료 대신 baseline을 버리고
    /// keyframe을 재요청한다 (codex P1). handle_decoded_event의 malformed 분기를 소켓 페어로 검증.
    #[test]
    fn 기형_delta는_패닉없이_keyframe재동기화() {
        let (client_sock, server_sock) = socket_pair();
        let writer = Mutex::new(client_sock);
        let subscribers: Arc<Mutex<Vec<RemoteSubscriber>>> = Arc::default();

        let s = SessionId(3);
        // recon seq 0으로 baseline 세팅 — base_seq는 일치시키되 delta 내용만 기형으로.
        let base = make_snapshot(10, 3, &["x"], false);
        let mut recon: HashMap<SessionId, (u64, Arc<TerminalViewportSnapshot>)> = HashMap::new();
        recon.insert(s, (0, base));
        let mut pending: HashSet<SessionId> = HashSet::new();

        // base_seq 0(일치)이지만 row가 범위 밖 + cells 수 불일치 → try_apply_delta가 Err.
        let malformed = ViewportDelta {
            cols: 10,
            rows: 3,
            cursor: CursorSnapshot {
                col: 0,
                row: 0,
                shape: CursorShape::Block,
                visible: true,
            },
            scroll_offset: 0,
            is_alt_screen: false,
            title: None,
            changed_rows: vec![RowPatch {
                row: 99,
                cells: vec![cell('z')],
            }],
        };
        let cont = handle_decoded_event(
            DecodedEvent::Delta {
                session: s,
                seq: 1,
                base_seq: 0,
                delta: malformed,
                bracketed_paste: false,
            },
            &subscribers,
            &writer,
            &mut recon,
            &mut pending,
        );
        assert!(cont, "기형 delta는 접속을 끊지 않는다");
        assert!(!recon.contains_key(&s), "기형 delta 시 baseline을 버린다");
        assert!(pending.contains(&s), "keyframe 재요청 pending");

        let mut r = BufReader::new(server_sock);
        let frame = read_frame(&mut r).expect("RequestKeyframe 프레임");
        assert!(matches!(
            Codec::Delta.decode_command(&frame).unwrap(),
            DecodedCommand::RequestKeyframe(got) if got == s
        ));
    }

    /// SessionExited는 서버 pump의 last_sent에서 그 세션 baseline을 제거한다 (codex P2).
    #[test]
    fn session_exited는_서버_last_sent_정리() {
        let s = SessionId(5);
        let mut last_sent: HashMap<SessionId, (u64, Arc<TerminalViewportSnapshot>)> =
            HashMap::new();
        let snap = make_snapshot(10, 3, &["a"], false);
        encode_viewport_frame(&mut last_sent, s, &snap, false).unwrap();
        assert!(last_sent.contains_key(&s), "keyframe이 baseline을 남긴다");

        let exit = RuntimeEvent::SessionExited {
            session: s,
            exit_code: Some(0),
        };
        let mut exited = HashSet::new();
        encode_pump_frame(Codec::Delta, &mut last_sent, &mut exited, &exit).unwrap();
        assert!(
            !last_sent.contains_key(&s),
            "SessionExited가 서버 baseline을 정리한다"
        );
        assert!(exited.contains(&s), "종료 세션이 배치 집합에 등록된다");
    }

    /// 같은 drain 배치의 [SessionExited(X), Viewport(X)]에서 trailing viewport가 종료 세션의
    /// baseline을 되살리지 않는다 (codex P2). 그래도 최종 출력은 클라 slot에 emit된다.
    #[test]
    fn 종료_배치의_trailing_viewport는_baseline_되살리지_않는다() {
        let s = SessionId(8);
        // 이전 tick의 keyframe으로 양쪽에 baseline이 있었다고 가정.
        let mut last_sent: HashMap<SessionId, (u64, Arc<TerminalViewportSnapshot>)> =
            HashMap::new();
        let snap0 = make_snapshot(10, 3, &["a"], false);
        encode_viewport_frame(&mut last_sent, s, &snap0, false).unwrap();
        assert!(last_sent.contains_key(&s));

        // 종료 tick: drain 순서대로 SessionExited(X) 먼저, Viewport(X) 나중.
        let mut exited: HashSet<SessionId> = HashSet::new();
        let exit_ev = RuntimeEvent::SessionExited {
            session: s,
            exit_code: Some(0),
        };
        let final_snap = make_snapshot(10, 3, &["bye"], false);
        let vp_ev = RuntimeEvent::Viewport {
            session: s,
            snapshot: Arc::clone(&final_snap),
            bracketed_paste: false,
        };
        let exit_frame =
            encode_pump_frame(Codec::Delta, &mut last_sent, &mut exited, &exit_ev).unwrap();
        let vp_frame =
            encode_pump_frame(Codec::Delta, &mut last_sent, &mut exited, &vp_ev).unwrap();

        // 서버: 종료 세션 baseline이 되살아나지 않는다.
        assert!(
            !last_sent.contains_key(&s),
            "trailing viewport가 서버 baseline을 되살리면 안 된다"
        );
        // 종료 세션의 viewport는 keyframe/delta가 아니라 plain full(WireMsg::Event)로 나간다.
        assert!(
            matches!(
                Codec::Delta.decode_event(&vp_frame).unwrap(),
                DecodedEvent::Event(RuntimeEvent::Viewport { .. })
            ),
            "종료 세션 viewport는 plain Event여야 한다"
        );

        // 클라이언트: slot을 관찰할 실제 구독자 하나를 붙이고 두 프레임을 순서대로 처리한다.
        let (client_sock, _server) = socket_pair();
        let writer = Mutex::new(client_sock);
        let (tx, _rx) = std::sync::mpsc::channel();
        let slot: Arc<Mutex<HashMap<SessionId, RuntimeEvent>>> = Arc::default();
        let subscribers: Arc<Mutex<Vec<RemoteSubscriber>>> =
            Arc::new(Mutex::new(vec![RemoteSubscriber {
                events: tx,
                viewports: Arc::clone(&slot),
            }]));
        let mut recon: HashMap<SessionId, (u64, Arc<TerminalViewportSnapshot>)> = HashMap::new();
        recon.insert(s, (0, snap0)); // 클라도 baseline이 있었음
        let mut pending: HashSet<SessionId> = HashSet::new();

        let dec_exit = Codec::Delta.decode_event(&exit_frame).unwrap();
        handle_decoded_event(dec_exit, &subscribers, &writer, &mut recon, &mut pending);
        assert!(
            !recon.contains_key(&s),
            "SessionExited가 클라 recon을 정리한다"
        );

        let dec_vp = Codec::Delta.decode_event(&vp_frame).unwrap();
        handle_decoded_event(dec_vp, &subscribers, &writer, &mut recon, &mut pending);
        assert!(
            !recon.contains_key(&s),
            "trailing viewport가 클라 recon을 되살리면 안 된다"
        );

        // 그래도 최종 출력 viewport는 slot에 emit돼 화면에 보인다.
        let emitted = slot.lock().unwrap();
        match emitted.get(&s) {
            Some(RuntimeEvent::Viewport { snapshot, .. }) => {
                assert_eq!(
                    snapshot.as_ref(),
                    final_snap.as_ref(),
                    "최종 viewport가 slot에 emit"
                )
            }
            _ => panic!("최종 viewport가 slot에 emit돼야 한다"),
        }
    }

    /// SessionExited는 클라이언트 reader의 recon/pending_keyframe에서 그 세션을 제거한다 (codex P2).
    #[test]
    fn session_exited는_클라_recon_정리() {
        let (client_sock, _server) = socket_pair();
        let writer = Mutex::new(client_sock);
        let subscribers: Arc<Mutex<Vec<RemoteSubscriber>>> = Arc::default();

        let s = SessionId(6);
        let mut recon: HashMap<SessionId, (u64, Arc<TerminalViewportSnapshot>)> = HashMap::new();
        recon.insert(s, (2, make_snapshot(10, 3, &["a"], false)));
        let mut pending: HashSet<SessionId> = HashSet::new();
        pending.insert(s);

        let cont = handle_decoded_event(
            DecodedEvent::Event(RuntimeEvent::SessionExited {
                session: s,
                exit_code: None,
            }),
            &subscribers,
            &writer,
            &mut recon,
            &mut pending,
        );
        assert!(cont);
        assert!(
            !recon.contains_key(&s),
            "SessionExited가 클라 recon을 정리한다"
        );
        assert!(!pending.contains(&s), "SessionExited가 pending을 정리한다");
    }
}
