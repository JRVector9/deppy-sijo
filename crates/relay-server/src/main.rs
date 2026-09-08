//! Relay 데이터 평면 전송 계층. 상태 기계는 [`relay_server::core`]가 전부 가지고 있고,
//! 여기서는 소켓과 스레드만 다룬다.
//!
//! 스레드 모델: accept 스레드 하나 + 연결당 스레드 하나. 각 연결 스레드가 자기 WebSocket을
//! 단독으로 소유하며, 읽기에 시한을 걸어 주기적으로 깨어나 자기 발신 큐를 비운다. 코어 락은
//! 프레임 판정 동안만 잡고 쓰기 중에는 잡지 않는다 — 느린 소비자 하나가 서버 전체를 멈추게
//! 두지 않기 위해서다.
//!
//! TLS는 배포 edge가 종단한다. 이 프로세스는 평문 TCP 위 WebSocket만 말하며, 공개 노출은
//! edge를 통해서만 이뤄져야 한다.
//!
//! v1은 단일 인스턴스다. 라우트/티켓/연결 상태를 공유 저장소에 두지 않으므로 인스턴스를
//! 늘리면 페어링이 깨진다 — 공유 저장소를 명시적으로 선택·검증하기 전까지 수평 확장 금지.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use relay_protocol::{
    ADMISSION_CREDENTIAL_BYTES, AdmissionCredential, HEADER_BYTES, MAX_FRAME_BYTES, ROUTE_ID_BYTES,
    RejectionCode, RouteId,
};
use relay_server::{ConnectionKey, RelayAction, RelayCore, RelayLimits, RouteVerifier};
use tungstenite::protocol::WebSocketConfig;
use tungstenite::{Message, WebSocket};

/// 읽기 시한. 이 주기로 깨어나 발신 큐를 비우고 시한 만료를 정리한다.
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// 비차단 accept 폴링 주기. 정지 깃발이 관측되기까지의 최악 지연이다.
const ACCEPT_POLL: Duration = Duration::from_millis(100);
/// 쓰기 시한. 이 시간 안에 받아 가지 못하는 상대는 느린 소비자로 보고 끊는다.
///
/// 시한이 없으면 수신 윈도가 막힌 상대에게 `send`가 무한정 걸린다. 그 스레드는 자기 발신
/// 채널에 도착한 종료 지시도 못 보고, `connection_closed`도 못 부른다. 그러면 코어가
/// 붙잡아 둔 정리 대기 예산이 영원히 안 풀려 서버 전체 발신 예산이 말라붙는다.
const WRITE_DEADLINE: Duration = Duration::from_secs(10);
/// 입장 전 연결이 보낼 수 있는 가장 큰 프레임 — 헤더 + 32바이트 자격증명.
///
/// DRLY 선점검은 WebSocket 메시지 하나가 **다 모인 뒤에야** 돌 수 있다. 그래서 입장 전에는
/// WebSocket 수준의 상한을 이 크기로 조여, 아직 아무 자격도 증명하지 않은 상대가 1 MiB짜리
/// 읽기 버퍼를 잡게 만들지 못하도록 한다. 입장에 성공한 뒤에만 정규 상한으로 올린다.
const PRE_ADMISSION_MESSAGE_BYTES: usize = HEADER_BYTES + ADMISSION_CREDENTIAL_BYTES;

enum Outbound {
    Frame(Vec<u8>),
    Close(RejectionCode),
}

/// 연결별 발신 채널 **과 소켓 복제본**. 절단 지시를 채널에만 넣으면, `read()`에 갇힌 워커는
/// 그 지시를 영영 못 본다 — 1바이트씩 흘려 넣는 상대는 읽기 시한을 매번 갱신해 `read()`가
/// 돌아오지 않게 만들 수 있다. 소켓을 직접 닫아야 그 `read()`가 오류로 돌아온다.
type Registry = Arc<Mutex<HashMap<ConnectionKey, (Sender<Outbound>, TcpStream)>>>;

/// 살아 있는 접속 스레드 수. accept마다 스레드를 만들기 **전에** 이 값으로 상한을 건다 —
/// 코어의 `max_connections`는 WebSocket 핸드셰이크 뒤에야 적용되므로, 그 전 단계에서
/// 스레드가 무한정 생기는 것을 막지 못한다.
static LIVE_WORKERS: AtomicUsize = AtomicUsize::new(0);

/// SIGTERM/SIGINT가 올리는 정지 깃발. `systemctl stop`이 프레임 중간에 프로세스를 죽이는
/// 대신, accept 루프가 이 깃발을 보고 모든 연결을 정확히 한 번씩 닫는다.
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn request_shutdown(_signal: libc::c_int) {
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
}

#[cfg(unix)]
fn install_signal_handlers() {
    // SAFETY: 핸들러는 원자적 저장 한 번만 수행한다 — async-signal-safe다.
    unsafe {
        libc::signal(
            libc::SIGTERM,
            request_shutdown as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGINT,
            request_shutdown as *const () as libc::sighandler_t,
        );
    }
}

#[cfg(not(unix))]
fn install_signal_handlers() {}

/// 접속 스레드 하나의 수명 동안 `LIVE_WORKERS`를 잡는다. panic으로 죽어도 Drop이 되돌린다.
struct WorkerSlot;

impl WorkerSlot {
    fn try_acquire(limit: usize) -> Option<Self> {
        let taken = LIVE_WORKERS.fetch_add(1, Ordering::SeqCst);
        if taken >= limit {
            LIVE_WORKERS.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(Self)
    }
}

impl Drop for WorkerSlot {
    fn drop(&mut self) {
        LIVE_WORKERS.fetch_sub(1, Ordering::SeqCst);
    }
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_ansi(false)
        .init();

    let bind: SocketAddr = std::env::var("DEPPY_RELAY_BIND")
        .unwrap_or_else(|_| "127.0.0.1:9443".to_owned())
        .parse()
        .context("DEPPY_RELAY_BIND 주소 형식이 올바르지 않다")?;
    let verifiers = route_verifiers_from_env()?;

    let core = Arc::new(Mutex::new(
        RelayCore::new(RelayLimits::default(), verifiers)
            .map_err(|error| anyhow::anyhow!("Relay 코어 구성 실패: {error}"))?,
    ));
    let registry: Registry = Arc::new(Mutex::new(HashMap::new()));
    let running = Arc::new(AtomicBool::new(true));

    let listener = TcpListener::bind(bind).with_context(|| format!("{bind} bind 실패"))?;
    // 비차단 accept + 폴링. 차단 accept는 정지 깃발을 볼 기회가 없어 SIGTERM이 프레임
    // 중간에 프로세스를 죽이게 둔다.
    listener
        .set_nonblocking(true)
        .context("listener 설정 실패")?;
    install_signal_handlers();
    let max_workers = RelayLimits::default().max_connections;
    tracing::info!(%bind, "Relay 데이터 평면 시작 (TLS는 배포 edge가 종단한다)");

    // 핸드셰이크 이전 소켓도 종료 때 닫는다. 개수는 WorkerSlot 상한에 묶인다.
    let mut workers: Vec<(JoinHandle<()>, TcpStream)> = Vec::new();
    while running.load(Ordering::SeqCst) && !SHUTDOWN_REQUESTED.load(Ordering::SeqCst) {
        let stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(ACCEPT_POLL);
                continue;
            }
            Err(error) => {
                tracing::warn!(%error, "accept 실패");
                continue;
            }
        };
        workers.retain(|(worker, _)| !worker.is_finished());
        // 스레드를 만들기 **전에** 상한을 건다. 코어의 연결 상한은 핸드셰이크 뒤에야
        // 적용되므로, 그 전 단계에서 스레드가 무한정 생기는 것은 여기서 막는다.
        let Some(slot) = WorkerSlot::try_acquire(max_workers) else {
            tracing::debug!("접속 스레드 상한 — 연결을 받지 않는다");
            drop(stream);
            continue;
        };
        let shutdown_handle = match stream.try_clone() {
            Ok(handle) => handle,
            Err(error) => {
                tracing::warn!(%error, "종료용 소켓 복제 실패");
                continue;
            }
        };
        let core = core.clone();
        let registry = registry.clone();
        let running = running.clone();
        let worker = std::thread::spawn(move || {
            let _slot = slot;
            if let Err(error) = serve(stream, &core, &registry, &running) {
                // 오류 문자열에는 프레임 내용이 들어가지 않는다 — 종류만 남긴다.
                tracing::debug!(%error, "연결 종료");
            }
        });
        workers.push((worker, shutdown_handle));
    }

    // 종료: 모든 연결에 한 번씩 통지하고 스레드를 join한다.
    running.store(false, Ordering::SeqCst);
    let actions = core
        .lock()
        .map_err(|_| anyhow::anyhow!("Relay 코어 잠금 오염"))?
        .shutdown(unix_now());
    dispatch(&actions, &registry);
    // 일부 헤더만 계속 보내는 상대는 읽기 idle 시한을 갱신할 수 있다.
    // 아직 Registry에 없는 핸드셰이크까지 모두 끊은 뒤 join한다.
    for (_, socket) in &workers {
        let _ = socket.shutdown(Shutdown::Both);
    }
    for (worker, _) in workers {
        let _ = worker.join();
    }
    Ok(())
}

/// 라우트와 Mac 승인 자격증명은 **주입만 받는다**. 기본값을 지어내지 않는다 —
/// provisioning/회전 주체가 아직 정해지지 않았고(계획 Task 3 Step 2), 기본 자격증명이
/// 있는 순간 그것이 곧 백도어가 된다.
fn route_verifiers_from_env() -> anyhow::Result<Vec<RouteVerifier>> {
    let raw = std::env::var("DEPPY_RELAY_ROUTES").context(
        "DEPPY_RELAY_ROUTES가 필요하다 (형식: <route-hex32>:<credential-hex64>[,...]). \
         기본 자격증명은 존재하지 않는다",
    )?;
    let mut verifiers = Vec::new();
    for entry in raw.split(',').filter(|entry| !entry.trim().is_empty()) {
        let (route, credential) = entry
            .trim()
            .split_once(':')
            .context("라우트 항목은 <route-hex>:<credential-hex> 형식이다")?;
        verifiers.push(RouteVerifier::new(
            RouteId::from_bytes(unhex::<ROUTE_ID_BYTES>(route)?),
            AdmissionCredential::from_bytes(unhex::<ADMISSION_CREDENTIAL_BYTES>(credential)?),
        ));
    }
    anyhow::ensure!(!verifiers.is_empty(), "라우트가 하나도 구성되지 않았다");
    Ok(verifiers)
}

fn unhex<const N: usize>(value: &str) -> anyhow::Result<[u8; N]> {
    anyhow::ensure!(
        value.len() == N * 2,
        "16진 값의 길이가 {}이어야 한다",
        N * 2
    );
    let mut bytes = [0u8; N];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        bytes[index] = (nibble(pair[0])? << 4) | nibble(pair[1])?;
    }
    Ok(bytes)
}

fn nibble(byte: u8) -> anyhow::Result<u8> {
    Ok(match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        b'A'..=b'F' => byte - b'A' + 10,
        _ => anyhow::bail!("16진 값이 아니다"),
    })
}

fn serve(
    stream: TcpStream,
    core: &Arc<Mutex<RelayCore>>,
    registry: &Registry,
    running: &Arc<AtomicBool>,
) -> anyhow::Result<()> {
    // macOS는 listener의 비차단 상태를 상속한다. 워커는 차단 I/O와 idle 시한을 쓴다.
    stream
        .set_nonblocking(false)
        .context("연결 소켓 차단 모드 설정 실패")?;
    let peer = stream.peer_addr().context("peer 주소를 읽지 못했다")?;
    // 절단용 복제본. 채널에 넣은 절단 지시는 `read()`에 갇힌 워커가 못 본다 — 소켓을 직접
    // 닫아야 그 `read()`가 오류로 돌아와 정리 경로에 들어온다.
    let shutdown_handle = stream.try_clone().context("소켓 복제 실패")?;
    stream
        .set_read_timeout(Some(POLL_INTERVAL))
        .context("읽기 시한 설정 실패")?;
    // 핸드셰이크도 쓰기를 한다 — accept 이전에 걸어야 한다.
    stream
        .set_write_timeout(Some(WRITE_DEADLINE))
        .context("쓰기 시한 설정 실패")?;
    stream.set_nodelay(true).ok();

    let mut config = WebSocketConfig::default();
    config.max_message_size = Some(PRE_ADMISSION_MESSAGE_BYTES);
    config.max_frame_size = Some(PRE_ADMISSION_MESSAGE_BYTES);
    let mut socket = tungstenite::accept_with_config(stream, Some(config))
        .context("WebSocket 핸드셰이크 실패")?;

    let key = {
        let mut guard = core
            .lock()
            .map_err(|_| anyhow::anyhow!("Relay 코어 잠금 오염"))?;
        guard
            .connection_opened(ip_bytes(peer.ip()), unix_now())
            .map_err(|refusal| anyhow::anyhow!("연결 거절: {refusal:?}"))?
    };

    let (sender, receiver) = channel::<Outbound>();
    registry
        .lock()
        .map_err(|_| anyhow::anyhow!("registry 잠금 오염"))?
        .insert(key, (sender, shutdown_handle));

    let result = pump(&mut socket, core, registry, running, key, &receiver);

    registry
        .lock()
        .map_err(|_| anyhow::anyhow!("registry 잠금 오염"))?
        .remove(&key);
    // 아직 채널에 남아 있는 발신 바이트를 **먼저** 버린다. 코어는 이 확인이 오기 전까지
    // 그만큼을 전체 예산에 계속 계산하고 있으므로, 순서가 뒤집히면 예산이 실제보다 먼저
    // 풀린다.
    drop(receiver);
    let actions = core
        .lock()
        .map_err(|_| anyhow::anyhow!("Relay 코어 잠금 오염"))?
        .connection_closed(key, unix_now());
    dispatch(&actions, registry);
    let _ = socket.close(None);
    result
}

fn pump(
    socket: &mut WebSocket<TcpStream>,
    core: &Arc<Mutex<RelayCore>>,
    registry: &Registry,
    running: &Arc<AtomicBool>,
    key: ConnectionKey,
    receiver: &Receiver<Outbound>,
) -> anyhow::Result<()> {
    let mut last_tick = unix_now();
    let mut admitted = false;
    while running.load(Ordering::SeqCst) {
        // 1) 대기 중인 발신을 통째로 꺼낸다. 코어의 연결당 예약이 상한을 걸어 두므로 유계다.
        let mut pending = Vec::new();
        let mut closing = None;
        loop {
            match receiver.try_recv() {
                Ok(Outbound::Frame(frame)) => pending.push(frame),
                Ok(Outbound::Close(code)) => {
                    closing = Some(code);
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    closing = Some(RejectionCode::PeerDisconnected);
                    break;
                }
            }
        }
        if let Some(code) = closing {
            // 끊기로 판정된 상대에게 밀린 백로그를 마저 써 줄 이유가 없다. 그대로 버려야
            // 코어가 붙잡아 둔 정리 대기 예산이 실제로 풀린다.
            drop(pending);
            tracing::debug!(?code, "코어 판정으로 연결을 닫는다");
            return Ok(());
        }
        // 실제로 쓴 만큼만 코어의 예약을 푼다.
        let written = pending.len();
        for frame in pending {
            socket.send(Message::Binary(frame.into()))?;
        }
        if written > 0 {
            core.lock()
                .map_err(|_| anyhow::anyhow!("Relay 코어 잠금 오염"))?
                .queue_flushed(key, written);
        }

        // 2) 한 프레임을 읽는다. 시한이 지나면 그냥 다음 바퀴로 넘어간다.
        match socket.read() {
            Ok(Message::Binary(payload)) => {
                let actions = core
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Relay 코어 잠금 오염"))?
                    .frame_received(key, &payload, unix_now())
                    .map_err(|error| anyhow::anyhow!("{error}"))?;
                let closing = closes_me(&actions, key);
                if closing {
                    // dispatch의 직접 socket shutdown보다 먼저 거절을 써야 한다.
                    flush_self(socket, &actions, key);
                }
                dispatch(&actions, registry);
                if closing {
                    return Ok(());
                }
                if !admitted
                    && core
                        .lock()
                        .map_err(|_| anyhow::anyhow!("Relay 코어 잠금 오염"))?
                        .connection_is_admitted(key)
                {
                    admitted = true;
                    socket.set_config(|config| {
                        config.max_message_size = Some(MAX_FRAME_BYTES);
                        config.max_frame_size = Some(MAX_FRAME_BYTES);
                    });
                }
            }
            // 텍스트·핑퐁 외 제어 흐름은 계약에 없다. 이진 프레임만 받는다.
            Ok(Message::Close(_)) => return Ok(()),
            Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => {}
            Ok(Message::Text(_)) => {
                anyhow::bail!("텍스트 메시지는 계약에 없다");
            }
            Err(tungstenite::Error::Io(error))
                if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(error) => return Err(error.into()),
        }

        // 3) 초당 한 번 시한 만료를 정리한다.
        let now = unix_now();
        if now > last_tick {
            last_tick = now;
            let actions = core
                .lock()
                .map_err(|_| anyhow::anyhow!("Relay 코어 잠금 오염"))?
                .tick(now);
            let closing = closes_me(&actions, key);
            if closing {
                flush_self(socket, &actions, key);
            }
            dispatch(&actions, registry);
            if closing {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// 이 연결로 향하는 프레임만 즉시 써 낸다. 쓰기가 실패해도 종료 경로는 계속 간다 —
/// 상대가 이미 사라졌을 수 있고, 그때도 정리는 끝나야 한다.
fn flush_self(socket: &mut WebSocket<TcpStream>, actions: &[RelayAction], key: ConnectionKey) {
    for action in actions {
        if let RelayAction::Send { connection, frame } = action
            && *connection == key
        {
            let _ = socket.send(Message::Binary(frame.clone().into()));
        }
    }
    let _ = socket.flush();
}

fn closes_me(actions: &[RelayAction], key: ConnectionKey) -> bool {
    actions.iter().any(
        |action| matches!(action, RelayAction::Disconnect { connection, .. } if *connection == key),
    )
}

/// 코어가 낸 지시를 각 연결의 발신 큐로 옮긴다. 여기서 소켓에 직접 쓰지 않는다 —
/// 한 연결의 느린 쓰기가 다른 연결의 판정을 막으면 안 된다.
fn dispatch(actions: &[RelayAction], registry: &Registry) {
    let Ok(guard) = registry.lock() else {
        return;
    };
    for action in actions {
        match action {
            RelayAction::Send { connection, frame } => {
                if let Some((sender, _)) = guard.get(connection) {
                    let _ = sender.send(Outbound::Frame(frame.clone()));
                }
            }
            RelayAction::Disconnect { connection, code } => {
                if let Some((sender, socket)) = guard.get(connection) {
                    let _ = sender.send(Outbound::Close(*code));
                    // 채널만으로는 부족하다. 1바이트씩 흘려 넣어 `read()`를 붙잡은 상대는
                    // 채널을 영영 확인하지 않는다 — 소켓을 닫아 `read()`를 깨운다.
                    let _ = socket.shutdown(Shutdown::Both);
                }
            }
        }
    }
}

fn ip_bytes(address: IpAddr) -> [u8; 16] {
    match address {
        IpAddr::V4(value) => value.to_ipv6_mapped().octets(),
        IpAddr::V6(value) => value.octets(),
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}
