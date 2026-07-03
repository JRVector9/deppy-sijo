//! Remote Transport (설계문서 PR-19 스켈레톤 + v1 auth/바이너리 프레이밍).
//! localhost-only attach — InProcessRuntimeClient와 **같은 명령/이벤트 모델**을
//! length-prefixed 바이너리 프레임([u32 LE len][postcard])으로 loopback TCP에
//! 실어 나른다 (JSON Vec<u8> 숫자 배열 팽창 해소).
//! 인증: 서버가 실행마다 생성하는 토큰을 클라이언트가 첫 프레임으로 보낸다 —
//! §1.5 "auth required by default". 불일치/무응답(5s)은 즉시 종료.
//! public remote는 아직 아니다: bind는 127.0.0.1 고정, attach는 loopback만 허용.
//! (TLS/delta 스트림은 v1+ — §8.2 "Viewport는 terminal delta로 대체되는 자리")
//!
//! 신뢰 경계 주의 (public remote 전 필수 — codex 리뷰): SpawnAgent가 command/args/
//! env/credential_id를 그대로 실어 나르므로, 이 프로토콜을 공개 네트워크에 내놓기
//! 전에 인증·권한(capability) 레이어나 제한된 원격 명령 스키마가 반드시 선행해야 한다.
//! 지금은 loopback 강제가 그 경계다. 와이어 값 검증(validate_command/validate_event)은
//! 기형 peer 방어일 뿐 권한 통제가 아니다.

use std::io::{BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use anyhow::{Context, bail};
use deppy_core::SessionId;

use crate::client::{RuntimeCommandSink, RuntimeEventReceiver, RuntimeEventStream};
use crate::command::RuntimeCommand;
use crate::event::RuntimeEvent;
use crate::in_process::InProcessRuntimeClient;

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

/// 인증 성공 시 서버가 회신하는 ACK payload.
const AUTH_ACK: &[u8] = b"ok";

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
    // 인증: 첫 프레임 = 토큰. 침묵 peer가 서버를 잡아두지 못하게 timeout.
    let _ = stream.set_read_timeout(Some(AUTH_TIMEOUT));
    {
        let mut auth_reader = match stream.try_clone() {
            Ok(s) => s,
            Err(_) => return,
        };
        let authorized =
            read_frame(&mut auth_reader).is_some_and(|frame| token_matches(auth_token, &frame));
        if !authorized {
            tracing::warn!("remote 인증 실패 — 접속 거부");
            let _ = stream.shutdown(Shutdown::Both);
            return;
        }
        // 성공 ACK — 클라이언트 attach가 인증 결과를 동기적으로 알 수 있게 (codex 리뷰)
        if write_frame(&mut auth_reader, AUTH_ACK).is_err() {
            let _ = stream.shutdown(Shutdown::Both);
            return;
        }
    }
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
    let pump = std::thread::Builder::new()
        .name("remote-pump".into())
        .spawn(move || {
            let mut stream = pump_stream;
            let mut last_activity = std::time::Instant::now();
            loop {
                if pump_stop.load(Ordering::SeqCst) || pump_done.load(Ordering::SeqCst) {
                    break;
                }
                let mut sent_event = false;
                for event in receiver.drain() {
                    let payload = match postcard::to_allocvec(&event) {
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
        match postcard::from_bytes::<RuntimeCommand>(&frame) {
            Ok(command) => {
                if let Err(reason) = validate_command(&command) {
                    tracing::warn!("remote 명령 검증 실패({reason}), 접속 종료");
                    break;
                }
                if backend.send_command(command).is_err() {
                    break; // worker 종료됨
                }
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

/// loopback의 RemoteRuntimeServer에 attach하는 클라이언트.
/// InProcessRuntimeClient와 같은 trait(RuntimeCommandSink/RuntimeEventStream)을
/// 구현한다 — UI 입장에서 교체 가능 (완료 기준).
pub struct RemoteRuntimeClient {
    writer: Mutex<TcpStream>,
    subscribers: Arc<Mutex<Vec<RemoteSubscriber>>>,
    reader_thread: Option<JoinHandle<()>>,
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
        write_frame(&mut stream, token.as_bytes()).context("remote 인증 프레임 전송 실패")?;
        // 서버 ACK를 기다린다 — 없으면 잘못된 토큰/거부 (attach가 Ok를 반환하고
        // 나서야 끊긴 것을 아는 반쪽 상태 방지. codex 리뷰)
        stream
            .set_read_timeout(Some(AUTH_TIMEOUT))
            .context("remote 인증 대기 설정 실패")?;
        let acked = read_frame(&mut stream).is_some_and(|frame| frame == AUTH_ACK);
        if !acked {
            bail!("remote 인증 거부 — 토큰을 확인하세요");
        }
        stream
            .set_read_timeout(None)
            .context("remote 인증 대기 해제 실패")?;
        let subscribers: Arc<Mutex<Vec<RemoteSubscriber>>> = Arc::default();

        let reader_subscribers = Arc::clone(&subscribers);
        let reader_stream = stream.try_clone().context("remote stream clone 실패")?;
        let reader_thread = std::thread::Builder::new()
            .name("remote-events".into())
            .spawn(move || {
                let mut reader = BufReader::new(reader_stream);
                while let Some(frame) = read_frame(&mut reader) {
                    if frame.is_empty() {
                        continue; // heartbeat(길이 0 프레임) — 이벤트가 아닌 keepalive로 소비
                    }
                    let Ok(event) = postcard::from_bytes::<RuntimeEvent>(&frame) else {
                        tracing::warn!("remote 이벤트 프로토콜 위반 — 접속 종료");
                        break;
                    };
                    if let Err(reason) = validate_event(&event) {
                        tracing::warn!("remote 이벤트 검증 실패({reason}) — 접속 종료");
                        break;
                    }
                    dispatch(&reader_subscribers, event);
                }
                // 수신이 죽은 클라이언트가 명령 전송만 성공하는 반쪽 상태 방지 —
                // 소켓을 양방향으로 닫아 이후 send_command도 실패하게 한다 (codex 리뷰)
                let _ = reader.into_inner().shutdown(Shutdown::Both);
            })
            .context("remote reader thread 생성 실패")?;

        Ok(Self {
            writer: Mutex::new(stream),
            subscribers,
            reader_thread: Some(reader_thread),
        })
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
        let payload = postcard::to_allocvec(&command).context("remote 명령 직렬화 실패")?;
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
    use std::time::{Duration, Instant};

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

    /// §1.5 auth required: 잘못된 토큰은 attach가 이벤트를 받지 못하고 끊긴다.
    #[test]
    fn 잘못된_토큰은_거부() {
        let server = RemoteRuntimeServer::serve(test_backend("badtoken"), 0).unwrap();
        let mut raw = TcpStream::connect(server.local_addr()).unwrap();
        write_frame(&mut raw, b"wrong-token").unwrap();
        // 서버가 끊는다 — read가 EOF(0)
        raw.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut buf = [0u8; 16];
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match std::io::Read::read(&mut raw, &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            assert!(Instant::now() < deadline, "거부 대기 시간 초과");
        }
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
        write_frame(&mut raw, server.auth_token().as_bytes()).unwrap();
        raw.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let ack = read_frame(&mut raw).expect("auth ack 없음");
        assert_eq!(ack, AUTH_ACK);

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
        write_frame(&mut raw, server.auth_token().as_bytes()).unwrap();
        raw.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let _ack = read_frame(&mut raw).expect("auth ack 없음");

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
        // 정상 인증 후 postcard로 해석 불가한 프레임 전송
        write_frame(&mut raw, server.auth_token().as_bytes()).unwrap();
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
}
