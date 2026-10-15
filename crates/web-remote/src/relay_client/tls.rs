//! 바깥 방향 WSS 전송. [`super::worker::RelayTransport`]의 프로덕션 구현이다.
//!
//! 신뢰 근거는 **컴파일된 WebPKI 루트 하나뿐**이다. 시스템 신뢰 저장소도, 사설 CA도, 검증
//! 우회도 없다. 기기에 심어진 임의 CA를 믿으면 Relay 종단을 가로챌 수 있고, 그러면 E2EE로
//! 감싸지 못하는 메타데이터(라우트 핸들, 접속 시각, 트래픽 양)가 그대로 샌다.
//!
//! 시한은 단계마다 건다: DNS 조회, TCP 연결, 그리고 소켓 읽기/쓰기. 어느 한 단계라도 시한 없이
//! 두면 워커 스레드가 거기서 영구히 멈추고, 그러면 끄기도 종료도 막힌다.

use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

use super::lifecycle::RelayEndpoint;
use super::worker::{RelaySession, RelayTransport, TransportError};

/// 한 프레임의 상한 — 데이터 평면의 값을 그대로 쓴다. 숫자를 따로 적으면 언젠가 갈라진다.
pub const MAX_RELAY_FRAME_BYTES: usize = relay_protocol::MAX_FRAME_BYTES;

pub struct TlsRelayTransport;

impl RelayTransport for TlsRelayTransport {
    fn connect(
        &mut self,
        endpoint: &RelayEndpoint,
        deadline: Duration,
    ) -> Result<Box<dyn RelaySession>, TransportError> {
        let started = Instant::now();
        let left = |elapsed: Duration| deadline.checked_sub(elapsed).filter(|left| !left.is_zero());

        // 1) DNS. **시한 안에서** 조회한다.
        let addresses = resolve_within(endpoint.host(), endpoint.port(), {
            left(started.elapsed()).ok_or(TransportError::Unavailable)?
        })?;

        // 2) TCP. 주소를 순서대로 시도하되 전체 시한을 넘기지 않는다.
        let mut stream = None;
        for address in addresses {
            let Some(left) = left(started.elapsed()) else {
                break;
            };
            if let Ok(connected) = TcpStream::connect_timeout(&address, left) {
                stream = Some(connected);
                break;
            }
        }
        let stream = stream.ok_or(TransportError::Unavailable)?;
        stream.set_nodelay(true).ok();

        // 3) 핸드셰이크에도 **남은 접속 예산**을 건다. 고정 소켓 시한을 그대로 쓰면 TCP가
        //    예산을 거의 다 쓴 뒤 TLS/WS 단계에서 그만큼을 또 기다릴 수 있다.
        let handshake_budget = left(started.elapsed()).ok_or(TransportError::Unavailable)?;
        stream
            .set_read_timeout(Some(handshake_budget))
            .map_err(|_| TransportError::Unavailable)?;
        stream
            .set_write_timeout(Some(handshake_budget))
            .map_err(|_| TransportError::Unavailable)?;

        // 4) TLS + WebSocket. connector를 `None`으로 두면 tungstenite가 이 크레이트에
        //    켜진 feature(`rustls-tls-webpki-roots`)로 기본 커넥터를 만든다 — 즉 컴파일된
        //    WebPKI 루트와 통상적인 호스트명 검증만 쓴다.
        let mut stream = DeadlineTcpStream::new(stream);
        stream.begin_io(handshake_budget)?;
        let mut config = tungstenite::protocol::WebSocketConfig::default();
        config.max_message_size = Some(MAX_RELAY_FRAME_BYTES);
        config.max_frame_size = Some(MAX_RELAY_FRAME_BYTES);
        let (socket, _response) =
            tungstenite::client_tls_with_config(endpoint.url(), stream, Some(config), None)
                .map_err(classify_handshake)?;

        let mut session = TlsRelaySession { socket };
        // 세션 단계의 쓰기 시한으로 되돌린다. 읽기 시한은 매 수신마다 워커가 준 값으로 맞춘다.
        session.set_write_timeout(WRITE_TIMEOUT)?;
        Ok(Box::new(session))
    }
}

/// 소켓 쓰기 시한. 받아 가지 못하는 상대에게 영구히 걸리지 않는다.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// 동시에 살아 있을 수 있는 이름 풀이 스레드 수.
///
/// 백오프는 시도 **빈도**만 제한한다. 해석기가 영영 돌아오지 않는 상황에서는 시도마다 버려진
/// 스레드가 하나씩 쌓여, 상한이 없으면 프로세스 수명 내내 늘어난다. 그래서 총량에 직접 상한을
/// 둔다. 어차피 해석기가 막혀 있으면 붙지도 못하므로, 여기서 거절하는 편이 옳다.
const MAX_OUTSTANDING_LOOKUPS: usize = 2;

static OUTSTANDING_LOOKUPS: AtomicUsize = AtomicUsize::new(0);

/// 시한 안에서 이름을 푼다.
///
/// std 해석기에는 시한 API가 없다. 그래서 조회는 별도 스레드에 맡기고 **기다리는 쪽에** 시한을
/// 건다. 시한을 넘기면 그 스레드는 버린다 — OS 해석기가 끝나면 스스로 종료하며 카운터를
/// 되돌린다. 소유 스레드가 해석기 안에서 멈추면 끄기도 종료도 막히는데, 그쪽이 훨씬 나쁘다.
fn resolve_within(
    host: &str,
    port: u16,
    budget: Duration,
) -> Result<Vec<SocketAddr>, TransportError> {
    // 자리를 먼저 잡는다. 상한을 넘으면 스레드를 아예 만들지 않는다.
    let taken = OUTSTANDING_LOOKUPS.fetch_add(1, Ordering::SeqCst);
    if taken >= MAX_OUTSTANDING_LOOKUPS {
        OUTSTANDING_LOOKUPS.fetch_sub(1, Ordering::SeqCst);
        return Err(TransportError::Unavailable);
    }

    let (sender, receiver) = channel();
    let host = host.to_owned();
    let spawned = std::thread::Builder::new()
        .name("relay-dns".into())
        .spawn(move || {
            let resolved = (host.as_str(), port)
                .to_socket_addrs()
                .map(|addresses| addresses.collect::<Vec<_>>());
            let _ = sender.send(resolved);
            OUTSTANDING_LOOKUPS.fetch_sub(1, Ordering::SeqCst);
        });
    if spawned.is_err() {
        OUTSTANDING_LOOKUPS.fetch_sub(1, Ordering::SeqCst);
        return Err(TransportError::Unavailable);
    }

    match receiver.recv_timeout(budget) {
        Ok(Ok(addresses)) if !addresses.is_empty() => Ok(addresses),
        _ => Err(TransportError::Unavailable),
    }
}

/// 아직 돌아오지 않은 이름 풀이 스레드 수.
#[cfg(test)]
fn outstanding_lookups() -> usize {
    OUTSTANDING_LOOKUPS.load(Ordering::SeqCst)
}

/// 핸드셰이크 실패를 재시도 가능 여부로 가른다.
///
/// HTTP 401/403은 자격증명이 거부됐다는 뜻이므로 **재시도 금지** 쪽으로 보낸다. 그 외에는
/// 일시적 실패로 본다 — TLS 오류나 DNS 실패로 영구히 포기하면 잠깐의 네트워크 장애가
/// 사용자 개입을 요구하는 상태로 굳는다.
type ClientHandshakeError = tungstenite::handshake::HandshakeError<
    tungstenite::handshake::client::ClientHandshake<MaybeTlsStream<DeadlineTcpStream>>,
>;

fn classify_handshake(error: ClientHandshakeError) -> TransportError {
    match error {
        tungstenite::handshake::HandshakeError::Failure(tungstenite::Error::Http(response)) => {
            match response.status().as_u16() {
                401 | 403 => TransportError::AuthenticationRejected,
                410 => TransportError::Revoked,
                _ => TransportError::Unavailable,
            }
        }
        _ => TransportError::Unavailable,
    }
}

/// The deadline belongs below both TLS and WebSocket decoders. A timeout on each raw read
/// alone can be renewed indefinitely by a peer dripping partial records or message bytes.
struct DeadlineTcpStream {
    stream: TcpStream,
    deadline: Option<Instant>,
    stop: Option<Arc<AtomicBool>>,
}

const CANCEL_IO_SLICE: Duration = Duration::from_millis(50);

impl DeadlineTcpStream {
    fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            deadline: None,
            stop: None,
        }
    }

    fn begin_io(&mut self, timeout: Duration) -> Result<(), TransportError> {
        self.deadline = Some(
            Instant::now()
                .checked_add(timeout)
                .ok_or(TransportError::Unavailable)?,
        );
        Ok(())
    }

    fn remaining_slice(&self) -> io::Result<Duration> {
        if self
            .stop
            .as_ref()
            .is_some_and(|stop| stop.load(Ordering::SeqCst))
        {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "relay shutdown requested",
            ));
        }
        let remaining = self
            .deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
            .unwrap_or(CANCEL_IO_SLICE);
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "relay I/O deadline elapsed",
            ));
        }
        Ok(remaining.min(CANCEL_IO_SLICE))
    }
}

impl Read for DeadlineTcpStream {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        loop {
            self.stream
                .set_read_timeout(Some(self.remaining_slice()?))?;
            match self.stream.read(bytes) {
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                result => return result,
            }
        }
    }
}

impl Write for DeadlineTcpStream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        loop {
            self.stream
                .set_write_timeout(Some(self.remaining_slice()?))?;
            match self.stream.write(bytes) {
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                result => return result,
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.remaining_slice()?;
        self.stream.flush()
    }
}

struct TlsRelaySession {
    socket: WebSocket<MaybeTlsStream<DeadlineTcpStream>>,
}

impl TlsRelaySession {
    /// TLS 여부와 무관하게 밑에 깔린 TCP 소켓을 꺼낸다.
    fn socket_io_mut(&mut self) -> Option<&mut DeadlineTcpStream> {
        match self.socket.get_mut() {
            MaybeTlsStream::Plain(stream) => Some(stream),
            MaybeTlsStream::Rustls(stream) => Some(&mut stream.sock),
            _ => None,
        }
    }

    fn set_write_timeout(&mut self, timeout: Duration) -> Result<(), TransportError> {
        self.socket_io_mut()
            .ok_or(TransportError::Unavailable)?
            .begin_io(timeout)
    }

    fn apply_read_timeout(&mut self, timeout: Duration) -> Result<(), TransportError> {
        // Refresh the deadline even for the same duration: each receive owns a new total budget.
        self.socket_io_mut()
            .ok_or(TransportError::Unavailable)?
            .begin_io(timeout)
    }
}

impl RelaySession for TlsRelaySession {
    fn receive(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>, TransportError> {
        self.apply_read_timeout(timeout)?;
        match self.socket.read() {
            Ok(Message::Binary(payload)) => Ok(Some(payload.to_vec())),
            Ok(Message::Text(_) | Message::Close(_)) => Err(TransportError::Unavailable),
            Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => Ok(None),
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                Ok(None)
            }
            Err(_) => Err(TransportError::Unavailable),
        }
    }

    fn send(&mut self, frame: &[u8]) -> Result<(), TransportError> {
        if frame.len() > MAX_RELAY_FRAME_BYTES {
            return Err(TransportError::Unavailable);
        }
        self.set_write_timeout(WRITE_TIMEOUT)?;
        self.socket
            .send(Message::Binary(frame.to_vec().into()))
            .map_err(|_| TransportError::Unavailable)
    }

    fn close(&mut self) {
        // Retire the transport immediately, including incomplete TLS/WS frames. Waiting to flush
        // a graceful close to a stalled peer must not reintroduce a shutdown write dependency.
        if let Some(io) = self.socket_io_mut() {
            let _ = io.stream.shutdown(Shutdown::Both);
        }
    }

    fn set_stop_signal(&mut self, stop: Arc<AtomicBool>) {
        if let Some(io) = self.socket_io_mut() {
            io.stop = Some(stop);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn production_source() -> &'static str {
        include_str!("tls.rs")
            .split("\n#[cfg(test)]\nmod tests {")
            .next()
            .unwrap()
    }

    /// 신뢰 근거는 컴파일된 WebPKI 루트 하나뿐이다. 대체 경로를 열어 두면 그것이 곧
    /// 중간자 경로가 된다.
    #[test]
    fn the_transport_offers_no_alternative_trust_path() {
        let production = production_source();
        for forbidden in [
            "NativeTls",
            "native_tls",
            "rustls_native_certs",
            "native-roots",
            "danger_accept",
            "dangerous(",
            "ServerCertVerifier",
            "add_trust_anchor",
            "Connector::Plain",
            "insecure",
        ] {
            assert!(!production.contains(forbidden), "{forbidden}");
        }
        // 커넥터를 직접 만들지 않는다 — feature로 정해진 기본 커넥터만 쓴다.
        assert!(
            production
                .contains("client_tls_with_config(endpoint.url(), stream, Some(config), None)"),
            "기본(WebPKI) 커넥터를 쓰는 호출이 바뀌었다"
        );
    }

    /// 이 크레이트의 매니페스트가 시스템 루트나 native-tls를 켜지 않는다.
    #[test]
    fn the_manifest_pins_webpki_roots_only() {
        // 주석은 금지 대상을 **설명하려고** 그 이름을 적는다. 지시문만 본다.
        let directives = include_str!("../../Cargo.toml")
            .lines()
            .filter(|line| !line.trim_start().starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(directives.contains("\"rustls-tls-webpki-roots\","));
        for forbidden in ["rustls-tls-native-roots", "native-tls"] {
            assert!(!directives.contains(forbidden), "{forbidden}");
        }
    }

    /// 모든 단계에 시한이 있다. 하나라도 빠지면 소유 스레드가 거기서 멈추고, 그러면 끄기도
    /// 종료도 막힌다. DNS·TCP·핸드셰이크·읽기·쓰기 다섯 단계를 모두 확인한다.
    #[test]
    fn every_stage_of_the_connection_carries_a_deadline() {
        let production = production_source();
        for (stage, required) in [
            ("DNS", "resolve_within(endpoint.host(), endpoint.port()"),
            ("TCP", "connect_timeout(&address, left)"),
            (
                "핸드셰이크 읽기",
                "set_read_timeout(Some(handshake_budget))",
            ),
            (
                "핸드셰이크 쓰기",
                "set_write_timeout(Some(handshake_budget))",
            ),
            ("세션 쓰기", "set_write_timeout(WRITE_TIMEOUT)"),
            ("세션 읽기", "self.apply_read_timeout(timeout)?"),
        ] {
            assert!(production.contains(required), "{stage}: {required}");
        }
        assert!(
            production.contains("deadline.checked_sub(elapsed).filter(|left| !left.is_zero())"),
            "남은 시한을 단계마다 다시 계산해야 전체 시한이 지켜진다"
        );
        // DNS 조회는 시한을 걸 수 없으므로 기다리는 쪽에 건다.
        assert!(
            production.contains("receiver.recv_timeout(budget)"),
            "이름 풀이가 소유 스레드를 붙잡으면 종료가 막힌다"
        );
        // 고정 읽기 시한 상수를 되살리면 워커가 준 시한이 무시된다.
        assert!(
            !production.contains("const READ_TIMEOUT"),
            "읽기 시한은 워커가 정한다 — 고정 상수를 두면 그 값이 무시된다"
        );
    }

    /// 버려진 이름 풀이 스레드는 총량에 상한이 있어야 한다. 백오프는 빈도만 제한하므로,
    /// 해석기가 영영 돌아오지 않으면 상한 없이는 프로세스 수명 내내 늘어난다.
    #[test]
    fn abandoned_name_lookups_are_bounded_in_total_not_just_in_rate() {
        let production = production_source();
        assert!(production.contains("const MAX_OUTSTANDING_LOOKUPS: usize = 2;"));
        assert!(
            production.contains("if taken >= MAX_OUTSTANDING_LOOKUPS"),
            "상한 확인이 스레드 생성보다 먼저여야 한다"
        );
        let guard = production
            .find("if taken >= MAX_OUTSTANDING_LOOKUPS")
            .expect("상한 확인");
        let spawn = production
            .find("std::thread::Builder::new()")
            .expect("스레드 생성");
        assert!(guard < spawn, "상한을 넘으면 스레드를 아예 만들지 않는다");
        assert!(
            production.contains("OUTSTANDING_LOOKUPS.fetch_sub(1, Ordering::SeqCst);\n        });"),
            "조회가 끝나면 자리를 반드시 돌려줘야 한다"
        );
    }

    /// 끝난 조회는 자리를 돌려준다 — 그러지 않으면 상한이 곧 영구 잠금이 된다.
    #[test]
    fn a_completed_lookup_returns_its_slot() {
        // 해석에 실패하더라도(존재하지 않는 이름) 스레드는 끝나고 자리를 돌려준다.
        for _ in 0..(MAX_OUTSTANDING_LOOKUPS * 3) {
            let _ = resolve_within(
                "relay.invalid.example.test.invalid",
                443,
                Duration::from_secs(5),
            );
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline && outstanding_lookups() > 0 {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            outstanding_lookups(),
            0,
            "끝난 조회가 자리를 돌려주지 않으면 상한이 영구 잠금이 된다"
        );
    }

    /// 워커가 준 수신 시한이 실제 소켓에 걸려야 한다. 무시하면 끄기·종료가 그만큼 늦어진다.
    #[test]
    fn the_session_honours_the_receive_timeout_it_is_given() {
        let production = production_source();
        assert!(
            production.contains("fn receive(&mut self, timeout: Duration)"),
            "수신 시한 인자를 `_timeout`으로 흘려버리면 안 된다"
        );
        assert!(production.contains("fn apply_read_timeout(&mut self, timeout: Duration)"));
        assert!(production.contains("stream.begin_io(handshake_budget)?"));
        assert!(production.contains("fn remaining_slice(&self)"));
    }

    /// 자격증명 거부와 일시적 실패를 갈라야 자동 재시도 정책이 의미를 갖는다.
    #[test]
    fn only_credential_rejections_map_to_a_non_retryable_error() {
        let production = production_source();
        assert!(production.contains("401 | 403 => TransportError::AuthenticationRejected"));
        assert!(production.contains("410 => TransportError::Revoked"));
        assert!(
            production.contains("_ => TransportError::Unavailable"),
            "그 밖의 실패는 재시도 가능해야 한다 — 잠깐의 장애가 사용자 개입을 요구하는 \
             상태로 굳으면 안 된다"
        );
    }

    /// Mac 클라이언트·E2EE 계약·데이터 평면 세 곳의 상한을 **컴파일 타임에** 묶는다.
    ///
    /// 세 값이 갈라지면 한쪽이 만든 프레임을 다른 쪽이 거부하는데, 그건 실제 기기를 붙여
    /// 봐야 드러난다. 숫자를 각자 적어 두는 대신 서로를 참조하게 만든다.
    #[test]
    fn the_frame_ceiling_is_tied_to_both_the_e2ee_contract_and_the_wire_protocol() {
        assert_eq!(
            MAX_RELAY_FRAME_BYTES,
            crate::relay::crypto::MAX_RELAY_CIPHERTEXT_BYTES + relay_protocol::HEADER_BYTES,
            "E2EE 레코드 상한 + 와이어 헤더가 곧 한 프레임의 상한이다"
        );
        assert_eq!(
            crate::relay::crypto::MAX_RELAY_CIPHERTEXT_BYTES,
            relay_protocol::MAX_CIPHERTEXT_BYTES,
            "E2EE 계약과 데이터 평면의 암호문 상한이 갈라지면 안 된다"
        );
        assert_eq!(MAX_RELAY_FRAME_BYTES, relay_protocol::MAX_FRAME_BYTES);
    }

    #[test]
    fn full_fix_20261014_relay_receive_obeys_total_budget() {
        use std::io::Write;
        use tungstenite::protocol::Role;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        let mut session = TlsRelaySession {
            socket: WebSocket::from_raw_socket(
                MaybeTlsStream::Plain(DeadlineTcpStream::new(client)),
                Role::Client,
                None,
            ),
        };
        let send = std::thread::spawn(move || {
            server.write_all(&[0x82, 8]).unwrap();
            for _ in 0..8 {
                std::thread::sleep(Duration::from_millis(25));
                server.write_all(b"x").unwrap();
            }
        });
        let started = Instant::now();
        let result = session.receive(Duration::from_millis(40));
        let elapsed = started.elapsed();
        send.join().unwrap();
        println!(
            "receive_budget_ms=40 elapsed_ms={} received_bytes={}",
            elapsed.as_millis(),
            result.unwrap().map(|b| b.len()).unwrap_or(0)
        );
        assert!(
            elapsed < Duration::from_millis(120),
            "production receive ignored total deadline while bytes continued arriving"
        );
    }

    #[test]
    fn full_fix_20261014_relay_worker_shutdown_observes_receive_slice() {
        use super::super::lifecycle::BackoffPolicy;
        use super::super::worker::{
            IgnoreObserver, RelayDeadlines, RelayFrameSink, RelayWorker, SinkOutcome,
        };
        use std::io::Write;
        use std::sync::{Arc, mpsc};
        use tungstenite::protocol::Role;
        struct Ready {
            session: Option<Box<dyn RelaySession>>,
        }
        impl RelayTransport for Ready {
            fn connect(
                &mut self,
                _: &RelayEndpoint,
                _: Duration,
            ) -> Result<Box<dyn RelaySession>, TransportError> {
                self.session.take().ok_or(TransportError::Unavailable)
            }
        }
        struct Sink;
        impl RelayFrameSink for Sink {
            fn accept(&mut self, _: &[u8]) -> SinkOutcome {
                SinkOutcome::Continue
            }
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut peer, _) = listener.accept().unwrap();
        let session = TlsRelaySession {
            socket: WebSocket::from_raw_socket(
                MaybeTlsStream::Plain(DeadlineTcpStream::new(client)),
                Role::Client,
                None,
            ),
        };
        let (in_read, ready) = mpsc::sync_channel(1);
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let sender = std::thread::spawn(move || {
            peer.write_all(&[0x82, 16]).unwrap();
            peer.write_all(b"x").unwrap();
            in_read.send(()).unwrap();
            started_rx.recv().unwrap();
            for _ in 1..16 {
                std::thread::sleep(Duration::from_millis(200));
                if peer.write_all(b"x").is_err() {
                    break;
                }
            }
        });
        let mut worker = RelayWorker::spawn(
            RelayEndpoint::parse("wss://relay.example.test/").unwrap(),
            Box::new(Ready {
                session: Some(Box::new(session)),
            }),
            Box::new(Sink),
            Arc::new(IgnoreObserver),
            RelayDeadlines::default(),
            BackoffPolicy::default(),
        );
        assert!(worker.enable());
        ready.recv().unwrap();
        std::thread::sleep(Duration::from_millis(50));
        started_tx.send(()).unwrap();
        let began = Instant::now();
        worker.shutdown();
        let elapsed = began.elapsed();
        sender.join().unwrap();
        println!(
            "worker_receive_slice_ms=1000 shutdown_elapsed_ms={}",
            elapsed.as_millis()
        );
        assert!(
            elapsed < Duration::from_millis(1500),
            "actual RelayWorker shutdown blocked on continuously fragmented message"
        );
    }

    fn loopback_rustls() -> (
        TlsRelaySession,
        rustls::StreamOwned<rustls::ServerConnection, TcpStream>,
    ) {
        use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
        // These public synthetic fixtures are trusted only by this test's client config.
        let certificate = CertificateDer::from(
            include_bytes!("../../tests/fixtures/relay-loopback/localhost-cert.der").to_vec(),
        );
        let private_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
            include_bytes!("../../tests/fixtures/relay-loopback/localhost-key.der").to_vec(),
        ));
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let server_config = rustls::ServerConfig::builder_with_provider(Arc::clone(&provider))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![certificate.clone()], private_key)
            .unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(certificate).unwrap();
        let client_config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        server
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let peer = std::thread::spawn(move || {
            let mut peer = rustls::StreamOwned::new(
                rustls::ServerConnection::new(Arc::new(server_config)).unwrap(),
                server,
            );
            while peer.conn.is_handshaking() {
                peer.conn.complete_io(&mut peer.sock).unwrap();
            }
            peer
        });
        let mut client = rustls::StreamOwned::new(
            rustls::ClientConnection::new(
                Arc::new(client_config),
                ServerName::try_from("localhost").unwrap(),
            )
            .unwrap(),
            DeadlineTcpStream::new(client),
        );
        client.sock.begin_io(Duration::from_secs(3)).unwrap();
        while client.conn.is_handshaking() {
            client.conn.complete_io(&mut client.sock).unwrap();
        }
        let peer = peer.join().unwrap();
        let session = TlsRelaySession {
            socket: WebSocket::from_raw_socket(
                MaybeTlsStream::Rustls(client),
                tungstenite::protocol::Role::Client,
                None,
            ),
        };
        (session, peer)
    }

    #[test]
    fn full_fix_20261014_relay_rustls_partial_record_obeys_total_budget() {
        let (mut session, mut peer) = loopback_rustls();
        peer.conn
            .writer()
            .write_all(&[0x82, 4, b't', b'e', b's', b't'])
            .unwrap();
        let mut ciphertext = Vec::new();
        while peer.conn.wants_write() {
            peer.conn.write_tls(&mut ciphertext).unwrap();
        }
        assert!(
            ciphertext.len() > 6,
            "fixture must contain an encrypted TLS record"
        );
        let writer = std::thread::spawn(move || {
            for byte in ciphertext {
                if peer.sock.write_all(&[byte]).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(15));
            }
        });
        let started = Instant::now();
        let partial = session.receive(Duration::from_millis(40)).unwrap();
        let elapsed = started.elapsed();
        assert!(
            partial.is_none(),
            "a partial TLS record cannot produce a WS frame"
        );
        // Preserve the TLS/WS decoder state across receive budgets, then finish normally.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut completed = None;
        while completed.is_none() && Instant::now() < deadline {
            completed = session.receive(Duration::from_millis(40)).unwrap();
        }
        writer.join().unwrap();
        assert_eq!(completed.as_deref(), Some(b"test".as_slice()));
        eprintln!(
            "rustls partial record: budget_ms=40 first_receive_ms={}",
            elapsed.as_millis()
        );
        assert!(
            elapsed < Duration::from_millis(120),
            "TLS complete_io renewed the receive budget per ciphertext byte"
        );
    }

    #[test]
    fn full_fix_20261014_relay_rustls_fragmented_message_survives_deadlines_and_normal_send() {
        let (mut session, mut peer) = loopback_rustls();
        let (first_tx, first_rx) = std::sync::mpsc::sync_channel(1);
        let (finish_tx, finish_rx) = std::sync::mpsc::sync_channel(1);
        let writer = std::thread::spawn(move || {
            // A non-final binary fragment is retained when the next receive budget expires.
            peer.write_all(&[0x02, 2, b'o', b'k']).unwrap();
            peer.flush().unwrap();
            first_tx.send(()).unwrap();
            finish_rx.recv().unwrap();
            peer.write_all(&[0x80, 1, b'!']).unwrap();
            peer.flush().unwrap();
            let mut socket =
                WebSocket::from_raw_socket(peer, tungstenite::protocol::Role::Server, None);
            assert_eq!(socket.read().unwrap().into_data().as_ref(), b"outbound");
        });
        first_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(
            session
                .receive(Duration::from_millis(40))
                .unwrap()
                .is_none()
        );
        finish_tx.send(()).unwrap();
        assert_eq!(
            session.receive(Duration::from_secs(3)).unwrap().as_deref(),
            Some(b"ok!".as_slice())
        );
        session.send(b"outbound").unwrap();
        writer.join().unwrap();
    }

    #[test]
    fn full_fix_20261014_relay_rustls_stop_retires_partial_record_promptly() {
        let (mut session, mut peer) = loopback_rustls();
        peer.conn
            .writer()
            .write_all(&[0x82, 4, b't', b'e', b's', b't'])
            .unwrap();
        let mut ciphertext = Vec::new();
        while peer.conn.wants_write() {
            peer.conn.write_tls(&mut ciphertext).unwrap();
        }
        // Withhold the record body indefinitely after establishing actual TLS.
        peer.sock.write_all(&ciphertext[..6]).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        session.set_stop_signal(Arc::clone(&stop));
        let reader = std::thread::spawn(move || {
            let result = session.receive(Duration::from_secs(30));
            session.close();
            result
        });
        std::thread::sleep(Duration::from_millis(25));
        let started = Instant::now();
        stop.store(true, Ordering::SeqCst);
        assert_eq!(reader.join().unwrap(), Err(TransportError::Unavailable));
        let elapsed = started.elapsed();
        eprintln!(
            "rustls partial record stop: elapsed_ms={}",
            elapsed.as_millis()
        );
        assert!(
            elapsed < Duration::from_millis(250),
            "partial TLS record ignored the stop flag"
        );
        let mut closed = [0u8; 1];
        assert_eq!(
            peer.sock.read(&mut closed).unwrap(),
            0,
            "stop must retire the TCP transport"
        );
    }
}
