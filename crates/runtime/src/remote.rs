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

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, TryRecvError, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use deppy_core::SessionId;
use mux::MuxSnapshot;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{WebPkiSupportedAlgorithms, verify_tls12_signature, verify_tls13_signature};
use rustls::{DigitallySignedStruct, SignatureScheme};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use terminal::TerminalViewportSnapshot;

use crate::client::{
    LOCAL_EVENT_QUEUE_CAP, RuntimeCommandSendError, RuntimeCommandSink, RuntimeEventReceiver,
    RuntimeEventStream,
};
use crate::command::RuntimeCommand;
use crate::event::RuntimeEvent;
use crate::in_process::InProcessRuntimeClient;
use crate::protocol::{
    CLIENT_FEATURES, ClientHello, Codec, DecodedCommand, DecodedEvent, PROTO_MAGIC, PROTO_VERSION,
    SERVER_FEATURES, ServerHello, WireMsg, diff_viewport, encode_request_keyframe, encode_wire_msg,
    try_apply_delta,
};
use crate::tls_identity::TlsIdentity;

mod liveness;

/// 이벤트 pump 폴링 주기 — worker의 output batch와 별개인 전송 주기.
const PUMP_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);
/// heartbeat 주기 — 이 시간 동안 보낼 이벤트가 없으면 길이 0 프레임(keepalive)을
/// 보내 half-open(죽은) peer를 조기에 감지한다. write 실패 = 죽은 peer로 접속 정리.
const HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);
/// 프레임 payload 상한 — 바이너리라 팽창이 없으므로(postcard) 원본 크기 기준.
/// 대형 붙여넣기(수 MB)와 큰 viewport 스냅샷이 여유 있게 들어간다.
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
/// 원격 명령의 scrollback 상한 — 무제한 usize로 과대 할당을 요구하지 못하게.
use terminal::policy::SCROLLBACK_LINES_MAX as MAX_SCROLLBACK_LINES;
/// 인증 프레임 대기 상한 — 접속만 열고 침묵하는 peer가 서버를 잡아두지 못하게.
const AUTH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// 클라이언트 최초 TCP connect 상한 — OS 기본 connect timeout에 의존하지 않는다.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// TLS 접속당 단일 I/O 루프가 아무 진전이 없을 때 다음 tick 전 짧게 재운다 —
/// busy-spin(CPU 100%) 방지. 1~5ms 범위(설계 §2.4). read/write 어느 쪽도 블록하지 않는다.
const TLS_IDLE_SLEEP: Duration = Duration::from_millis(2);
/// TLS 핸드셰이크+hello 교환 상한 (TLS record + AUTH_TIMEOUT 여유). loopback에선 순식간.
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// TLS 클라이언트 명령 송신 큐(sync_channel) 용량 — 명령은 순서 보존이 필수라 코얼레싱하지
/// 않고, 유계는 이 채널 용량으로 강제한다. 초과 try_send는 Err(backpressure를 호출자에 surface).
const TLS_CMD_QUEUE_CAP: usize = 256;
/// IO 루프가 한 tick에 채널에서 흡수하는 명령 상한 — fast producer가 루프를 drain에
/// 붙잡아 read/write 인터리브를 막지 못하게 한다(기아 방지).
const TLS_CMD_DRAIN_MAX: usize = 64;
/// 한 tick의 read 단계 상한 — read_tls+복호(+프레임 드레인)를 이 횟수까지만 반복하고
/// write/drain 단계로 넘어간다(codex HIGH). peer가 소켓을 계속 readable하게 유지해도
/// (고출력 스트림) 송신 경로가 굶지 않는다. 남은 수신분은 다음 tick이 이어받는다.
const TLS_READ_STEPS_MAX: usize = 16;
/// 접속별 서버→remote client 상태/lifecycle 이벤트 큐 상한. Viewport는 별도 세션별 slot으로
/// coalesce하므로 이 cap은 silent drop이 금지된 이벤트에만 적용한다. cap 초과는 느린 client의
/// degraded 상태로 보고 접속을 끊어 재연결을 유도한다.
const OUTBOUND_DURABLE_QUEUE_CAP: usize = 1024;
/// 접속별 서버→remote client viewport slot 상한. 초과 시 가장 오래된 viewport slot을 버리고
/// 최신 viewport를 유지한다. Viewport/delta는 drop/coalesce 허용 대상이다.
const OUTBOUND_VIEWPORT_SLOT_CAP: usize = 256;
/// 한 tick에서 소켓/TLS writer로 밀어 넣는 서버 outbound 프레임 상한. 큰 backlog가 생겨도
/// pump loop가 receiver drain/read side 처리로 돌아오도록 한다.
const OUTBOUND_WRITE_FRAMES_PER_TICK: usize = 64;
/// 종료 세션 tombstone 상한. SessionExited 뒤 trailing Viewport가 delta baseline을 되살리지
/// 않도록 짧게 기억하되, 장기 연결에서 세션 수만큼 무한히 늘지 않게 한다.
const EXITED_SESSION_TOMBSTONE_CAP: usize = 512;
/// 평문 서버 이벤트 write 상한. 평문은 reader/pump가 같은 socket clone을 공유하므로 nonblocking
/// write만 켤 수 없다. 대신 짧은 write timeout 후 접속을 닫아 slow reader가 pump를 붙잡지 못하게 한다.
const PLAIN_EVENT_WRITE_TIMEOUT: Duration = Duration::from_millis(100);
/// 평문 클라이언트 명령 write 상한. TLS 경로는 bounded try_send를 쓰고, Plain 경로는 timeout으로
/// 직접 write가 caller를 오래 붙잡지 않게 한다.
const PLAIN_CMD_WRITE_TIMEOUT: Duration = Duration::from_millis(250);

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

fn connect_with(
    addr: SocketAddr,
    connect: impl FnOnce(&SocketAddr, Duration) -> std::io::Result<TcpStream>,
) -> std::io::Result<TcpStream> {
    connect(&addr, CONNECT_TIMEOUT)
}

fn observe_client_frame_for_liveness(
    tracker: &mut liveness::LivenessTracker,
    _frame: &[u8],
    now: Instant,
) {
    tracker.observe_frame(now);
}

fn client_liveness_expired(tracker: &liveness::LivenessTracker, now: Instant) -> bool {
    tracker.expired(now)
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

// ============================================================================
// TLS 전송 (단계 C-2/C-3): 접속당 **단일 I/O 스레드 + non-blocking 상태기계**
// ----------------------------------------------------------------------------
// 왜 단일 스레드인가 (설계 §2.4): rustls `Connection`은 read/write 半으로 안전하게
// 분할되지 않고 `try_clone`도 불가하므로, 평문의 reader/pump 2-스레드 모델을 TLS에는
// 쓸 수 없다. 접속당 한 스레드가 rustls 스트림을 단독 소유하고 mini 이벤트 루프로
// 양방향을 처리한다. **평문 loopback은 검증된 2-스레드 모델을 그대로 유지**(설계 §2.4의
// "평문도 통일" 권고에서 의도적으로 이탈 — 기존 테스트/동작 무변경, T1 교훈).
//
// 기아/데드락 방지 (이전 리팩터 reject 교훈):
//  - 나갈 프레임은 rustls writer에 버퍼링되고 `write_tls`로 소켓이 받는 만큼만 흘려보낸다.
//    소켓 버퍼가 차면 `write_tls`가 WouldBlock → 미전송분은 rustls 안/앱 큐에 남겨두고,
//    **write 진척과 무관하게 매 tick read를 먼저 수행**한다(명령 처리 기아 없음).
//  - 앱 큐(`out`)는 "한 배치를 다 흘려보낸 뒤에만" 다음 배치를 drain해 메모리를 배치 1개로
//    상한한다. drain을 미루는 동안 viewport는 receiver slot에서 latest-wins로 코얼레싱되고
//    (§4.2 slot 모델), 수명 이벤트는 채널에 순서대로 남는다 → viewport backpressure 코얼레싱.
//  - rustls 나가는 평문 버퍼 상한은 해제(`set_buffer_limit(None)`)한다: 한 viewport
//    keyframe이 기본 상한(64KiB)을 넘어 write_all이 WriteZero로 실패하는 것을 막고, 메모리는
//    위 배치-1 상한으로 앱계층에서 관리한다.

/// [`FrameDecoder::advance`] 결과 — WouldBlock(중간 record)/EOF/완성 프레임을 구분한다
/// (설계 §2.4: read_frame의 3-값 정제 — timeout/EOF/protocol error를 모두 None으로 접지 않기).
enum FramePoll {
    /// 완성된 프레임 하나(길이 0 = heartbeat 포함).
    Frame(Vec<u8>),
    /// 성공적으로 바이트를 소비했지만 아직 프레임이 완성되지 않았다.
    Partial,
    /// 지금은 더 읽을 게 없다(WouldBlock/TimedOut) — 다음 tick에 이어 읽는다.
    Pending,
    /// EOF 또는 프로토콜 위반(상한 초과) — 접속 종료.
    Closed,
}

fn tls_idle_sleep_needed(progressed: bool) -> bool {
    !progressed
}

/// `[u32 LE len][payload]` 프레임을 **부분 진척을 tick 간 보존**하며 디코드한다.
/// non-blocking 평문 스트림(`rustls::Connection::reader()`)에서 조각조각 읽어도 상태를 유지한다.
struct FrameDecoder {
    len_buf: [u8; 4],
    len_filled: usize,
    payload: Vec<u8>,
    payload_filled: usize,
    /// `None` = 아직 길이 4바이트 읽는 중, `Some(n)` = payload n바이트 읽는 중.
    need: Option<usize>,
}

impl FrameDecoder {
    fn new() -> Self {
        Self {
            len_buf: [0u8; 4],
            len_filled: 0,
            payload: Vec::new(),
            payload_filled: 0,
            need: None,
        }
    }

    /// 준비된 만큼 읽어 프레임 하나가 완성되면 반환한다. 성공적으로 일부 바이트만 읽었으면
    /// `Partial`(상태 보존 + IO 진척), WouldBlock/TimedOut이면 `Pending`(true idle),
    /// EOF/상한 초과면 `Closed`.
    fn advance(&mut self, r: &mut impl Read) -> FramePoll {
        loop {
            match self.need {
                None => match r.read(&mut self.len_buf[self.len_filled..]) {
                    Ok(0) => return FramePoll::Closed, // EOF
                    Ok(n) => {
                        self.len_filled += n;
                        if self.len_filled == 4 {
                            let len = u32::from_le_bytes(self.len_buf) as usize;
                            if len > MAX_FRAME_BYTES {
                                return FramePoll::Closed; // 프로토콜 위반 — 폭주 할당 방지
                            }
                            self.payload = vec![0u8; len];
                            self.payload_filled = 0;
                            self.need = Some(len);
                            if len == 0 {
                                continue;
                            }
                        }
                        return FramePoll::Partial;
                    }
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut =>
                    {
                        return FramePoll::Pending;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => return FramePoll::Closed,
                },
                Some(need) => {
                    if self.payload_filled >= need {
                        // 완성 (길이 0 heartbeat도 여기서 즉시 반환).
                        let frame = std::mem::take(&mut self.payload);
                        self.need = None;
                        self.len_filled = 0;
                        self.payload_filled = 0;
                        return FramePoll::Frame(frame);
                    }
                    match r.read(&mut self.payload[self.payload_filled..need]) {
                        Ok(0) => return FramePoll::Closed, // EOF (부분 프레임 중 단절)
                        Ok(n) => {
                            self.payload_filled += n;
                            if self.payload_filled < need {
                                return FramePoll::Partial;
                            }
                        }
                        Err(e)
                            if e.kind() == std::io::ErrorKind::WouldBlock
                                || e.kind() == std::io::ErrorKind::TimedOut =>
                        {
                            return FramePoll::Pending;
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(_) => return FramePoll::Closed,
                    }
                }
            }
        }
    }
}

/// [`tls_pump_read`] 결과 — peer EOF와 정상 진행을 구분.
enum PumpRead {
    Ok,
    Eof,
}

/// [`tls_read_step`] 결과 — 한 번의 read_tls로 진전(복호 평문 생김)/지금은 없음/EOF를 구분.
enum ReadStep {
    /// 소켓에서 TLS를 읽어 복호했다 — 평문 프레임을 드레인한 뒤 다시 읽어야 한다.
    Progressed,
    /// 지금 읽을 TLS 없음(WouldBlock) — 이 tick의 read 단계 종료.
    Idle,
    /// peer EOF.
    Eof,
}

/// **한 번만** read_tls + process_new_packets 한다(루프 없음). 호출측이 read_tls 사이에 반드시
/// 평문 프레임을 드레인하도록 강제 — 그래야 rustls received_plaintext 버퍼가 넘치지 않는다
/// ("received plaintext buffer full" 방지: read를 끝없이 돌리지 않고 read↔drain을 교대한다).
fn tls_read_step(conn: &mut rustls::Connection, sock: &mut TcpStream) -> std::io::Result<ReadStep> {
    match conn.read_tls(sock) {
        Ok(0) => Ok(ReadStep::Eof),
        Ok(_) => {
            conn.process_new_packets().map_err(std::io::Error::other)?;
            Ok(ReadStep::Progressed)
        }
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(ReadStep::Idle),
        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => Ok(ReadStep::Idle),
        Err(e) => Err(e),
    }
}

/// rustls 나갈 TLS 바이트를 소켓이 받는 만큼 흘려보낸다. 소켓 버퍼가 차면(WouldBlock)
/// 남은 바이트는 rustls 안에 남기고 멈춘다 — **블록하지 않는다**(단일 IO 데드락 방지 핵심).
fn tls_flush(conn: &mut rustls::Connection, sock: &mut TcpStream) -> std::io::Result<()> {
    while conn.wants_write() {
        match conn.write_tls(sock) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// 소켓에서 준비된 TLS 바이트를 흡수해 rustls 상태기계를 전진시킨다(handshake/app data).
/// WouldBlock이면 "지금은 없음"으로 멈춘다. peer EOF는 [`PumpRead::Eof`].
fn tls_pump_read(conn: &mut rustls::Connection, sock: &mut TcpStream) -> std::io::Result<PumpRead> {
    loop {
        match conn.read_tls(sock) {
            Ok(0) => return Ok(PumpRead::Eof),
            Ok(_) => {
                // 매 read마다 처리해 rustls 내부 버퍼가 넘치지 않게 한다.
                conn.process_new_packets().map_err(std::io::Error::other)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(PumpRead::Ok),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
}

/// 서버측 핸드셰이크 v2 (§3.1): 첫 프레임 = [`ClientHello`].
/// 매직/버전 검증 → 토큰 상수시간 비교 → features 교집합 → [`ServerHello`] 회신.
/// 성공 시 협상된 접속 [`Codec`]을 반환한다. 실패(침묵/기형/구버전 프레임/토큰 불일치)는
/// 소켓을 닫고 `None` — hang 없이 조기 거부한다(v1의 "인증 실패 = 즉시 종료" 계약 유지).
fn client_hello_matches_protocol(hello: &ClientHello) -> bool {
    hello.magic == PROTO_MAGIC && hello.proto_version == PROTO_VERSION
}

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
        Ok(hello) if client_hello_matches_protocol(&hello) => hello,
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
    if let RuntimeCommand::SpawnAgent {
        agent_config_id: Some(id),
        ..
    } = command
        && !crate::command::agent_config_id_is_valid(id)
    {
        return Err("agent_config_id가 비었거나 상한/NUL 규칙 위반");
    }
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
        RuntimeCommand::DurableEventBarrier { correlation_id: 0 } => {
            return Err("durable event barrier correlation_id는 0일 수 없음");
        }
        _ => {}
    }
    Ok(())
}

/// 클라이언트가 수신한 이벤트의 와이어 값 검증 — 기형 스냅샷이 렌더러에
/// 닿기 전에 거른다 (cols=0 나눗셈, 셀 수 불일치, 비정상 ratio).
type ViewportBaseline = (
    u64,
    Arc<TerminalViewportSnapshot>,
    Option<crate::ResizeStamp>,
);

fn validate_event(event: &RuntimeEvent) -> Result<(), &'static str> {
    let valid_stamp = |stamp: &crate::ResizeStamp| {
        stamp.epoch > 0
            && stamp.owner_epoch > 0
            && stamp.cols > 0
            && stamp.rows > 0
            && stamp
                .token
                .is_none_or(|token| token.is_valid() && token.owner_epoch == stamp.owner_epoch)
    };
    match event {
        RuntimeEvent::ResizeApplied { stamp, .. }
            if !valid_stamp(stamp) || stamp.token.is_none() =>
        {
            return Err("resize 적용 stamp 무효");
        }
        RuntimeEvent::ResizeFailed { token, .. } if !token.is_valid() => {
            return Err("resize 실패 token 무효");
        }
        _ => {}
    }
    if let RuntimeEvent::ViewportTracked {
        snapshot, stamp, ..
    } = event
        && (!valid_stamp(stamp) || (snapshot.cols, snapshot.rows) != (stamp.cols, stamp.rows))
    {
        return Err("viewport resize stamp 불일치");
    }
    match event {
        RuntimeEvent::Viewport { snapshot, .. }
        | RuntimeEvent::ViewportTracked { snapshot, .. } => {
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
        RuntimeEvent::AgentSpawnResolved {
            agent_config_id, ..
        } if !agent_config_id.is_valid() => {
            return Err("agent_config_id가 비었거나 상한/NUL 규칙 위반");
        }
        RuntimeEvent::DurableEventBarrierReached { correlation_id: 0 } => {
            return Err("durable event barrier correlation_id는 0일 수 없음");
        }
        _ => {}
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutboundDrain {
    Open,
    SourceClosed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutboundOverflow {
    DurableFull,
}

/// 접속별 서버→client outbound queue.
///
/// - non-viewport RuntimeEvent는 status/lifecycle/control event라 FIFO로 보존한다.
/// - Viewport는 세션별 최신 slot만 유지한다.
/// - durable cap 초과는 silent drop 대신 접속 종료로 surface한다.
struct OutboundEventQueue {
    durable: VecDeque<RuntimeEvent>,
    viewports: HashMap<SessionId, RuntimeEvent>,
    viewport_order: VecDeque<SessionId>,
    durable_cap: usize,
    viewport_cap: usize,
}

impl OutboundEventQueue {
    fn new() -> Self {
        Self::with_caps(OUTBOUND_DURABLE_QUEUE_CAP, OUTBOUND_VIEWPORT_SLOT_CAP)
    }

    fn with_caps(durable_cap: usize, viewport_cap: usize) -> Self {
        Self {
            durable: VecDeque::new(),
            viewports: HashMap::new(),
            viewport_order: VecDeque::new(),
            durable_cap,
            viewport_cap,
        }
    }

    fn enqueue(&mut self, event: RuntimeEvent) -> Result<(), OutboundOverflow> {
        if let RuntimeEvent::Viewport { session, .. }
        | RuntimeEvent::ViewportTracked { session, .. } = &event
        {
            let session = *session;
            if self.viewport_cap == 0 {
                return Ok(());
            }
            if let std::collections::hash_map::Entry::Occupied(mut entry) =
                self.viewports.entry(session)
            {
                // 아직 송신 안 된 이전 스냅샷의 dirty 델타를 합친다 — 버리면 원격
                // 뷰어 renderer가 그 행들을 재shaping하지 않아 stale로 남는다
                // (in_process 슬롯과 동일 클래스, 2026-07-14 codex 리뷰).
                let prev = entry.insert(event);
                crate::event::merge_unconsumed_viewport_dirty(&prev, entry.get_mut());
                return Ok(());
            }
            while self.viewports.len() >= self.viewport_cap {
                if let Some(oldest) = self.viewport_order.pop_front() {
                    self.viewports.remove(&oldest);
                } else {
                    break;
                }
            }
            self.viewport_order.push_back(session);
            self.viewports.insert(session, event);
            Ok(())
        } else {
            if self.durable.len() >= self.durable_cap {
                return Err(OutboundOverflow::DurableFull);
            }
            self.durable.push_back(event);
            Ok(())
        }
    }

    fn pop_front(&mut self) -> Option<RuntimeEvent> {
        if let Some(event) = self.durable.pop_front() {
            return Some(event);
        }
        while let Some(session) = self.viewport_order.pop_front() {
            if let Some(event) = self.viewports.remove(&session) {
                return Some(event);
            }
        }
        None
    }

    fn is_empty(&self) -> bool {
        self.durable.is_empty() && self.viewports.is_empty()
    }

    #[cfg(test)]
    fn durable_len(&self) -> usize {
        self.durable.len()
    }

    #[cfg(test)]
    fn viewport_len(&self) -> usize {
        self.viewports.len()
    }
}

struct ExitedSessionTombstones {
    set: HashSet<SessionId>,
    order: VecDeque<SessionId>,
}

impl ExitedSessionTombstones {
    fn new() -> Self {
        Self {
            set: HashSet::new(),
            order: VecDeque::new(),
        }
    }

    fn insert(&mut self, session: SessionId) {
        if self.set.insert(session) {
            self.order.push_back(session);
            while self.order.len() > EXITED_SESSION_TOMBSTONE_CAP {
                if let Some(oldest) = self.order.pop_front() {
                    self.set.remove(&oldest);
                }
            }
        }
    }

    fn contains(&self, session: &SessionId) -> bool {
        self.set.contains(session)
    }
}

fn drain_receiver_into_outbound(
    receiver: &RuntimeEventReceiver,
    outbound: &mut OutboundEventQueue,
) -> Result<OutboundDrain, OutboundOverflow> {
    // RuntimeEventReceiver::drain과 같은 happens-before 계약: viewport slot을 먼저 take하고,
    // 상태 채널을 비운 뒤, 상태 이벤트를 먼저 enqueue한다. 이렇게 하면 Spawned/MuxUpdated가
    // 같은 tick의 Viewport보다 먼저 durable queue에 들어간다.
    let viewports: Vec<RuntimeEvent> = receiver
        .viewports
        .lock()
        .expect("remote receiver viewport slot lock")
        .drain()
        .map(|(_, event)| event)
        .collect();
    // Local-only PTY input pressure is coalesced in a receiver slot. Drain it so
    // the remote bridge cannot accumulate telemetry, but keep it off the v2 wire
    // until protocol negotiation has a feature bit for additive telemetry.
    receiver
        .input_pressures
        .lock()
        .expect("remote receiver input pressure slot lock")
        .clear();

    let mut source = OutboundDrain::Open;
    loop {
        match receiver.try_recv_durable() {
            // ResourceUsage/PtyInputPressure/SessionStatusViewChanged는 로컬 UI
            // telemetry/additive status view — 원격 피어에 보내지 않는다.
            // postcard append-only는 기존 variant discriminant를 보존할 뿐, 구버전
            // 피어가 모르는 새 variant를 디코드하게 해 주지 않는다(수신 즉시 decode
            // 실패 → 접속 종료). 버전 협상으로 게이트하기 전까지 wire에서 제외 (codex).
            Ok(
                RuntimeEvent::ResourceUsage { .. }
                | RuntimeEvent::PtyInputPressure { .. }
                | RuntimeEvent::SessionStatusViewChanged { .. },
            ) => {}
            Ok(event) => outbound.enqueue(event)?,
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => {
                source = OutboundDrain::SourceClosed;
                break;
            }
        }
    }
    for event in viewports {
        outbound.enqueue(event)?;
    }
    Ok(source)
}

fn encode_next_outbound_frame(
    outbound: &mut OutboundEventQueue,
    codec: Codec,
    last_sent: &mut HashMap<SessionId, ViewportBaseline>,
    exited_sessions: &mut ExitedSessionTombstones,
    visible_sessions: &mut Option<HashSet<SessionId>>,
) -> anyhow::Result<Option<Vec<u8>>> {
    let Some(event) = outbound.pop_front() else {
        return Ok(None);
    };
    encode_pump_frame(codec, last_sent, exited_sessions, visible_sessions, &event).map(Some)
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

/// 접속당 처리기 — 평문([`serve_connection`])과 TLS([`serve_connection_tls`])가 이 시그니처를
/// 공유한다. accept 루프/등록/self-remove 골격은 하나로 두고 여기만 갈아끼운다.
type ConnHandler =
    Arc<dyn Fn(TcpStream, Arc<InProcessRuntimeClient>, Arc<AtomicBool>) + Send + Sync>;

/// 동시 접속 상한 — 접속마다 전용 스레드(+TLS는 2ms poll 루프)가 붙으므로, 재접속
/// 폭주나 토큰 추측 시도가 스레드를 무한 증식시키지 않게 한다 (web-remote
/// MAX_CONNECTIONS 관례). 통상 attach 클라이언트는 0~1개다.
const MAX_REMOTE_CONNECTIONS: usize = 8;

/// accept 루프를 스레드로 띄운다 — 접속마다 스레드를 붙이고, 등록/self-remove를 관리한다.
/// 평문과 TLS가 이 골격을 공유하고 접속 처리 로직만 `handler`로 주입한다(§2.4 "IO 경계 1회 재편").
/// `connections` 등록과 stop 재확인을 같은 락 안에서 하므로 shutdown drain과 race 창이 없다.
fn spawn_accept(
    listener: TcpListener,
    backend: Arc<InProcessRuntimeClient>,
    stop: Arc<AtomicBool>,
    connections: Arc<Mutex<Vec<ConnEntry>>>,
    handler: ConnHandler,
) -> anyhow::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("remote-accept".into())
        .spawn(move || {
            for stream in listener.incoming() {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                match stream {
                    Ok(stream) => {
                        // fd 레벨 shutdown용 clone — TLS에서도 raw 소켓 종료로 IO 루프를 깨운다(§2.4).
                        let shutdown_clone = match stream.try_clone() {
                            Ok(s) => s,
                            Err(e) => {
                                tracing::warn!("remote stream clone 실패: {e}");
                                continue;
                            }
                        };
                        let conn_backend = Arc::clone(&backend);
                        let conn_stop = Arc::clone(&stop);
                        let conn_conns = Arc::clone(&connections);
                        let conn_handler = Arc::clone(&handler);

                        let mut conns = connections.lock().expect("connections lock");
                        if stop.load(Ordering::SeqCst) {
                            drop(conns);
                            let _ = stream.shutdown(Shutdown::Both);
                            continue;
                        }
                        if conns.len() >= MAX_REMOTE_CONNECTIONS {
                            drop(conns);
                            tracing::warn!(
                                "remote 동시 접속 상한({MAX_REMOTE_CONNECTIONS}) 초과 — 새 접속 거부"
                            );
                            let _ = stream.shutdown(Shutdown::Both);
                            continue;
                        }
                        let handle = match std::thread::Builder::new()
                            .name("remote-conn".into())
                            .spawn(move || {
                                conn_handler(stream, conn_backend, conn_stop);
                                // 접속 종료 — 자기 항목을 스스로 제거(자기 join은 데드락).
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
        .context("remote accept thread 생성 실패")
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
        // 실행마다 새 attach 토큰 (uuid v4 ×2 ≈ 244bit 엔트로피)
        let auth_token = secret::token::random_hex_token();

        // 평문 접속 처리기 — 검증된 reader+pump 2-스레드 모델을 그대로 유지한다.
        let conn_token = auth_token.clone();
        let handler: ConnHandler = Arc::new(move |stream, backend, stop| {
            serve_connection(stream, &backend, &stop, &conn_token)
        });
        let accept_thread = spawn_accept(
            listener,
            Arc::clone(&backend),
            Arc::clone(&stop),
            Arc::clone(&connections),
            handler,
        )?;

        Ok(Self {
            addr,
            stop,
            accept_thread: Some(accept_thread),
            backend: Some(backend),
            connections,
            auth_token,
        })
    }

    /// TLS 전송으로 노출한다 (단계 C-2). `addr`에 bind하고(호출자가 주소를 정한다 —
    /// 테스트는 127.0.0.1:0), 접속마다 [`rustls::ServerConnection`](ring provider,
    /// `identity`의 cert/key)을 세워 **접속당 단일 I/O 스레드**(§2.4)로 v2 핸드셰이크 +
    /// 협상 코덱(loopback은 Delta) 프로토콜을 암호화 채널 위에서 그대로 돌린다.
    ///
    /// 평문 [`Self::serve`]와 프레이밍/토큰/코덱은 **동일** — TLS는 채널 보안만 얹는다(§2.3).
    ///
    /// C-4 bind 정책(§2.5): 비-loopback bind는 `allow_non_loopback = true` **명시 opt-in**일
    /// 때만 허용한다. 원격 attach는 SpawnAgent(임의 command/args/env)를 실을 수 있어
    /// **셸 접근 부여와 동등**(§6) — 기본은 거부하고, opt-in 시에도 눈에 띄는 경고를 남긴다.
    /// 공개 인터넷이 아니라 Tailscale/WireGuard 등 신뢰 경계 인터페이스 IP에 bind할 것.
    pub fn serve_tls(
        backend: InProcessRuntimeClient,
        addr: SocketAddr,
        identity: TlsIdentity,
        allow_non_loopback: bool,
    ) -> anyhow::Result<Self> {
        if !addr.ip().is_loopback() {
            anyhow::ensure!(
                allow_non_loopback,
                "비-loopback bind({addr})는 allow_non_loopback 명시가 필요합니다 — \
                 원격 노출은 셸 접근 부여와 동등(§6). 신뢰 경계(VPN) 인터페이스에만 여세요"
            );
            tracing::warn!(
                %addr,
                "remote TLS 서버를 비-loopback에 엽니다 — 토큰 보유자는 셸 접근과 동등한 \
                 권한을 가집니다(§6). 신뢰 경계(VPN) 밖 노출 금지"
            );
        }
        let tls_config = Arc::new(build_server_config(&identity)?);
        let listener = TcpListener::bind(addr).context("remote TLS 서버 bind 실패")?;
        let addr = listener.local_addr()?;
        let backend = Arc::new(backend);
        let stop = Arc::new(AtomicBool::new(false));
        let connections: Arc<Mutex<Vec<ConnEntry>>> = Arc::default();
        // 실행마다 새 attach 토큰 (uuid v4 ×2 ≈ 244bit 엔트로피)
        let auth_token = secret::token::random_hex_token();

        let conn_token = auth_token.clone();
        let handler: ConnHandler = Arc::new(move |stream, backend, stop| {
            serve_connection_tls(
                stream,
                Arc::clone(&tls_config),
                &backend,
                &stop,
                &conn_token,
            );
        });
        let accept_thread = spawn_accept(
            listener,
            Arc::clone(&backend),
            Arc::clone(&stop),
            Arc::clone(&connections),
            handler,
        )?;

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
            let _ = stream.set_write_timeout(Some(PLAIN_EVENT_WRITE_TIMEOUT));
            let mut last_activity = std::time::Instant::now();
            // 접속별 last_sent: 세션마다 (마지막 송신 seq, 그 스냅샷). Delta diff의 기준선.
            // Plain 접속에서는 사용되지 않는다(viewport도 encode_event 경로).
            let mut last_sent: HashMap<SessionId, ViewportBaseline> = HashMap::new();
            let mut visible_sessions: Option<HashSet<SessionId>> = None;
            let mut exited_sessions = ExitedSessionTombstones::new();
            let mut outbound = OutboundEventQueue::new();
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
                match drain_receiver_into_outbound(&receiver, &mut outbound) {
                    Ok(OutboundDrain::Open) => {}
                    Ok(OutboundDrain::SourceClosed) => break,
                    Err(OutboundOverflow::DurableFull) => {
                        tracing::warn!(
                            "remote plain outbound durable queue full — slow client disconnect"
                        );
                        let _ = stream.shutdown(Shutdown::Both);
                        return;
                    }
                }
                let mut sent_event = false;
                for _ in 0..OUTBOUND_WRITE_FRAMES_PER_TICK {
                    let payload = match encode_next_outbound_frame(
                        &mut outbound,
                        codec,
                        &mut last_sent,
                        &mut exited_sessions,
                        &mut visible_sessions,
                    ) {
                        Ok(Some(payload)) => payload,
                        Ok(None) => break,
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
                } else if outbound.is_empty() && last_activity.elapsed() >= HEARTBEAT_INTERVAL {
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

/// rustls `ServerConfig` — ring provider(§2.1, aws-lc-rs 회피) + `identity`의 자기서명 cert/key.
/// provider를 명시 주입해 프로세스 전역 default provider 설치 없이 동작한다(스레드 안전).
fn build_server_config(identity: &TlsIdentity) -> anyhow::Result<rustls::ServerConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cert = CertificateDer::from(identity.cert_der.clone());
    // rcgen `serialize_der()`는 PKCS#8 DER를 준다.
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(identity.key_der.clone()));
    rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("rustls 서버 프로토콜 버전 구성 실패")?
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .context("rustls 서버 인증서 구성 실패")
}

/// TLS 접속 하나를 **접속당 단일 I/O 스레드**로 처리한다 (§2.4). 평문 [`serve_connection`]의
/// reader/pump 2-스레드와 프로토콜(v2 hello + 협상 코덱)은 동일하나, rustls 스트림을 단독
/// 소유하는 non-blocking mini 이벤트 루프로 양방향을 인터리브한다(기아/데드락 방지 — 위 모듈 주석).
fn serve_connection_tls(
    mut sock: TcpStream,
    tls_config: Arc<rustls::ServerConfig>,
    backend: &Arc<InProcessRuntimeClient>,
    stop: &Arc<AtomicBool>,
    auth_token: &str,
) {
    if sock.set_nonblocking(true).is_err() {
        return;
    }
    let mut conn: rustls::Connection = match rustls::ServerConnection::new(tls_config) {
        Ok(c) => c.into(),
        Err(e) => {
            tracing::warn!("rustls ServerConnection 생성 실패: {e}");
            return;
        }
    };
    // 나가는 평문 버퍼 상한 해제 — 큰 viewport keyframe(>64KiB)이 write_all에서 WriteZero로
    // 실패하지 않게. 메모리는 앱계층 배치-1 상한으로 관리한다(모듈 주석).
    conn.set_buffer_limit(None);

    let mut dec = FrameDecoder::new();
    // Phase 1: TLS 핸드셰이크 + ClientHello + ServerHello (AUTH/handshake timeout 안에서).
    let Some(codec) = tls_server_handshake(&mut conn, &mut sock, &mut dec, auth_token, stop) else {
        let _ = sock.shutdown(Shutdown::Both);
        return;
    };

    // Phase 2: 명령/이벤트 루프. last_sent/keyframe_requests는 이 스레드 단독 소유(락 불필요).
    let receiver = backend.subscribe();
    let mut last_sent: HashMap<SessionId, ViewportBaseline> = HashMap::new();
    let mut visible_sessions: Option<HashSet<SessionId>> = None;
    let mut exited_sessions = ExitedSessionTombstones::new();
    let mut keyframe_requests: Vec<SessionId> = Vec::new();
    let mut outbound = OutboundEventQueue::new();
    let mut heartbeat = liveness::OutboundHeartbeatTracker::new(Instant::now(), HEARTBEAT_INTERVAL);

    'main: loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let mut progressed = false;

        // (1) READ 먼저 — write 진척과 무관하게 명령을 흡수한다(기아 방지). read_tls와 프레임
        //     드레인을 **교대**해 received_plaintext 버퍼 넘침을 막고, read 단계도 tick당
        //     [`TLS_READ_STEPS_MAX`]로 상한한다(codex HIGH) — peer가 소켓을 계속 readable하게
        //     유지해도 이벤트 송신/heartbeat 단계가 굶지 않는다. 남은 수신분은 다음 tick에.
        let mut eof = false;
        let mut read_steps = 0usize;
        'read: loop {
            // 준비된 평문 프레임을 먼저 비운다(버퍼 소비).
            loop {
                match dec.advance(&mut conn.reader()) {
                    FramePoll::Frame(frame) => {
                        progressed = true;
                        if frame.is_empty() {
                            continue; // heartbeat(클라가 보낼 일은 없으나 방어)
                        }
                        match codec.decode_command(&frame) {
                            Ok(DecodedCommand::Command(command)) => {
                                if let Err(reason) = validate_command(&command) {
                                    tracing::warn!("remote 명령 검증 실패({reason}), 접속 종료");
                                    break 'main;
                                }
                                if backend.send_command(command).is_err() {
                                    break 'main; // worker 종료됨
                                }
                            }
                            Ok(DecodedCommand::RequestKeyframe(session)) => {
                                keyframe_requests.push(session);
                            }
                            Err(e) => {
                                tracing::warn!("remote 명령 파싱 실패, 접속 종료: {e}");
                                break 'main;
                            }
                        }
                    }
                    FramePoll::Partial => {
                        progressed = true;
                        continue;
                    }
                    FramePoll::Pending => break,
                    FramePoll::Closed => break 'main,
                }
            }
            // 상한 도달 — write/drain 단계로 넘어간다(progressed라 sleep 없이 다음 tick 계속).
            if read_steps >= TLS_READ_STEPS_MAX {
                progressed = true;
                break 'read;
            }
            // 그다음 TLS를 한 번 더 읽어 복호한다. 진전이 있으면 다시 드레인.
            match tls_read_step(&mut conn, &mut sock) {
                Ok(ReadStep::Progressed) => {
                    read_steps += 1;
                    continue 'read;
                }
                Ok(ReadStep::Idle) => break 'read,
                Ok(ReadStep::Eof) => {
                    eof = true;
                    break 'read;
                }
                Err(_) => break 'main,
            }
        }

        // (2) receiver를 계속 bounded outbound queue로 흡수한다. write가 막힌 동안에도
        //     runtime subscriber 채널이 무제한으로 자라지 않게 하고, durable event cap 초과는
        //     해당 slow client disconnect로 surface한다.
        for session in keyframe_requests.drain(..) {
            last_sent.remove(&session);
        }
        match drain_receiver_into_outbound(&receiver, &mut outbound) {
            Ok(OutboundDrain::Open) => {}
            Ok(OutboundDrain::SourceClosed) => break,
            Err(OutboundOverflow::DurableFull) => {
                tracing::warn!("remote TLS outbound durable queue full — slow client disconnect");
                break;
            }
        }

        // (3) bounded queue에서 한 프레임씩 rustls writer로 밀고 flush한다. 소켓 backpressure로
        //     conn에 pending TLS bytes가 남으면 추가 인코딩을 멈춰 rustls buffer 성장을 제한한다.
        if !conn.wants_write() {
            for _ in 0..OUTBOUND_WRITE_FRAMES_PER_TICK {
                let payload = match encode_next_outbound_frame(
                    &mut outbound,
                    codec,
                    &mut last_sent,
                    &mut exited_sessions,
                    &mut visible_sessions,
                ) {
                    Ok(Some(payload)) => payload,
                    Ok(None) => break,
                    Err(e) => {
                        tracing::warn!("remote event 직렬화 실패: {e}");
                        continue;
                    }
                };
                if write_frame(&mut conn.writer(), &payload).is_err() {
                    break 'main;
                }
                heartbeat.observe_frame(Instant::now());
                progressed = true;
                if tls_flush(&mut conn, &mut sock).is_err() {
                    break 'main;
                }
                if conn.wants_write() {
                    break; // 소켓이 더 못 받음 — 다음 tick에 flush부터 재시도
                }
            }
        }
        if tls_flush(&mut conn, &mut sock).is_err() {
            break;
        }

        // (4) heartbeat — 유휴가 HEARTBEAT_INTERVAL 넘고 보낼 것도 없으면 길이 0 프레임.
        if heartbeat.due(Instant::now()) && outbound.is_empty() && !conn.wants_write() {
            if write_frame(&mut conn.writer(), &[]).is_err()
                || tls_flush(&mut conn, &mut sock).is_err()
            {
                break;
            }
            heartbeat.observe_frame(Instant::now());
        }

        if eof {
            break;
        }
        if tls_idle_sleep_needed(progressed) {
            std::thread::sleep(TLS_IDLE_SLEEP);
        }
    }

    let _ = conn.write_tls(&mut sock); // best-effort: 남은 alert/데이터 flush
    let _ = sock.shutdown(Shutdown::Both);
}

/// 서버 TLS 핸드셰이크: TLS record 핸드셰이크 완료 → ClientHello 프레임 수신(토큰 검증) →
/// ServerHello 회신. [`TLS_HANDSHAKE_TIMEOUT`] 안에 못 끝내거나 매직/버전/토큰 불일치면 `None`.
/// non-blocking 루프라 진전이 없을 때만 짧게 잔다(busy-spin 방지).
fn tls_server_handshake(
    conn: &mut rustls::Connection,
    sock: &mut TcpStream,
    dec: &mut FrameDecoder,
    auth_token: &str,
    stop: &Arc<AtomicBool>,
) -> Option<Codec> {
    let deadline = Instant::now() + TLS_HANDSHAKE_TIMEOUT;
    // ClientHello 프레임을 받을 때까지 TLS I/O 구동.
    let hello_frame = loop {
        if stop.load(Ordering::SeqCst) || Instant::now() >= deadline {
            return None;
        }
        if tls_flush(conn, sock).is_err() {
            return None;
        }
        match tls_pump_read(conn, sock) {
            Ok(PumpRead::Ok) => {}
            Ok(PumpRead::Eof) | Err(_) => return None,
        }
        match dec.advance(&mut conn.reader()) {
            FramePoll::Frame(frame) if !frame.is_empty() => break frame,
            FramePoll::Frame(_) => {} // 길이 0(있을 리 없지만) 무시
            FramePoll::Partial => {}
            FramePoll::Pending => std::thread::sleep(TLS_IDLE_SLEEP),
            FramePoll::Closed => return None,
        }
    };

    let hello = match postcard::from_bytes::<ClientHello>(&hello_frame) {
        Ok(hello) if client_hello_matches_protocol(&hello) => hello,
        _ => {
            tracing::warn!("remote TLS 핸드셰이크 실패(매직/버전/기형) — 접속 거부");
            return None;
        }
    };
    if !token_matches(auth_token, &hello.token) {
        tracing::warn!("remote TLS 인증 실패 — 접속 거부");
        return None;
    }
    let features = SERVER_FEATURES & hello.features;
    let server_hello = ServerHello {
        proto_version: PROTO_VERSION,
        features,
    };
    let payload = postcard::to_allocvec(&server_hello).ok()?;
    if write_frame(&mut conn.writer(), &payload).is_err() {
        return None;
    }
    // ServerHello가 소켓으로 완전히 나갈 때까지 flush.
    loop {
        if Instant::now() >= deadline {
            return None;
        }
        if tls_flush(conn, sock).is_err() {
            return None;
        }
        if !conn.wants_write() {
            break;
        }
        std::thread::sleep(TLS_IDLE_SLEEP);
    }
    Some(Codec::from_features(features))
}

/// pump가 한 이벤트를 접속 코덱으로 프레임 payload로 만든다.
/// Delta 접속의 viewport만 keyframe/delta 특수 처리(last_sent 갱신)하고, 나머지는
/// 기존 [`Codec::encode_event`] 경로 그대로 — Plain은 이 경로에서도 바이트 동일.
///
/// `SessionExited`는 Delta 접속에서 그 세션의 baseline을 [`HashMap::remove`]하고
/// bounded tombstone에 등록한다 — 장기 연결에서 종료된 세션의 snapshot Arc가 last_sent에
/// 누적되지 않게 (codex P2). 이미 종료된 세션의 trailing Viewport는 keyframe/delta가 아니라
/// plain full(`WireMsg::Event`)로 보내 baseline을 되살리지 않는다 — 최종 출력은 여전히 클라
/// slot에 전달된다.
///
/// `MuxUpdated`는 active tab visible session set을 연결 상태로 갱신하고, set 밖 baseline을
/// 즉시 prune한다. 같은 drain 배치에서 뒤따르는 hidden `Viewport`도 plain full로만 보내
/// `last_sent`가 hidden snapshot Arc를 다시 잡지 않게 한다.
fn encode_pump_frame(
    codec: Codec,
    last_sent: &mut HashMap<SessionId, ViewportBaseline>,
    exited_sessions: &mut ExitedSessionTombstones,
    visible_sessions: &mut Option<HashSet<SessionId>>,
    event: &RuntimeEvent,
) -> anyhow::Result<Vec<u8>> {
    match (codec, event) {
        (Codec::Delta, RuntimeEvent::MuxUpdated { snapshot }) => {
            let visible = visible_session_set(snapshot);
            last_sent.retain(|session, _| visible.contains(session));
            *visible_sessions = Some(visible);
            codec.encode_event(event)
        }
        (
            Codec::Delta,
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
            },
        ) => {
            if exited_sessions.contains(session)
                || visible_sessions
                    .as_ref()
                    .is_some_and(|visible| !visible.contains(session))
            {
                // 종료됐거나 현재 active tab 밖인 세션 — baseline을 만들지 않고 전체 스냅샷을 그대로.
                codec.encode_event(event)
            } else {
                match event.viewport().and_then(|(_, _, _, stamp)| stamp) {
                    Some(stamp) => encode_viewport_frame_stamped(
                        last_sent,
                        *session,
                        snapshot,
                        *bracketed_paste,
                        Some(stamp),
                    ),
                    None => encode_viewport_frame(last_sent, *session, snapshot, *bracketed_paste),
                }
            }
        }
        (Codec::Delta, RuntimeEvent::SessionExited { session, .. }) => {
            exited_sessions.insert(*session);
            last_sent.remove(session);
            codec.encode_event(event)
        }
        _ => codec.encode_event(event),
    }
}

fn visible_session_set(snapshot: &MuxSnapshot) -> HashSet<SessionId> {
    snapshot
        .active_tab
        .as_ref()
        .and_then(|active| snapshot.tabs.iter().find(|tab| &tab.id == active))
        .into_iter()
        .flat_map(|tab| &tab.panes)
        .filter_map(|pane| pane.session_id)
        .collect()
}

/// Delta 접속에서 세션 viewport를 last_sent 대비 keyframe/delta로 인코딩하고 baseline을
/// 갱신한다 (§4.4/§4.7). keyframe 조건: baseline 없음(신규/재구독/RequestKeyframe로 제거됨),
/// 또는 diff가 폴백(차원·alt-screen 변경/heavy repaint)을 반환. seq는 (접속,세션)마다 단조 증가.
fn encode_viewport_frame(
    last_sent: &mut HashMap<SessionId, ViewportBaseline>,
    session: SessionId,
    snapshot: &Arc<TerminalViewportSnapshot>,
    bracketed_paste: bool,
) -> anyhow::Result<Vec<u8>> {
    encode_viewport_frame_stamped(last_sent, session, snapshot, bracketed_paste, None)
}

fn encode_viewport_frame_stamped(
    last_sent: &mut HashMap<SessionId, ViewportBaseline>,
    session: SessionId,
    snapshot: &Arc<TerminalViewportSnapshot>,
    bracketed_paste: bool,
    stamp: Option<crate::ResizeStamp>,
) -> anyhow::Result<Vec<u8>> {
    let (wire, new_seq) = match last_sent.get(&session) {
        Some((prev_seq, prev_snap, prev_stamp)) => {
            let seq = prev_seq
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("viewport sequence 소진"))?;
            match (*prev_stamp == stamp)
                .then(|| diff_viewport(prev_snap, snapshot))
                .flatten()
            {
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
    let wire = match (stamp, wire) {
        (
            Some(stamp),
            WireMsg::ViewportKeyframe {
                session,
                seq,
                snapshot,
                bracketed_paste,
            },
        ) => WireMsg::ViewportKeyframeTracked {
            session,
            seq,
            snapshot,
            bracketed_paste,
            stamp,
        },
        (
            Some(stamp),
            WireMsg::ViewportDelta {
                session,
                seq,
                base_seq,
                delta,
                bracketed_paste,
            },
        ) => WireMsg::ViewportDeltaTracked {
            session,
            seq,
            base_seq,
            delta,
            bracketed_paste,
            stamp,
        },
        (_, wire) => wire,
    };
    let payload = encode_wire_msg(&wire)?;
    last_sent.insert(session, (new_seq, Arc::clone(snapshot), stamp));
    Ok(payload)
}

/// RemoteRuntimeServer에 attach하는 클라이언트.
/// InProcessRuntimeClient와 같은 trait(RuntimeCommandSink/RuntimeEventStream)을
/// 구현한다 — UI 입장에서 교체 가능 (완료 기준).
pub struct RemoteRuntimeClient {
    /// 명령 송신 경로 — 평문은 소켓 직접 write, TLS는 IO 스레드로 채널 enqueue.
    transport: ClientTransport,
    subscribers: Arc<Mutex<Vec<RemoteSubscriber>>>,
    connected: Arc<AtomicBool>,
    /// 평문은 reader 스레드, TLS는 단일 IO 스레드.
    reader_thread: Option<JoinHandle<()>>,
    /// 핸드셰이크에서 협상된 접속 코덱 (§3.2). loopback은 Delta.
    codec: Codec,
}

/// 클라이언트 명령 송신 경로. 평문은 검증된 소켓 직접 write(2-스레드), TLS는 rustls 스트림을
/// 단독 소유한 IO 스레드로 인코딩된 프레임을 채널 enqueue(§2.4 클라이언트도 단일 IO 필요).
enum ClientTransport {
    /// 명령을 소켓에 직접 write. reader 스레드도 seq gap 시 같은 뮤텍스로 RequestKeyframe를 보낸다.
    Plain(Arc<Mutex<TcpStream>>),
    /// 인코딩된 명령 프레임을 IO 스레드로 보낸다. **유계 sync_channel**([`TLS_CMD_QUEUE_CAP`]) —
    /// 큐가 가득 차면 try_send가 즉시 Err(backpressure surface, 블록/silent drop 없음),
    /// IO 스레드가 죽어 Receiver가 drop되면 Disconnected Err — 그 죽음을 다음 호출에
    /// surface한다(silent drop 금지, 설계 요구 #3).
    Tls {
        commands: std::sync::mpsc::SyncSender<Vec<u8>>,
        /// fd 레벨 shutdown용 raw 소켓 clone — Drop이 IO 스레드를 깨운다.
        shutdown: TcpStream,
    },
}

struct RemoteSubscriber {
    events: SyncSender<RuntimeEvent>,
    overflowed: Arc<AtomicBool>,
    viewports: Arc<Mutex<std::collections::HashMap<SessionId, RuntimeEvent>>>,
    input_pressures: Arc<Mutex<std::collections::HashMap<SessionId, RuntimeEvent>>>,
}

impl RemoteRuntimeClient {
    /// loopback 주소에만 attach한다 (완료 기준: localhost-only attach).
    /// `token`은 서버의 [`RemoteRuntimeServer::auth_token`] — 첫 프레임으로 제시한다.
    pub fn attach(addr: SocketAddr, token: &str) -> anyhow::Result<Self> {
        if !addr.ip().is_loopback() {
            bail!("remote attach는 localhost만 허용합니다 (public remote는 v1+): {addr}");
        }
        let mut stream = connect_with(addr, TcpStream::connect_timeout)
            .with_context(|| format!("remote 서버 연결 실패: {addr}"))?;
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
        stream
            .set_write_timeout(Some(PLAIN_CMD_WRITE_TIMEOUT))
            .context("remote plain 명령 write timeout 설정 실패")?;
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
        let connected = Arc::new(AtomicBool::new(true));
        let writer = Arc::new(Mutex::new(stream));

        let reader_subscribers = Arc::clone(&subscribers);
        let reader_connected = Arc::clone(&connected);
        let reader_writer = Arc::clone(&writer);
        let reader_stream = writer
            .lock()
            .expect("remote writer lock")
            .try_clone()
            .context("remote stream clone 실패")?;
        reader_stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .context("remote plain reader timeout 설정 실패")?;
        let reader_thread = std::thread::Builder::new()
            .name("remote-events".into())
            .spawn(move || {
                let mut reader = reader_stream;
                let mut decoder = FrameDecoder::new();
                let mut liveness = liveness::LivenessTracker::new(
                    Instant::now(),
                    liveness::CLIENT_LIVENESS_TIMEOUT,
                );
                // 접속별 재구성 상태 (§4.3): 세션마다 (마지막 적용 seq, 현재 재구성본).
                // reader가 TCP를 UI 소비와 무관하게 완전히 드레인하므로 delta는 여기서 유실되지 않고,
                // slot에는 항상 "재구성된 전체 스냅샷"만 담긴다 — UI 계약(전체 Viewport)은 불변.
                let mut recon: HashMap<SessionId, ViewportBaseline> = HashMap::new();
                // seq gap으로 keyframe을 이미 요청한 세션 — keyframe 도착 전까지 delta를 조용히 버려
                // RequestKeyframe 폭주를 막는다.
                let mut pending_keyframe: HashSet<SessionId> = HashSet::new();
                loop {
                    match decoder.advance(&mut reader) {
                        FramePoll::Frame(frame) => {
                            observe_client_frame_for_liveness(
                                &mut liveness,
                                &frame,
                                Instant::now(),
                            );
                            if !frame.is_empty() {
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
                        }
                        FramePoll::Partial => {}
                        FramePoll::Pending => {}
                        FramePoll::Closed => break,
                    }
                    if client_liveness_expired(&liveness, Instant::now()) {
                        tracing::warn!(
                            "remote client liveness timeout — no frames received, 접속 종료"
                        );
                        break;
                    }
                }
                // 수신이 죽은 클라이언트가 명령 전송만 성공하는 반쪽 상태 방지 —
                // 소켓을 양방향으로 닫아 이후 send_command도 실패하게 한다 (codex 리뷰)
                let _ = reader.shutdown(Shutdown::Both);
                let mut subscribers = reader_subscribers.lock().expect("remote subscribers lock");
                reader_connected.store(false, Ordering::Release);
                subscribers.clear();
            })
            .context("remote reader thread 생성 실패")?;

        Ok(Self {
            transport: ClientTransport::Plain(writer),
            subscribers,
            connected,
            reader_thread: Some(reader_thread),
            codec,
        })
    }

    /// TLS 서버에 attach한다 (단계 C-2 + C-3 core). `expected_fingerprint`는 서버 인증서의
    /// SHA-256 지문("ab:cd:…", [`TlsIdentity::fingerprint`]) — 커스텀 검증기가 CA/hostname/만료
    /// 대신 **지문 일치만** 확인한다(TOFU/SSH 모델, §2.2 · Open Question 2). 지문 불일치 →
    /// TLS 핸드셰이크 실패 → attach Err. 잘못된 토큰 → ServerHello 없음 → attach Err.
    ///
    /// 이후 IO는 rustls 스트림을 단독 소유하는 **단일 스레드**(§2.4)가 담당한다:
    /// 이벤트는 구독자 slot으로 재구성 dispatch, `RequestKeyframe` 재동기화 경로 보존,
    /// heartbeat(길이 0 프레임) 소비. `known_hosts` 저장/UX는 후속(C-3 나머지) — 호출자가 지문을 준다.
    pub fn attach_tls(
        addr: SocketAddr,
        token: &str,
        expected_fingerprint: &str,
    ) -> anyhow::Result<Self> {
        Self::attach_tls_inner(addr, token, Some(expected_fingerprint)).map(|(client, _)| client)
    }

    /// known_hosts 기반 TOFU attach (단계 C-3 잔여).
    ///
    /// - 저장된 핀이 있으면 그 지문으로 검증 attach → `TofuOutcome::Verified`.
    ///   지문이 바뀌었으면 handshake가 실패하고 에러에 관찰 지문 + forget 안내가 담긴다.
    /// - 항목이 없으면(first-use) **캡처 모드**로 접속해 관찰 지문을 즉시 핀한다 →
    ///   `TofuOutcome::Pinned { fingerprint }`. **최초 접속은 무검증 창**(SSH TOFU와 동일
    ///   한계, §6) — 반환된 지문을 사용자에게 보여주고 대역외 대조를 요구하는 것은 앱 UX 소관.
    pub fn attach_tls_tofu(
        addr: SocketAddr,
        token: &str,
        known_hosts: &mut crate::known_hosts::KnownHosts,
    ) -> anyhow::Result<(Self, TofuOutcome)> {
        let host = addr.to_string();
        match known_hosts.lookup(&host).map(|s| s.to_owned()) {
            Some(pinned) => {
                let (client, _) = Self::attach_tls_inner(addr, token, Some(&pinned))?;
                Ok((client, TofuOutcome::Verified))
            }
            None => {
                let (client, observed) = Self::attach_tls_inner(addr, token, None)?;
                let fingerprint =
                    observed.context("TLS handshake가 끝났는데 관찰 지문이 없음 (내부 오류)")?;
                known_hosts.pin(&host, &fingerprint)?;
                Ok((client, TofuOutcome::Pinned { fingerprint }))
            }
        }
    }

    /// 공통 코어: expected가 Some이면 핀 검증, None이면 캡처 모드(first-use TOFU).
    /// 성공 시 (클라이언트, 관찰 지문)을 돌려준다.
    fn attach_tls_inner(
        addr: SocketAddr,
        token: &str,
        expected_fingerprint: Option<&str>,
    ) -> anyhow::Result<(Self, Option<String>)> {
        let (config, observed) = build_client_config(expected_fingerprint)?;
        let config = Arc::new(config);
        // ServerName은 검증기가 무시한다(TOFU). SNI/참조용 고정 더미(cert SAN과 동일).
        let server_name = ServerName::try_from("deppy-remote").expect("정적 server name");
        let mut conn: rustls::Connection = rustls::ClientConnection::new(config, server_name)
            .context("rustls ClientConnection 생성 실패")?
            .into();
        conn.set_buffer_limit(None);
        let mut sock = connect_with(addr, TcpStream::connect_timeout)
            .with_context(|| format!("remote TLS 연결 실패: {addr}"))?;
        sock.set_nonblocking(true)
            .context("remote TLS non-blocking 설정 실패")?;

        // 핸드셰이크 + hello를 호출 스레드에서 동기로 끝내 인증/지문 거부를 Err로 즉시 안다.
        let mut dec = FrameDecoder::new();
        let codec = tls_client_handshake(&mut conn, &mut sock, &mut dec, token)?;

        let subscribers: Arc<Mutex<Vec<RemoteSubscriber>>> = Arc::default();
        let connected = Arc::new(AtomicBool::new(true));
        // 유계 채널 — 명령 outbound backpressure를 채널 용량으로 상한한다 (codex HIGH).
        let (cmd_tx, cmd_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(TLS_CMD_QUEUE_CAP);
        let shutdown = sock.try_clone().context("remote TLS shutdown clone 실패")?;
        let io_subscribers = Arc::clone(&subscribers);
        let io_connected = Arc::clone(&connected);
        let reader_thread = std::thread::Builder::new()
            .name("remote-tls-io".into())
            .spawn(move || {
                client_tls_io_loop(conn, sock, dec, codec, io_subscribers, io_connected, cmd_rx);
            })
            .context("remote TLS IO thread 생성 실패")?;

        let observed_fp = observed.lock().expect("observed lock").clone();
        Ok((
            Self {
                transport: ClientTransport::Tls {
                    commands: cmd_tx,
                    shutdown,
                },
                subscribers,
                connected,
                reader_thread: Some(reader_thread),
                codec,
            },
            observed_fp,
        ))
    }
}

/// attach_tls_tofu 결과 — 호출측 UX가 분기한다 (first-use면 지문 대역외 확인 안내).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TofuOutcome {
    /// first-use: 관찰 지문을 새로 핀했다 — 사용자에게 보여주고 대조를 요구할 것.
    Pinned { fingerprint: String },
    /// 저장된 핀과 일치 확인.
    Verified,
}

/// rustls `ClientConfig` — ring provider + 지문 핀닝 검증기(C-3 core). CA/hostname/만료를 무시하고
/// SHA-256(cert DER) 지문 일치만으로 서버를 신뢰한다(TOFU).
fn build_client_config(
    expected_fingerprint: Option<&str>,
) -> anyhow::Result<(rustls::ClientConfig, Arc<Mutex<Option<String>>>)> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let observed: Arc<Mutex<Option<String>>> = Arc::default();
    let verifier = Arc::new(FingerprintVerifier {
        expected: expected_fingerprint.map(str::to_ascii_lowercase),
        observed: Arc::clone(&observed),
        supported: provider.signature_verification_algorithms,
    });
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("rustls 클라이언트 프로토콜 버전 구성 실패")?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    Ok((config, observed))
}

/// TOFU 지문 핀닝 검증기 (C-3 core, 설계 §2.2). 표준 PKI(CA 체인·hostname·만료) 대신
/// **SHA-256(end-entity cert DER) == 핀 지문**만 확인한다. handshake 서명은 여전히 검증해
/// 핀된 cert의 키를 실제로 보유한 peer만 통과시킨다(핀만 흉내낸 MITM 차단).
#[derive(Debug)]
struct FingerprintVerifier {
    /// 소문자 정규화된 기대 지문("ab:cd:…"). None = **캡처 모드**(first-use TOFU) —
    /// 지문 대조 없이 통과시키되 관찰 지문만 기록한다. 캡처 모드 접속은 무검증 창이므로
    /// attach_tls_tofu의 first-use 경로에서만 쓰고, 성공 시 즉시 pin한다.
    expected: Option<String>,
    /// handshake에서 관찰한 서버 cert 지문 — Mismatch 오류 표면화·first-use pin에 사용.
    observed: Arc<Mutex<Option<String>>>,
    supported: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for FingerprintVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        // CA/hostname/만료는 무시(TOFU) — 지문만 대조.
        let got = crate::tls_identity::fingerprint(end_entity.as_ref());
        *self.observed.lock().expect("observed lock") = Some(got.clone());
        match &self.expected {
            // 캡처 모드(first-use TOFU): 통과 — 호출측이 관찰 지문을 pin한다.
            None => Ok(ServerCertVerified::assertion()),
            Some(expected) if fingerprint_eq(got.as_bytes(), expected.as_bytes()) => {
                Ok(ServerCertVerified::assertion())
            }
            Some(_) => Err(rustls::Error::General(format!(
                "TLS 서버 인증서 지문 불일치 (TOFU 핀 실패) — 관찰 지문: {got}. \
                 서버가 바뀌었거나 중간자일 수 있습니다; 의도한 변경이면 known_hosts에서 \
                 이 host를 제거(forget) 후 재접속해 다시 핀하세요"
            ))),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.supported)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.supported)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.supported.supported_schemes()
    }
}

/// 지문 상수시간-ish 비교 — 대소문자는 호출측이 이미 정규화(양쪽 소문자). 길이 다르면 거부.
fn fingerprint_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// 클라이언트 TLS 핸드셰이크: ClientHello 프레임을 버퍼링하고 TLS record 핸드셰이크를 완료시킨 뒤
/// ServerHello 프레임을 받아 협상 코덱을 확정한다. 지문 불일치(검증기 Err)나 토큰 거부(EOF)는
/// `Err`로 올라와 attach가 실패한다. [`TLS_HANDSHAKE_TIMEOUT`] 초과도 Err.
fn tls_client_handshake(
    conn: &mut rustls::Connection,
    sock: &mut TcpStream,
    dec: &mut FrameDecoder,
    token: &str,
) -> anyhow::Result<Codec> {
    let hello = ClientHello {
        magic: PROTO_MAGIC,
        proto_version: PROTO_VERSION,
        features: CLIENT_FEATURES,
        token: token.as_bytes().to_vec(),
    };
    let payload = postcard::to_allocvec(&hello).context("remote ClientHello 직렬화 실패")?;
    // 핸드셰이크 완료 전이라도 writer는 평문을 버퍼링하고 traffic key 준비 후 전송한다.
    write_frame(&mut conn.writer(), &payload).context("remote ClientHello 버퍼링 실패")?;

    let deadline = Instant::now() + TLS_HANDSHAKE_TIMEOUT;
    loop {
        if Instant::now() >= deadline {
            bail!("remote TLS 핸드셰이크 시간 초과");
        }
        tls_flush(conn, sock).context("remote TLS 쓰기 실패")?;
        // 지문 불일치는 process_new_packets가 여기서 Err를 돌려준다(검증기 거부).
        match tls_pump_read(conn, sock)
            .context("remote TLS 핸드셰이크 실패(지문/인증서 거부 가능)")?
        {
            PumpRead::Ok => {}
            PumpRead::Eof => bail!("remote TLS 인증 거부 — 토큰/지문을 확인하세요"),
        }
        match dec.advance(&mut conn.reader()) {
            FramePoll::Frame(frame) if frame.is_empty() => {} // heartbeat 무시
            FramePoll::Frame(frame) => {
                let server_hello: ServerHello =
                    postcard::from_bytes(&frame).context("remote ServerHello 디코드 실패")?;
                if server_hello.proto_version != PROTO_VERSION {
                    bail!("remote 프로토콜 버전 불일치");
                }
                // 협상 불변식: agreed ⊆ 요청(CLIENT_FEATURES) — 평문 attach와 동일 방어.
                if server_hello.features & !CLIENT_FEATURES != 0 {
                    bail!("remote 서버가 미요청 feature를 ack — 협상 불변식 위반, 접속 거부");
                }
                return Ok(Codec::from_features(server_hello.features));
            }
            FramePoll::Partial => {}
            FramePoll::Pending => std::thread::sleep(TLS_IDLE_SLEEP),
            FramePoll::Closed => bail!("remote TLS 인증 거부 — 접속 종료"),
        }
    }
}

/// 클라이언트 TLS 단일 IO 루프 (§2.4): rustls 스트림 단독 소유. 수신 이벤트는 재구성 dispatch,
/// 송신 명령(+gap 시 RequestKeyframe)은 채널에서 받아 소켓이 받는 만큼 흘려보낸다. read를 write
/// 진척과 무관하게 매 tick 수행해 이벤트 수신이 명령 backpressure에 굶지 않는다.
fn client_tls_io_loop(
    mut conn: rustls::Connection,
    mut sock: TcpStream,
    mut dec: FrameDecoder,
    codec: Codec,
    subscribers: Arc<Mutex<Vec<RemoteSubscriber>>>,
    connected: Arc<AtomicBool>,
    commands: std::sync::mpsc::Receiver<Vec<u8>>,
) {
    let mut recon: HashMap<SessionId, ViewportBaseline> = HashMap::new();
    let mut pending_keyframe: HashSet<SessionId> = HashSet::new();
    let mut out: VecDeque<Vec<u8>> = VecDeque::new();
    let mut liveness =
        liveness::LivenessTracker::new(Instant::now(), liveness::CLIENT_LIVENESS_TIMEOUT);

    'main: loop {
        let mut progressed = false;

        // (1) READ 먼저 — 명령 송신 backpressure와 무관하게 이벤트를 흡수한다. read_tls와 프레임
        //     드레인을 교대해 received_plaintext 버퍼 넘침을 막고, read 단계도 tick당
        //     [`TLS_READ_STEPS_MAX`]로 상한한다(codex HIGH) — 고출력 서버 스트림이 소켓을 계속
        //     readable하게 유지해도 명령 drain/write 단계(Ctrl-C/Resize/RequestKeyframe)가
        //     굶지 않는다. 남은 수신분은 다음 tick에.
        let mut eof = false;
        let mut read_steps = 0usize;
        'read: loop {
            loop {
                match dec.advance(&mut conn.reader()) {
                    FramePoll::Frame(frame) => {
                        progressed = true;
                        observe_client_frame_for_liveness(&mut liveness, &frame, Instant::now());
                        if frame.is_empty() {
                            continue; // heartbeat 소비
                        }
                        let Ok(decoded) = codec.decode_event(&frame) else {
                            tracing::warn!("remote 이벤트 프로토콜 위반 — 접속 종료");
                            break 'main;
                        };
                        let outcome = reconstruct_and_dispatch(
                            decoded,
                            &subscribers,
                            &mut recon,
                            &mut pending_keyframe,
                        );
                        if outcome.disconnect {
                            break 'main;
                        }
                        if let Some(session) = outcome.request_keyframe {
                            match encode_request_keyframe(session) {
                                Ok(p) => out.push_back(p),
                                Err(e) => tracing::warn!("remote RequestKeyframe 직렬화 실패: {e}"),
                            }
                        }
                    }
                    FramePoll::Partial => {
                        progressed = true;
                        if client_liveness_expired(&liveness, Instant::now()) {
                            tracing::warn!(
                                "remote client liveness timeout — no frames received, 접속 종료"
                            );
                            break 'main;
                        }
                        continue;
                    }
                    FramePoll::Pending => {
                        if client_liveness_expired(&liveness, Instant::now()) {
                            tracing::warn!(
                                "remote client liveness timeout — no frames received, 접속 종료"
                            );
                            break 'main;
                        }
                        break;
                    }
                    FramePoll::Closed => break 'main,
                }
            }
            // 상한 도달 — 명령 drain/write 단계로 넘어간다(progressed라 sleep 없이 다음 tick 계속).
            if read_steps >= TLS_READ_STEPS_MAX {
                progressed = true;
                break 'read;
            }
            match tls_read_step(&mut conn, &mut sock) {
                Ok(ReadStep::Progressed) => {
                    read_steps += 1;
                    continue 'read;
                }
                Ok(ReadStep::Idle) => break 'read,
                Ok(ReadStep::Eof) => {
                    eof = true;
                    break 'read;
                }
                Err(_) => break 'main,
            }
        }

        if client_liveness_expired(&liveness, Instant::now()) {
            tracing::warn!("remote client liveness timeout — no frames received, 접속 종료");
            break 'main;
        }

        // (2) 송신 명령 흡수 — 서버 outbound와 같은 원칙(codex HIGH): 직전 배치를 소켓에 다
        //     흘려보냈을 때만(out 빔 + 소켓 미포화) drain하고, tick당 상한으로 cap해 fast
        //     producer가 루프를 붙잡지 못하게 한다. 백로그/포화 중엔 drain하지 않는다 —
        //     명령은 유계 sync_channel에서 자연 대기(순서 보존, 코얼레싱 없음).
        //     Receiver Disconnected = 클라이언트 drop.
        if out.is_empty() && !conn.wants_write() {
            for _ in 0..TLS_CMD_DRAIN_MAX {
                match commands.try_recv() {
                    Ok(frame) => out.push_back(frame),
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => break 'main,
                }
            }
        }

        // (3) out을 한 프레임씩 밀고 flush — 소켓 backpressure면 나머지는 out에 남긴다.
        while let Some(front) = out.front() {
            if write_frame(&mut conn.writer(), front).is_err() {
                break 'main;
            }
            out.pop_front();
            progressed = true;
            if tls_flush(&mut conn, &mut sock).is_err() {
                break 'main;
            }
            if conn.wants_write() {
                break;
            }
        }
        if tls_flush(&mut conn, &mut sock).is_err() {
            break;
        }

        if eof {
            break;
        }
        if tls_idle_sleep_needed(progressed) {
            std::thread::sleep(TLS_IDLE_SLEEP);
        }
    }

    publish_tls_disconnect(commands, subscribers, connected);
    let _ = conn.write_tls(&mut sock);
    let _ = sock.shutdown(Shutdown::Both);
}

fn publish_tls_disconnect(
    commands: std::sync::mpsc::Receiver<Vec<u8>>,
    subscribers: Arc<Mutex<Vec<RemoteSubscriber>>>,
    connected: Arc<AtomicBool>,
) {
    drop(commands);
    let mut subscribers = subscribers.lock().expect("remote subscribers lock");
    connected.store(false, Ordering::Release);
    subscribers.clear();
}

/// [`reconstruct_and_dispatch`] 결과 — 접속 종료 여부 + keyframe 재동기화 요청 세션.
/// transport(평문 소켓 / TLS 큐)에 독립적이라 두 경로가 재구성 로직을 공유한다.
struct ReconOutcome {
    /// 검증 실패/프로토콜 위반 → 접속 종료.
    disconnect: bool,
    /// gap/기형 delta로 keyframe을 요청해야 하는 세션(세션당 1회, 폭주 방지 후).
    request_keyframe: Option<SessionId>,
}

/// 디코드된 이벤트 하나를 재구성해 항상 전체 Viewport를 slot에 dispatch한다 (§4.3/§4.4).
/// transport 무관 코어 — 평문([`handle_decoded_event`])과 TLS IO 루프가 공유한다.
/// keyframe 요청은 여기서 보내지 않고 [`ReconOutcome`]로 돌려 호출측이 자기 transport로 보낸다.
fn attach_viewport_stamp(event: RuntimeEvent, stamp: Option<crate::ResizeStamp>) -> RuntimeEvent {
    match (event, stamp) {
        (
            RuntimeEvent::Viewport {
                session,
                snapshot,
                bracketed_paste,
            },
            Some(stamp),
        ) => RuntimeEvent::ViewportTracked {
            session,
            snapshot,
            bracketed_paste,
            stamp,
        },
        (event, _) => event,
    }
}

fn reconstruct_and_dispatch(
    decoded: DecodedEvent,
    subscribers: &Arc<Mutex<Vec<RemoteSubscriber>>>,
    recon: &mut HashMap<SessionId, ViewportBaseline>,
    pending_keyframe: &mut HashSet<SessionId>,
) -> ReconOutcome {
    let cont = ReconOutcome {
        disconnect: false,
        request_keyframe: None,
    };
    let disconnect = ReconOutcome {
        disconnect: true,
        request_keyframe: None,
    };
    let (decoded, stamp) = match decoded {
        DecodedEvent::KeyframeTracked {
            session,
            seq,
            snapshot,
            bracketed_paste,
            stamp,
        } => (
            DecodedEvent::Keyframe {
                session,
                seq,
                snapshot,
                bracketed_paste,
            },
            Some(stamp),
        ),
        DecodedEvent::DeltaTracked {
            session,
            seq,
            base_seq,
            delta,
            bracketed_paste,
            stamp,
        } => (
            DecodedEvent::Delta {
                session,
                seq,
                base_seq,
                delta,
                bracketed_paste,
            },
            Some(stamp),
        ),
        decoded => (decoded, None),
    };
    match decoded {
        DecodedEvent::KeyframeTracked { .. } | DecodedEvent::DeltaTracked { .. } => {
            unreachable!("위에서 정규화")
        }
        DecodedEvent::Event(event) => {
            if let Err(reason) = validate_event(&event) {
                tracing::warn!("remote 이벤트 검증 실패({reason}) — 접속 종료");
                return disconnect;
            }
            // 종료된 세션의 재구성 상태를 정리한다 — 장기 연결에서 baseline Arc/pending이
            // 누적되지 않게 (codex P2). Viewport slot은 receiver.drain()이 프레임마다 비운다.
            match &event {
                RuntimeEvent::SessionExited { session, .. } => {
                    recon.remove(session);
                    pending_keyframe.remove(session);
                }
                RuntimeEvent::MuxUpdated { snapshot } => {
                    let visible = visible_session_set(snapshot);
                    recon.retain(|session, _| visible.contains(session));
                    pending_keyframe.retain(|session| visible.contains(session));
                }
                _ => {}
            }
            dispatch(subscribers, event);
            cont
        }
        DecodedEvent::Keyframe {
            session,
            seq,
            snapshot,
            bracketed_paste,
        } => {
            // 전체 기준선 — recon을 세팅하고 전체 Viewport로 emit. dirty_ranges는
            // **full로 승격**한다: keyframe의 dirty_ranges는 서버의 마지막 로컬
            // take_snapshot 기준이라 클라이언트 렌더 캐시의 기준선과 무관하다 —
            // 그대로 흘리면 UI row cache가 stale 행을 유지할 수 있다 (codex High:
            // slow-client coalesce/keyframe 폴백/RequestKeyframe 복구 경로).
            let snapshot = {
                let mut full = (*snapshot).clone();
                let cells = full.cols as usize * full.rows as usize;
                full.dirty_ranges = vec![terminal::CellRange {
                    start: 0,
                    end: cells,
                }];
                Arc::new(full)
            };
            let event = RuntimeEvent::Viewport {
                session,
                snapshot: Arc::clone(&snapshot),
                bracketed_paste,
            };
            let event = attach_viewport_stamp(event, stamp);
            if let Err(reason) = validate_event(&event) {
                tracing::warn!("remote keyframe 검증 실패({reason}) — 접속 종료");
                return disconnect;
            }
            recon.insert(session, (seq, snapshot, stamp));
            pending_keyframe.remove(&session);
            dispatch(subscribers, event);
            cont
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
                Some((cur_seq, snap, base_stamp))
                    if *cur_seq == base_seq && *base_stamp == stamp =>
                {
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
                    let event = attach_viewport_stamp(event, stamp);
                    if let Err(reason) = validate_event(&event) {
                        tracing::warn!("remote delta 재구성 검증 실패({reason}) — 접속 종료");
                        return disconnect;
                    }
                    recon.insert(session, (seq, reconstructed, stamp));
                    dispatch(subscribers, event);
                    cont
                }
                // 기형 delta 또는 seq gap → 패닉/접속종료 대신 gap 복구 경로 재사용:
                // baseline을 버리고 keyframe 요청, 이후 delta는 keyframe 도착까지 drop.
                // 요청은 세션당 1회만(폭주 방지).
                other => {
                    if let Some(Err(reason)) = other {
                        tracing::warn!("remote 기형 delta({reason}) — keyframe 재동기화");
                    }
                    recon.remove(&session);
                    let request_keyframe = pending_keyframe.insert(session).then_some(session);
                    ReconOutcome {
                        disconnect: false,
                        request_keyframe,
                    }
                }
            }
        }
    }
}

/// 평문 reader 스레드용 래퍼(기존 시그니처/동작 보존): 재구성 후 keyframe 요청을 writer 소켓으로
/// 보낸다. `false` 반환이면 접속을 끊는다. (TLS는 [`reconstruct_and_dispatch`]를 직접 쓴다.)
fn handle_decoded_event(
    decoded: DecodedEvent,
    subscribers: &Arc<Mutex<Vec<RemoteSubscriber>>>,
    writer: &Mutex<TcpStream>,
    recon: &mut HashMap<SessionId, ViewportBaseline>,
    pending_keyframe: &mut HashSet<SessionId>,
) -> bool {
    let outcome = reconstruct_and_dispatch(decoded, subscribers, recon, pending_keyframe);
    if let Some(session) = outcome.request_keyframe {
        request_keyframe(writer, session);
    }
    !outcome.disconnect
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
    if let Ok(mut stream) = writer.lock()
        && write_frame(&mut *stream, &payload).is_err()
    {
        let _ = stream.shutdown(Shutdown::Both);
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
            if let RuntimeEvent::Viewport { session, .. }
            | RuntimeEvent::ViewportTracked { session, .. } = &event
            {
                if Arc::strong_count(&subscriber.viewports) <= 1 {
                    return false;
                }
                {
                    let mut slot = subscriber
                        .viewports
                        .lock()
                        .expect("remote viewport slot lock");
                    let prev = slot.insert(*session, event.clone());
                    // in_process와 동일 — 미소비 스냅샷의 dirty 델타를 합쳐야 renderer가
                    // 그 행들을 재shaping한다 (2026-07-14 codex 리뷰: 동일 클래스 누락).
                    if let Some(prev) = prev
                        && let Some(current) = slot.get_mut(session)
                    {
                        crate::event::merge_unconsumed_viewport_dirty(&prev, current);
                    }
                }
                true
            } else if let RuntimeEvent::PtyInputPressure { session, .. } = &event {
                if Arc::strong_count(&subscriber.input_pressures) <= 1 {
                    return false;
                }
                subscriber
                    .input_pressures
                    .lock()
                    .expect("remote input pressure slot lock")
                    .insert(*session, event.clone());
                true
            } else {
                match subscriber.events.try_send(event.clone()) {
                    Ok(()) => true,
                    Err(TrySendError::Full(_)) => {
                        subscriber.overflowed.store(true, Ordering::Release);
                        false
                    }
                    Err(TrySendError::Disconnected(_)) => false,
                }
            }
        });
}

impl RuntimeCommandSink for RemoteRuntimeClient {
    fn send_command(&self, command: RuntimeCommand) -> anyhow::Result<()> {
        // 접속 코덱으로 인코딩. 단계 A(Plain)는 postcard(RuntimeCommand)와 바이트 동일.
        let payload = self.codec.encode_command(&command)?;
        match &self.transport {
            ClientTransport::Plain(writer) => {
                let mut stream = writer.lock().expect("remote writer lock");
                write_frame(&mut *stream, &payload)
                    .inspect_err(|_| {
                        let _ = stream.shutdown(Shutdown::Both);
                    })
                    .context("remote 명령 전송 실패")
            }
            // IO 스레드로 enqueue (유계 try_send — 블록 없음). 큐 가득참(backpressure)과
            // IO 스레드 종료를 각각 명시적 Err로 surface한다 — 조용히 버리지 않는다(설계 요구 #3).
            ClientTransport::Tls { commands, .. } => {
                // 크기 검사를 enqueue 전에 — oversized 명령이 Ok를 받고 나중에 IO 스레드
                // write_frame에서 접속을 죽이는 일이 없게 즉시 Err(접속 유지). 평문 경로의
                // write_frame 송신측 상한과 동일 계약 (codex MED).
                if payload.len() > MAX_FRAME_BYTES {
                    bail!(
                        "remote 명령 frame이 상한({MAX_FRAME_BYTES}B)을 초과: {}B — 전송 거부(접속 유지)",
                        payload.len()
                    );
                }
                match commands.try_send(payload) {
                    Ok(()) => Ok(()),
                    Err(std::sync::mpsc::TrySendError::Full(_)) => {
                        Err(RuntimeCommandSendError::Backpressure.into())
                    }
                    Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                        Err(RuntimeCommandSendError::Disconnected.into())
                    }
                }
            }
        }
    }
}

impl RuntimeEventStream for RemoteRuntimeClient {
    fn subscribe(&self) -> RuntimeEventReceiver {
        let (tx, rx) = sync_channel(LOCAL_EVENT_QUEUE_CAP);
        let viewports: Arc<Mutex<std::collections::HashMap<SessionId, RuntimeEvent>>> =
            Arc::default();
        let input_pressures: Arc<Mutex<std::collections::HashMap<SessionId, RuntimeEvent>>> =
            Arc::default();
        let overflowed = Arc::new(AtomicBool::new(false));
        let mut subscribers = self.subscribers.lock().expect("remote subscribers lock");
        if self.connected.load(Ordering::Acquire) {
            subscribers.push(RemoteSubscriber {
                events: tx,
                overflowed: Arc::clone(&overflowed),
                viewports: Arc::clone(&viewports),
                input_pressures: Arc::clone(&input_pressures),
            });
        }
        drop(subscribers);
        RuntimeEventReceiver {
            events: rx,
            pending_durable: Mutex::new(None),
            overflowed,
            viewports,
            input_pressures,
            // remote는 wire 단계에서 outbound 큐가 이미 유계/코얼레싱이라(감사 통과)
            // ResourceUsage도 채널 경로 그대로 — 빈 slot만 채운다.
            resource_usage: Arc::default(),
        }
    }
}

impl crate::client::RuntimeClient for RemoteRuntimeClient {}

impl Drop for RemoteRuntimeClient {
    fn drop(&mut self) {
        // 소켓을 닫아 IO/reader 스레드를 깨운다. TLS는 채널 sender도 함께 drop돼(transport
        // 소유) IO 루프가 Disconnected로도 빠져나온다.
        match &self.transport {
            ClientTransport::Plain(writer) => {
                if let Ok(stream) = writer.lock() {
                    let _ = stream.shutdown(Shutdown::Both);
                }
            }
            ClientTransport::Tls { shutdown, .. } => {
                let _ = shutdown.shutdown(Shutdown::Both);
            }
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
                cwd: None,
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

    fn wait_for_disconnect(rx: &RuntimeEventReceiver, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if rx.is_disconnected() {
                return;
            }
            let _ = rx.drain();
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("remote event receiver did not observe disconnect");
    }

    #[test]
    fn 서버는_loopback에만_bind() {
        let server = RemoteRuntimeServer::serve(test_backend("bind"), 0).unwrap();
        assert!(server.local_addr().ip().is_loopback());
        server.shutdown();
    }

    #[test]
    fn plain_server_disconnect_closes_event_subscription() {
        let server = RemoteRuntimeServer::serve(test_backend("plain-disconnect"), 0).unwrap();
        let client = RemoteRuntimeClient::attach(server.local_addr(), server.auth_token()).unwrap();
        let receiver = client.subscribe();

        server.shutdown();

        wait_for_disconnect(&receiver, Duration::from_secs(5));
    }

    #[test]
    fn subscribing_after_plain_disconnect_is_immediately_closed() {
        let server = RemoteRuntimeServer::serve(test_backend("plain-late-subscribe"), 0).unwrap();
        let client = RemoteRuntimeClient::attach(server.local_addr(), server.auth_token()).unwrap();
        let receiver = client.subscribe();

        server.shutdown();
        wait_for_disconnect(&receiver, Duration::from_secs(5));

        let late_receiver = client.subscribe();
        assert!(late_receiver.is_disconnected());
    }

    #[test]
    fn 비loopback_attach는_거부() {
        let Err(e) = RemoteRuntimeClient::attach("8.8.8.8:1".parse().unwrap(), "t") else {
            panic!("비loopback attach가 성공하면 안 된다");
        };
        assert!(format!("{e:#}").contains("localhost"));
    }

    #[test]
    fn connect_timeout_is_applied_by_connector_helper() {
        let addr: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let err = connect_with(addr, |_addr, timeout| {
            assert_eq!(timeout, Duration::from_secs(30));
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "fake deadline",
            ))
        })
        .unwrap_err();

        assert_eq!(CONNECT_TIMEOUT, Duration::from_secs(30));
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
    }

    #[test]
    fn client_liveness_observes_empty_heartbeat_frame() {
        let start = Instant::now();
        let observed = start + Duration::from_secs(44);
        let mut tracker = liveness::LivenessTracker::new(start, liveness::CLIENT_LIVENESS_TIMEOUT);

        observe_client_frame_for_liveness(&mut tracker, &[], observed);

        assert!(!client_liveness_expired(
            &tracker,
            observed + liveness::CLIENT_LIVENESS_TIMEOUT - Duration::from_nanos(1),
        ));
    }

    #[test]
    fn client_liveness_observes_ordinary_event_frame() {
        let start = Instant::now();
        let observed = start + Duration::from_secs(44);
        let mut tracker = liveness::LivenessTracker::new(start, liveness::CLIENT_LIVENESS_TIMEOUT);

        observe_client_frame_for_liveness(&mut tracker, &[1, 2, 3], observed);

        assert!(!client_liveness_expired(
            &tracker,
            observed + liveness::CLIENT_LIVENESS_TIMEOUT - Duration::from_nanos(1),
        ));
    }

    #[test]
    fn client_liveness_idle_poll_at_deadline_requests_disconnect() {
        let start = Instant::now();
        let tracker = liveness::LivenessTracker::new(start, liveness::CLIENT_LIVENESS_TIMEOUT);

        assert!(client_liveness_expired(
            &tracker,
            start + liveness::CLIENT_LIVENESS_TIMEOUT,
        ));
    }

    #[test]
    fn frame_decoder_treats_timed_out_like_pending() {
        struct TimedOutReader;
        impl Read for TimedOutReader {
            fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "fake timeout",
                ))
            }
        }

        let mut decoder = FrameDecoder::new();

        assert!(matches!(
            decoder.advance(&mut TimedOutReader),
            FramePoll::Pending
        ));
    }

    #[test]
    fn frame_decoder_yields_partial_after_incomplete_successful_read() {
        struct OneByteReader {
            used: bool,
        }
        impl Read for OneByteReader {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                assert!(!self.used, "decoder must return after one partial read");
                self.used = true;
                buf[0] = 1;
                Ok(1)
            }
        }

        let mut decoder = FrameDecoder::new();
        let mut reader = OneByteReader { used: false };

        assert!(matches!(decoder.advance(&mut reader), FramePoll::Partial));
    }

    #[test]
    fn frame_decoder_reports_idle_after_partial_progress_is_exhausted() {
        struct SplitThenTimeout {
            reads: usize,
        }
        impl Read for SplitThenTimeout {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.reads += 1;
                match self.reads {
                    1 => {
                        buf[..2].copy_from_slice(&[5, 0]);
                        Ok(2)
                    }
                    _ => Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "fake timeout",
                    )),
                }
            }
        }

        let mut decoder = FrameDecoder::new();
        let mut reader = SplitThenTimeout { reads: 0 };

        assert!(matches!(decoder.advance(&mut reader), FramePoll::Partial));
        assert!(matches!(decoder.advance(&mut reader), FramePoll::Pending));
    }

    #[test]
    fn tls_idle_sleep_is_skipped_after_partial_frame_progress() {
        assert!(!tls_idle_sleep_needed(true));
        assert!(tls_idle_sleep_needed(false));
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

    #[test]
    fn v12_v13_peer_is_rejected_at_hello_before_event_decode() {
        for version in [10, 12, 13, 17] {
            let old = ClientHello {
                magic: PROTO_MAGIC,
                proto_version: version,
                features: CLIENT_FEATURES,
                token: b"irrelevant".to_vec(),
            };
            assert_eq!(PROTO_VERSION, 18);
            assert!(!client_hello_matches_protocol(&old));
        }
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

        let correlation = RuntimeEvent::AgentSpawnResolved {
            agent_config_id: crate::AgentConfigCorrelationId::from_validated("cfg-1".to_owned()),
            session: Some(SessionId(7)),
        };
        assert_eq!(
            Codec::Plain.encode_event(&correlation).unwrap(),
            postcard::to_allocvec(&correlation).unwrap(),
            "new correlation must preserve the Plain postcard contract"
        );
        for codec in [Codec::Plain, Codec::Delta] {
            let frame = codec.encode_event(&correlation).unwrap();
            let DecodedEvent::Event(decoded) = codec.decode_event(&frame).unwrap() else {
                panic!("correlation decoded as a viewport frame");
            };
            validate_event(&decoded).unwrap();
            assert!(matches!(
                decoded,
                RuntimeEvent::AgentSpawnResolved {
                    agent_config_id,
                    session: Some(SessionId(7)),
                } if agent_config_id.as_str() == "cfg-1"
            ));
        }

        // 디코드도 기존 경로가 만든 프레임을 그대로 받아들인다(왕복).
        let cmd_frame = postcard::to_allocvec(&command).unwrap();
        assert!(matches!(
            Codec::Plain.decode_command(&cmd_frame).unwrap(),
            DecodedCommand::Command(RuntimeCommand::SpawnAgent { cols: 80, .. })
        ));
    }

    #[test]
    fn durable_event_barrier_remote_codecs_roundtrip_and_reject_invalid_payloads() {
        let command = RuntimeCommand::DurableEventBarrier { correlation_id: 91 };
        let event = RuntimeEvent::DurableEventBarrierReached { correlation_id: 91 };

        for codec in [Codec::Plain, Codec::Delta] {
            let command_frame = codec.encode_command(&command).unwrap();
            let DecodedCommand::Command(decoded_command) =
                codec.decode_command(&command_frame).unwrap()
            else {
                panic!("barrier decoded as keyframe request");
            };
            assert!(matches!(
                decoded_command,
                RuntimeCommand::DurableEventBarrier { correlation_id: 91 }
            ));
            validate_command(&decoded_command).unwrap();

            let event_frame = codec.encode_event(&event).unwrap();
            let DecodedEvent::Event(decoded_event) = codec.decode_event(&event_frame).unwrap()
            else {
                panic!("barrier decoded as viewport frame");
            };
            assert!(matches!(
                decoded_event,
                RuntimeEvent::DurableEventBarrierReached { correlation_id: 91 }
            ));
            validate_event(&decoded_event).unwrap();

            assert!(codec.decode_command(&[0xff, 0xff]).is_err());
            assert!(codec.decode_event(&[0xff, 0xff]).is_err());
        }

        assert!(
            validate_command(&RuntimeCommand::DurableEventBarrier { correlation_id: 0 }).is_err()
        );
        assert!(
            validate_event(&RuntimeEvent::DurableEventBarrierReached { correlation_id: 0 })
                .is_err()
        );
    }

    #[test]
    fn durable_event_barrier_crosses_remote_runtime_transport() {
        let server = RemoteRuntimeServer::serve(test_backend("durable-event-barrier"), 0).unwrap();
        let client = RemoteRuntimeClient::attach(server.local_addr(), server.auth_token()).unwrap();
        let receiver = client.subscribe();

        client
            .send_command(RuntimeCommand::DurableEventBarrier { correlation_id: 92 })
            .unwrap();
        let mut seen = Vec::new();
        wait_for(&receiver, &mut seen, Duration::from_secs(10), |events| {
            events.iter().any(|event| {
                matches!(
                    event,
                    RuntimeEvent::DurableEventBarrierReached { correlation_id: 92 }
                )
            })
        });

        drop(client);
        server.shutdown();
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

    #[test]
    fn plain_client_drop_joins_reader_liveness_thread() {
        let server = RemoteRuntimeServer::serve(test_backend("plain-client-drop"), 0).unwrap();
        let client = RemoteRuntimeClient::attach(server.local_addr(), server.auth_token()).unwrap();
        let done = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&done);
        let handle = std::thread::spawn(move || {
            drop(client);
            flag.store(true, Ordering::SeqCst);
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        while !done.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "plain client drop did not join");
            std::thread::sleep(Duration::from_millis(20));
        }

        handle.join().unwrap();
        server.shutdown();
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

    /// heartbeat/liveness: 유휴 접속에는 길이 0 heartbeat가 오거나, ResourceUsage 같은
    /// 저빈도 상태 이벤트가 이미 흐르고 있으면 그 valid 이벤트가 liveness를 증명한다.
    /// postcard RuntimeEvent는 항상 길이 > 0이라 heartbeat와 정상 이벤트가 구분된다.
    #[test]
    fn 유휴_접속은_heartbeat_또는_valid_event로_liveness_확인() {
        let server = RemoteRuntimeServer::serve(test_backend("heartbeat"), 0).unwrap();
        let mut raw = TcpStream::connect(server.local_addr()).unwrap();
        let hello = v2_handshake(&mut raw, server.auth_token().as_bytes(), CLIENT_FEATURES)
            .expect("ServerHello 없음");
        assert_eq!(hello.proto_version, PROTO_VERSION);

        // HEARTBEAT_INTERVAL(15s) + 15s 여유
        raw.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        let frame = read_frame(&mut raw).expect("heartbeat/event 프레임을 받지 못함");
        if !frame.is_empty() {
            Codec::Delta
                .decode_event(&frame)
                .expect("non-heartbeat liveness frame must be a valid event");
        }

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
        let malformed_id = "raw-id-never-reflect".repeat(16);
        let malformed_spawn = RuntimeCommand::SpawnAgent {
            cols: 80,
            rows: 24,
            scrollback_lines: 100,
            agent_config_id: Some(malformed_id.clone()),
            command: "/bin/echo".to_owned(),
            args: Vec::new(),
            env_plain: Vec::new(),
            env_secrets: Vec::new(),
            waiting_regex: None,
            approval_regex: None,
            error_regex: None,
            done_regex: None,
        };
        let reason = validate_command(&malformed_spawn).unwrap_err();
        assert!(!reason.contains(&malformed_id));
        assert!(reason.contains("agent_config_id"));
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
    use terminal::{CellRange, CursorShape, CursorSnapshot, TerminalCell};

    fn cell(c: char) -> TerminalCell {
        TerminalCell {
            c,
            fg: [10, 20, 30],
            bg: [0, 0, 0],
            wide: false,
            wide_spacer: false,
            attrs: Default::default(),
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

    fn assert_same_viewport_content(
        actual: &TerminalViewportSnapshot,
        expected: &TerminalViewportSnapshot,
    ) {
        assert_eq!(actual.cols, expected.cols);
        assert_eq!(actual.rows, expected.rows);
        assert_eq!(actual.cursor, expected.cursor);
        assert_eq!(actual.visible_cells, expected.visible_cells);
        assert_eq!(actual.title, expected.title);
        assert_eq!(actual.scroll_offset, expected.scroll_offset);
        assert_eq!(actual.is_alt_screen, expected.is_alt_screen);
    }

    #[test]
    fn outbound_queue는_viewport를_coalesce하고_status_lifecycle을_보존() {
        let mut queue = OutboundEventQueue::with_caps(4, 2);
        for i in 0..10 {
            let line = format!("s{i}");
            queue
                .enqueue(RuntimeEvent::Viewport {
                    session: SessionId(i),
                    snapshot: make_snapshot(8, 2, &[line.as_str()], false),
                    bracketed_paste: false,
                })
                .unwrap();
        }
        assert_eq!(queue.viewport_len(), 2, "viewport slot은 cap으로 제한된다");
        assert_eq!(queue.durable_len(), 0);

        queue
            .enqueue(RuntimeEvent::SessionStatusChanged {
                session: SessionId(100),
                status: session::SessionStatus::NeedsApproval,
            })
            .unwrap();
        queue
            .enqueue(RuntimeEvent::SessionExited {
                session: SessionId(101),
                exit_code: Some(0),
            })
            .unwrap();

        assert!(matches!(
            queue.pop_front(),
            Some(RuntimeEvent::SessionStatusChanged {
                session: SessionId(100),
                status: session::SessionStatus::NeedsApproval,
            })
        ));
        assert!(matches!(
            queue.pop_front(),
            Some(RuntimeEvent::SessionExited {
                session: SessionId(101),
                exit_code: Some(0),
            })
        ));

        let remaining: Vec<SessionId> = std::iter::from_fn(|| match queue.pop_front() {
            Some(RuntimeEvent::Viewport { session, .. }) => Some(session),
            Some(other) => panic!(
                "unexpected event after durable drain: {:?}",
                kind_of(&other)
            ),
            None => None,
        })
        .collect();
        assert_eq!(
            remaining,
            vec![SessionId(8), SessionId(9)],
            "old viewport slots are dropped, newest slots remain"
        );
    }

    #[test]
    fn remote_outbound_preserves_stale_and_requested_mux_before_durable_event_barrier() {
        let stale = mux_snapshot("stale", &[("stale", SessionId(1))]);
        let requested = mux_snapshot(
            "requested",
            &[("stale", SessionId(1)), ("requested", SessionId(2))],
        );
        let mut queue = OutboundEventQueue::with_caps(3, 2);
        queue
            .enqueue(RuntimeEvent::MuxUpdated {
                snapshot: Arc::clone(&stale),
            })
            .unwrap();
        queue
            .enqueue(RuntimeEvent::MuxUpdated {
                snapshot: Arc::clone(&requested),
            })
            .unwrap();
        queue
            .enqueue(RuntimeEvent::DurableEventBarrierReached { correlation_id: 94 })
            .unwrap();

        assert!(matches!(
            queue.pop_front(),
            Some(RuntimeEvent::MuxUpdated { snapshot }) if Arc::ptr_eq(&snapshot, &stale)
        ));
        assert!(matches!(
            queue.pop_front(),
            Some(RuntimeEvent::MuxUpdated { snapshot }) if Arc::ptr_eq(&snapshot, &requested)
        ));
        assert!(matches!(
            queue.pop_front(),
            Some(RuntimeEvent::DurableEventBarrierReached { correlation_id: 94 })
        ));
        assert!(queue.pop_front().is_none());
    }

    #[test]
    fn durable_event_barrier_explicitly_does_not_fence_viewport_slots() {
        let mut queue = OutboundEventQueue::with_caps(2, 1);
        queue
            .enqueue(RuntimeEvent::Viewport {
                session: SessionId(1),
                snapshot: make_snapshot(8, 2, &["before"], false),
                bracketed_paste: false,
            })
            .unwrap();
        queue
            .enqueue(RuntimeEvent::DurableEventBarrierReached { correlation_id: 95 })
            .unwrap();
        queue
            .enqueue(RuntimeEvent::Viewport {
                session: SessionId(1),
                snapshot: make_snapshot(8, 2, &["after"], false),
                bracketed_paste: false,
            })
            .unwrap();

        assert_eq!(
            queue.viewport_len(),
            1,
            "viewport remains one bounded latest slot"
        );
        assert!(matches!(
            queue.pop_front(),
            Some(RuntimeEvent::DurableEventBarrierReached { correlation_id: 95 })
        ));
        assert!(matches!(
            queue.pop_front(),
            Some(RuntimeEvent::Viewport {
                session: SessionId(1),
                ..
            })
        ));
        assert!(queue.pop_front().is_none());
    }

    #[test]
    fn outbound_queue는_durable_overflow를_silent_drop하지_않는다() {
        let mut queue = OutboundEventQueue::with_caps(2, 8);
        queue
            .enqueue(RuntimeEvent::SessionStatusChanged {
                session: SessionId(1),
                status: session::SessionStatus::Waiting,
            })
            .unwrap();
        queue
            .enqueue(RuntimeEvent::SessionStatusChanged {
                session: SessionId(2),
                status: session::SessionStatus::Done,
            })
            .unwrap();
        let overflow = queue
            .enqueue(RuntimeEvent::SessionExited {
                session: SessionId(3),
                exit_code: None,
            })
            .unwrap_err();
        assert_eq!(overflow, OutboundOverflow::DurableFull);
        assert_eq!(queue.durable_len(), 2, "durable queue remains bounded");
    }

    #[test]
    fn receiver_drain은_durable_cap에서_멈춘다() {
        let (tx, rx) = std::sync::mpsc::channel();
        let viewports: Arc<Mutex<HashMap<SessionId, RuntimeEvent>>> = Arc::default();
        let input_pressures: Arc<Mutex<HashMap<SessionId, RuntimeEvent>>> = Arc::default();
        let receiver = RuntimeEventReceiver {
            events: rx,
            pending_durable: Mutex::new(None),
            overflowed: Arc::default(),
            viewports,
            input_pressures,
            resource_usage: Arc::default(),
        };
        for i in 0..5 {
            tx.send(RuntimeEvent::SessionStatusChanged {
                session: SessionId(i),
                status: session::SessionStatus::Running,
            })
            .unwrap();
        }

        let mut queue = OutboundEventQueue::with_caps(3, 8);
        let overflow = drain_receiver_into_outbound(&receiver, &mut queue).unwrap_err();
        assert_eq!(overflow, OutboundOverflow::DurableFull);
        assert_eq!(queue.durable_len(), 3);
    }

    #[test]
    fn receiver_drain은_additive_local_events를_wire에서_필터링한다() {
        let (tx, rx) = std::sync::mpsc::channel();
        let viewports: Arc<Mutex<HashMap<SessionId, RuntimeEvent>>> = Arc::default();
        let input_pressures: Arc<Mutex<HashMap<SessionId, RuntimeEvent>>> = Arc::default();
        let receiver = RuntimeEventReceiver {
            events: rx,
            pending_durable: Mutex::new(None),
            overflowed: Arc::default(),
            viewports,
            input_pressures: Arc::clone(&input_pressures),
            resource_usage: Arc::default(),
        };
        tx.send(RuntimeEvent::SessionStatusViewChanged {
            session: SessionId(1),
            view: session::SessionStatusView::process_exit(session::SessionStatus::Done),
        })
        .unwrap();
        tx.send(RuntimeEvent::SessionStatusChanged {
            session: SessionId(1),
            status: session::SessionStatus::Done,
        })
        .unwrap();
        input_pressures.lock().unwrap().insert(
            SessionId(1),
            RuntimeEvent::PtyInputPressure {
                session: SessionId(1),
                pressure: pty::PtyInputPressure {
                    attempted_bytes: 1,
                    queued_bytes: 1,
                    queued_messages: 1,
                    max_bytes: 1,
                    max_messages: 1,
                    reason: pty::PtyInputRejectReason::QueueFull,
                },
            },
        );

        let mut queue = OutboundEventQueue::with_caps(4, 4);
        assert_eq!(
            drain_receiver_into_outbound(&receiver, &mut queue).unwrap(),
            OutboundDrain::Open
        );
        assert!(matches!(
            queue.pop_front(),
            Some(RuntimeEvent::SessionStatusChanged {
                session: SessionId(1),
                status: session::SessionStatus::Done,
            })
        ));
        assert!(queue.pop_front().is_none());
        assert!(receiver.input_pressures.lock().unwrap().is_empty());
    }

    fn kind_of(event: &RuntimeEvent) -> &'static str {
        match event {
            RuntimeEvent::ShellSpawned { .. } => "ShellSpawned",
            RuntimeEvent::AgentSpawned { .. } => "AgentSpawned",
            RuntimeEvent::SpawnFailed { .. } => "SpawnFailed",
            RuntimeEvent::Viewport { .. } => "Viewport",
            RuntimeEvent::SessionExited { .. } => "SessionExited",
            RuntimeEvent::MuxUpdated { .. } => "MuxUpdated",
            RuntimeEvent::SessionStatusChanged { .. } => "SessionStatusChanged",
            RuntimeEvent::SessionStatusViewChanged { .. } => "SessionStatusViewChanged",
            RuntimeEvent::ResourceUsage { .. } => "ResourceUsage",
            RuntimeEvent::PtyInputPressure { .. } => "PtyInputPressure",
            RuntimeEvent::SessionRestored { .. } => "SessionRestored",
            RuntimeEvent::ScrollbackSearchResult { .. } => "ScrollbackSearchResult",
            RuntimeEvent::LastOutputExtracted { .. } => "LastOutputExtracted",
            RuntimeEvent::AgentSpawnResolved { .. } => "AgentSpawnResolved",
            RuntimeEvent::SessionFreezeChanged { .. } => "SessionFreezeChanged",
            RuntimeEvent::DurableEventBarrierReached { .. } => "DurableEventBarrierReached",
            RuntimeEvent::UnattachedSessionsInspected { .. } => "UnattachedSessionsInspected",
            RuntimeEvent::UnattachedSessionsKilled { .. } => "UnattachedSessionsKilled",
            RuntimeEvent::ScrollbackLimitApplied { .. } => "ScrollbackLimitApplied",
            RuntimeEvent::ResizeApplied { .. } => "ResizeApplied",
            RuntimeEvent::ResizeFailed { .. } => "ResizeFailed",
            RuntimeEvent::ViewportTracked { .. } => "ViewportTracked",
            RuntimeEvent::EnvironmentApplied { .. } => "EnvironmentApplied",
            RuntimeEvent::InputAdmitted { .. } => "InputAdmitted",
        }
    }

    #[test]
    fn unattached_session_event_names_and_remote_codecs_roundtrip() {
        let events = [
            (
                RuntimeEvent::EnvironmentApplied {
                    session: Some(SessionId(4)),
                    revision: Some(99),
                },
                "EnvironmentApplied",
            ),
            (
                RuntimeEvent::UnattachedSessionsInspected { count: 7 },
                "UnattachedSessionsInspected",
            ),
            (
                RuntimeEvent::UnattachedSessionsKilled { count: 3 },
                "UnattachedSessionsKilled",
            ),
        ];

        for (event, expected_name) in events {
            assert_eq!(kind_of(&event), expected_name);
            for codec in [Codec::Plain, Codec::Delta] {
                let frame = codec.encode_event(&event).unwrap();
                let DecodedEvent::Event(decoded) = codec.decode_event(&frame).unwrap() else {
                    panic!("unattached session event decoded as a viewport frame");
                };
                validate_event(&decoded).unwrap();
                assert_eq!(kind_of(&decoded), expected_name);
            }
        }
    }

    fn mux_snapshot(active: &str, tabs: &[(&str, SessionId)]) -> Arc<MuxSnapshot> {
        Arc::new(MuxSnapshot {
            tabs: tabs
                .iter()
                .map(|(tab, session)| {
                    let tab_id = deppy_core::MuxTabId((*tab).to_owned());
                    let pane_id = deppy_core::MuxPaneId(format!("pane-{tab}"));
                    mux::TabSnapshot {
                        id: tab_id,
                        title: (*tab).to_owned(),
                        layout: crate::LayoutNode::Pane(pane_id.clone()),
                        panes: vec![mux::PaneSnapshot {
                            id: pane_id,
                            session_id: Some(*session),
                            title: format!("pane-{tab}"),
                            persistent_session_id: None,
                        }],
                    }
                })
                .collect(),
            active_tab: Some(deppy_core::MuxTabId(active.to_owned())),
            focused_pane: None,
        })
    }

    /// 네트워크 없이 서버 encode(last_sent) → 클라이언트 reconstruct(recon) 파이프라인을 돈다.
    /// round_trip은 매 tick의 재구성된 전체 스냅샷을 돌려준다 — source와 == 여야 한다(§4.8).
    struct DeltaPipe {
        last_sent: HashMap<SessionId, ViewportBaseline>,
        recon: HashMap<SessionId, ViewportBaseline>,
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
                    self.recon
                        .insert(session, (seq, Arc::clone(&snapshot), None));
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
                    let (cur_seq, prev, _) = self.recon.get(&session).expect("baseline 있어야 함");
                    assert_eq!(*cur_seq, base_seq, "base_seq가 recon seq와 일치해야 한다");
                    let new = try_apply_delta(prev, &delta).expect("정상 delta는 적용돼야 한다");
                    self.recon
                        .insert(session, (seq, Arc::new(new.clone()), None));
                    new
                }
                _ => panic!("legacy viewport codec fixture의 예상 밖 event"),
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
        let expected_dirty = [
            Vec::new(),
            vec![CellRange { start: 0, end: 20 }],
            vec![CellRange { start: 40, end: 60 }],
            Vec::new(),
        ];
        for (snap, dirty_ranges) in snaps.iter().zip(expected_dirty) {
            let recon = pipe.round_trip(s, snap);
            assert_same_viewport_content(&recon, snap);
            assert_eq!(
                recon.dirty_ranges, dirty_ranges,
                "delta 재구성은 변경 row를 dirty_ranges로 복원해야 한다"
            );
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
        let light_recon = pipe.round_trip(s, &light);
        assert_same_viewport_content(&light_recon, &light);
        assert_eq!(
            light_recon.dirty_ranges,
            vec![CellRange { start: 0, end: 20 }]
        );
        assert_eq!(pipe.deltas, 1, "20% 변경은 delta");

        // 8/10 = 80% 변경 → keyframe 폴백
        let heavy = make_snapshot(10, 10, &["1", "2", "3", "4", "5", "6", "7", "8"], false);
        assert_eq!(&pipe.round_trip(s, &heavy), heavy.as_ref());
        assert_eq!(pipe.keyframes, 2, "80% 변경은 keyframe 폴백");
    }

    fn resize_stamp(epoch: u64) -> crate::ResizeStamp {
        let token = crate::ResizeToken {
            owner: [1; 16],
            generation: epoch,
            owner_epoch: 1,
        };
        crate::ResizeStamp {
            epoch,
            owner_epoch: 1,
            token: Some(token),
            cols: 20,
            rows: 5,
        }
    }

    #[test]
    fn tracked_resize_plain_delta_왕복은_stamp를_그대로_보존한다() {
        let session = SessionId(1);
        let stamp = resize_stamp(1);
        let snapshot = make_snapshot(20, 5, &["a"], false);
        let event = RuntimeEvent::ViewportTracked {
            session,
            snapshot: snapshot.clone(),
            bracketed_paste: true,
            stamp,
        };
        let frame = Codec::Plain.encode_event(&event).unwrap();
        let DecodedEvent::Event(decoded) = Codec::Plain.decode_event(&frame).unwrap() else {
            panic!("plain event");
        };
        assert_eq!(decoded.viewport().unwrap().3, Some(stamp));
        let subscribers = Arc::default();
        let mut sent = HashMap::new();
        let mut recon = HashMap::new();
        let mut pending = HashSet::new();
        for snapshot in [snapshot, make_snapshot(20, 5, &["ab"], false)] {
            let frame =
                encode_viewport_frame_stamped(&mut sent, session, &snapshot, true, Some(stamp))
                    .unwrap();
            let decoded = Codec::Delta.decode_event(&frame).unwrap();
            let outcome = reconstruct_and_dispatch(decoded, &subscribers, &mut recon, &mut pending);
            assert!(!outcome.disconnect);
            assert_eq!(recon[&session].2, Some(stamp));
            assert_eq!(recon[&session].1.visible_cells, snapshot.visible_cells);
        }
    }

    #[test]
    fn tracked_resize_stamp가_달라지면_같은크기여도_keyframe이다() {
        let session = SessionId(1);
        let snapshot = make_snapshot(20, 5, &["a"], false);
        let mut sent = HashMap::new();
        encode_viewport_frame_stamped(&mut sent, session, &snapshot, false, Some(resize_stamp(1)))
            .unwrap();
        let frame = encode_viewport_frame_stamped(
            &mut sent,
            session,
            &snapshot,
            false,
            Some(resize_stamp(2)),
        )
        .unwrap();
        assert!(
            matches!(Codec::Delta.decode_event(&frame).unwrap(), DecodedEvent::KeyframeTracked { stamp, .. } if stamp == resize_stamp(2))
        );
    }

    #[test]
    fn tracked_resize_delta의_stamp가_기준선과_다르면_한번만_재동기화한다() {
        let session = SessionId(1);
        let before = make_snapshot(20, 5, &["a"], false);
        let after = make_snapshot(20, 5, &["ab"], false);
        let mut recon = HashMap::from([(session, (0, before.clone(), Some(resize_stamp(1))))]);
        let mut pending = HashSet::new();
        let subscribers = Arc::default();
        for expected in [Some(session), None] {
            let decoded = DecodedEvent::DeltaTracked {
                session,
                seq: 1,
                base_seq: 0,
                delta: diff_viewport(&before, &after).unwrap(),
                bracketed_paste: false,
                stamp: resize_stamp(2),
            };
            let outcome = reconstruct_and_dispatch(decoded, &subscribers, &mut recon, &mut pending);
            assert!(!outcome.disconnect);
            assert_eq!(outcome.request_keyframe, expected);
            assert!(recon.is_empty());
        }
    }

    /// gap 복구(§4.4): delta 유실로 base_seq가 앞서면 클라이언트가 keyframe을 요청하고,
    /// 서버는 baseline을 버려 다음 프레임을 keyframe으로 보내 재구성이 복구된다.
    #[test]
    fn delta_gap_복구() {
        let s = SessionId(1);
        let mut last_sent: HashMap<SessionId, ViewportBaseline> = HashMap::new();
        let mut recon: HashMap<SessionId, ViewportBaseline> = HashMap::new();

        // keyframe(seq 0) — 클라이언트 baseline 세팅
        let s0 = make_snapshot(20, 5, &["a"], false);
        let f0 = encode_viewport_frame(&mut last_sent, s, &s0, false).unwrap();
        let DecodedEvent::Keyframe { seq, snapshot, .. } = Codec::Delta.decode_event(&f0).unwrap()
        else {
            panic!("첫 프레임은 keyframe");
        };
        recon.insert(s, (seq, snapshot, None));

        // 서버가 delta(seq 1)를 보내지만 유실됐다고 가정 — 클라이언트는 못 받는다.
        let s1 = make_snapshot(20, 5, &["ab"], false);
        let _lost = encode_viewport_frame(&mut last_sent, s, &s1, false).unwrap();

        // 다음 delta(seq 2, base_seq 1)가 도착 — 클라 recon seq는 0이라 gap.
        let s2 = make_snapshot(20, 5, &["abc"], false);
        let f2 = encode_viewport_frame(&mut last_sent, s, &s2, false).unwrap();
        let DecodedEvent::Delta { base_seq, .. } = Codec::Delta.decode_event(&f2).unwrap() else {
            panic!("seq 2는 delta");
        };
        let (cur_seq, _, _) = recon.get(&s).unwrap();
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
        let mut recon: HashMap<SessionId, ViewportBaseline> = HashMap::new();
        recon.insert(s, (5, base.clone(), None));
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
        let mut recon: HashMap<SessionId, ViewportBaseline> = HashMap::new();
        recon.insert(s, (0, base, None));
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
        let mut last_sent: HashMap<SessionId, ViewportBaseline> = HashMap::new();
        let snap = make_snapshot(10, 3, &["a"], false);
        encode_viewport_frame(&mut last_sent, s, &snap, false).unwrap();
        assert!(last_sent.contains_key(&s), "keyframe이 baseline을 남긴다");

        let exit = RuntimeEvent::SessionExited {
            session: s,
            exit_code: Some(0),
        };
        let mut exited = ExitedSessionTombstones::new();
        let mut visible = None;
        encode_pump_frame(
            Codec::Delta,
            &mut last_sent,
            &mut exited,
            &mut visible,
            &exit,
        )
        .unwrap();
        assert!(
            !last_sent.contains_key(&s),
            "SessionExited가 서버 baseline을 정리한다"
        );
        assert!(exited.contains(&s), "종료 세션이 tombstone에 등록된다");
    }

    /// MuxUpdated(active_tab=B)는 서버 delta baseline을 visible set으로 prune하고,
    /// 같은 drain 배치의 hidden Viewport(A)가 last_sent를 되살리지 않는다.
    #[test]
    fn muxupdated는_서버_last_sent를_visible_set으로_prune() {
        let hidden = SessionId(21);
        let visible_session = SessionId(22);
        let hidden_snap = make_snapshot(10, 3, &["hidden"], false);
        let visible_snap = make_snapshot(10, 3, &["visible"], false);
        let mut last_sent: HashMap<SessionId, ViewportBaseline> = HashMap::new();
        encode_viewport_frame(&mut last_sent, hidden, &hidden_snap, false).unwrap();
        encode_viewport_frame(&mut last_sent, visible_session, &visible_snap, false).unwrap();
        assert!(last_sent.contains_key(&hidden));
        assert!(last_sent.contains_key(&visible_session));

        let mux = RuntimeEvent::MuxUpdated {
            snapshot: mux_snapshot("b", &[("a", hidden), ("b", visible_session)]),
        };
        let mut exited = ExitedSessionTombstones::new();
        let mut visible = None;
        encode_pump_frame(
            Codec::Delta,
            &mut last_sent,
            &mut exited,
            &mut visible,
            &mux,
        )
        .unwrap();
        assert!(
            !last_sent.contains_key(&hidden),
            "active tab 밖 session baseline은 MuxUpdated에서 제거돼야 한다"
        );
        assert!(
            last_sent.contains_key(&visible_session),
            "active tab visible session baseline은 유지돼야 한다"
        );

        let stale = RuntimeEvent::Viewport {
            session: hidden,
            snapshot: Arc::clone(&hidden_snap),
            bracketed_paste: false,
        };
        let frame = encode_pump_frame(
            Codec::Delta,
            &mut last_sent,
            &mut exited,
            &mut visible,
            &stale,
        )
        .unwrap();
        assert!(
            !last_sent.contains_key(&hidden),
            "MuxUpdated 뒤 hidden Viewport가 서버 baseline을 되살리면 안 된다"
        );
        assert!(
            matches!(
                Codec::Delta.decode_event(&frame).unwrap(),
                DecodedEvent::Event(RuntimeEvent::Viewport { session, .. }) if session == hidden
            ),
            "hidden trailing Viewport는 delta baseline 없이 plain Event로만 나가야 한다"
        );
    }

    /// MuxUpdated(active_tab=B)는 클라이언트 recon/pending baseline도 visible set으로 prune한다.
    #[test]
    fn muxupdated는_클라_recon을_visible_set으로_prune() {
        let hidden = SessionId(31);
        let visible_session = SessionId(32);
        let subscribers: Arc<Mutex<Vec<RemoteSubscriber>>> = Arc::default();
        let mut recon: HashMap<SessionId, ViewportBaseline> = HashMap::new();
        recon.insert(hidden, (0, make_snapshot(10, 3, &["hidden"], false), None));
        recon.insert(
            visible_session,
            (0, make_snapshot(10, 3, &["visible"], false), None),
        );
        let mut pending = HashSet::new();
        pending.insert(hidden);
        pending.insert(visible_session);

        let outcome = reconstruct_and_dispatch(
            DecodedEvent::Event(RuntimeEvent::MuxUpdated {
                snapshot: mux_snapshot("b", &[("a", hidden), ("b", visible_session)]),
            }),
            &subscribers,
            &mut recon,
            &mut pending,
        );

        assert!(!outcome.disconnect);
        assert!(
            !recon.contains_key(&hidden),
            "active tab 밖 session recon baseline은 제거돼야 한다"
        );
        assert!(
            recon.contains_key(&visible_session),
            "active tab visible session recon baseline은 유지돼야 한다"
        );
        assert!(
            !pending.contains(&hidden),
            "hidden pending keyframe도 정리한다"
        );
        assert!(
            pending.contains(&visible_session),
            "visible pending keyframe은 유지한다"
        );
    }

    /// 같은 drain 배치의 [SessionExited(X), Viewport(X)]에서 trailing viewport가 종료 세션의
    /// baseline을 되살리지 않는다 (codex P2). 그래도 최종 출력은 클라 slot에 emit된다.
    #[test]
    fn 종료_배치의_trailing_viewport는_baseline_되살리지_않는다() {
        let s = SessionId(8);
        // 이전 tick의 keyframe으로 양쪽에 baseline이 있었다고 가정.
        let mut last_sent: HashMap<SessionId, ViewportBaseline> = HashMap::new();
        let snap0 = make_snapshot(10, 3, &["a"], false);
        encode_viewport_frame(&mut last_sent, s, &snap0, false).unwrap();
        assert!(last_sent.contains_key(&s));

        // 종료 tick: drain 순서대로 SessionExited(X) 먼저, Viewport(X) 나중.
        let mut exited = ExitedSessionTombstones::new();
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
        let mut visible = None;
        let exit_frame = encode_pump_frame(
            Codec::Delta,
            &mut last_sent,
            &mut exited,
            &mut visible,
            &exit_ev,
        )
        .unwrap();
        let vp_frame = encode_pump_frame(
            Codec::Delta,
            &mut last_sent,
            &mut exited,
            &mut visible,
            &vp_ev,
        )
        .unwrap();

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
        let (tx, _rx) = sync_channel(LOCAL_EVENT_QUEUE_CAP);
        let slot: Arc<Mutex<HashMap<SessionId, RuntimeEvent>>> = Arc::default();
        let subscribers: Arc<Mutex<Vec<RemoteSubscriber>>> =
            Arc::new(Mutex::new(vec![RemoteSubscriber {
                events: tx,
                overflowed: Arc::default(),
                viewports: Arc::clone(&slot),
                input_pressures: Arc::default(),
            }]));
        let mut recon: HashMap<SessionId, ViewportBaseline> = HashMap::new();
        recon.insert(s, (0, snap0, None)); // 클라도 baseline이 있었음
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
        let mut recon: HashMap<SessionId, ViewportBaseline> = HashMap::new();
        recon.insert(s, (2, make_snapshot(10, 3, &["a"], false), None));
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

    // ---- TLS 전송 (단계 C-2 + C-3 core, §2) ----

    /// 자기서명 신원을 rcgen로 직접 만든다 (keyring 우회 — 신원 수명주기는 tls_identity.rs가
    /// 테스트한다). 여러 테스트가 공유 mock keyring의 고정 키 id를 다투지 않게 한다.
    fn test_identity() -> TlsIdentity {
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec!["deppy-remote".to_owned()]).unwrap();
        TlsIdentity {
            cert_der: cert.der().to_vec(),
            key_der: signing_key.serialize_der(),
        }
    }

    /// 커스텀 셸 커맨드로 백엔드를 만든다 (slow-consumer 테스트의 대량 출력용).
    fn test_backend_cmd(name: &str, args: Vec<String>) -> InProcessRuntimeClient {
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
                args,
                env: Vec::new(),
                cwd: None,
            },
            None,
        )
    }

    /// C-3 잔여: known_hosts TOFU — first-use는 핀, 재접속은 검증, 지문 변경은 거부+복구.
    #[test]
    fn tofu_first_use핀_재접속검증_지문변경거부_forget복구() {
        let kh_dir = std::env::temp_dir().join(format!("deppy-tofu-{}", std::process::id()));
        std::fs::create_dir_all(&kh_dir).unwrap();
        let kh_path = kh_dir.join("known_hosts");
        let mut kh = crate::known_hosts::KnownHosts::load(&kh_path).unwrap();

        let identity = test_identity();
        let fp = identity.fingerprint();
        let server = RemoteRuntimeServer::serve_tls(
            test_backend("tofu"),
            "127.0.0.1:0".parse().unwrap(),
            identity,
            false,
        )
        .unwrap();
        let addr = server.local_addr();
        let host = addr.to_string();

        // first-use → Pinned(관찰 지문) + 파일 반영
        let (client, outcome) =
            RemoteRuntimeClient::attach_tls_tofu(addr, server.auth_token(), &mut kh).unwrap();
        assert_eq!(
            outcome,
            TofuOutcome::Pinned {
                fingerprint: fp.clone()
            }
        );
        assert_eq!(kh.lookup(&host), Some(fp.as_str()));
        drop(client);

        // 재접속 → Verified (저장 핀으로 검증 attach)
        let mut kh2 = crate::known_hosts::KnownHosts::load(&kh_path).unwrap();
        let (client2, outcome2) =
            RemoteRuntimeClient::attach_tls_tofu(addr, server.auth_token(), &mut kh2).unwrap();
        assert_eq!(outcome2, TofuOutcome::Verified);
        drop(client2);

        // 지문 변경 시뮬레이션: 다른 identity의 서버 B에, A의 지문을 미리 핀해 두면
        // Mismatch → handshake 거부, 에러에 관찰 지문+forget 안내가 담긴다.
        let identity_b = test_identity();
        let fp_b = identity_b.fingerprint();
        let server_b = RemoteRuntimeServer::serve_tls(
            test_backend("tofu-b"),
            "127.0.0.1:0".parse().unwrap(),
            identity_b,
            false,
        )
        .unwrap();
        let host_b = server_b.local_addr().to_string();
        kh2.pin(&host_b, &fp).unwrap(); // 일부러 옛(A) 지문을 핀
        let err = match RemoteRuntimeClient::attach_tls_tofu(
            server_b.local_addr(),
            server_b.auth_token(),
            &mut kh2,
        ) {
            Ok(_) => panic!("지문 불일치인데 attach가 성공하면 안 된다"),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("지문 불일치"), "{msg}");

        // forget → 재-TOFU 성공(새 지문 핀)
        kh2.forget(&host_b).unwrap();
        let (_client3, outcome3) = RemoteRuntimeClient::attach_tls_tofu(
            server_b.local_addr(),
            server_b.auth_token(),
            &mut kh2,
        )
        .unwrap();
        assert_eq!(outcome3, TofuOutcome::Pinned { fingerprint: fp_b });
        std::fs::remove_dir_all(&kh_dir).unwrap();
    }

    /// C-4: 비-loopback bind는 명시 opt-in 없이는 거부된다 (bind 전에 검증 — 소켓 안 열림).
    #[test]
    fn 비loopback_bind는_optin_없이_거부() {
        let err = match RemoteRuntimeServer::serve_tls(
            test_backend("c4-guard"),
            "0.0.0.0:0".parse().unwrap(),
            test_identity(),
            false,
        ) {
            Ok(_) => panic!("opt-in 없는 비-loopback bind가 성공하면 안 된다"),
            Err(e) => e,
        };
        assert!(format!("{err:#}").contains("allow_non_loopback"), "{err:#}");
    }

    /// TLS 왕복: 올바른 지문으로 attach_tls → SpawnShell 명령이 암호화 채널을 건너 worker에 닿고
    /// ShellSpawned/MuxUpdated/Viewport(재구성된 전체)가 되돌아온다 (평문 왕복의 TLS판, Delta 협상).
    #[test]
    fn tls_attach_명령_이벤트_왕복() {
        let identity = test_identity();
        let fp = identity.fingerprint();
        let server = RemoteRuntimeServer::serve_tls(
            test_backend("tls-roundtrip"),
            "127.0.0.1:0".parse().unwrap(),
            identity,
            false,
        )
        .unwrap();
        let client =
            RemoteRuntimeClient::attach_tls(server.local_addr(), server.auth_token(), &fp).unwrap();
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

        drop(client);
        server.shutdown();
    }

    #[test]
    fn tls_server_disconnect_closes_event_subscription() {
        let identity = test_identity();
        let fingerprint = identity.fingerprint();
        let server = RemoteRuntimeServer::serve_tls(
            test_backend("tls-disconnect"),
            "127.0.0.1:0".parse().unwrap(),
            identity,
            false,
        )
        .unwrap();
        let client =
            RemoteRuntimeClient::attach_tls(server.local_addr(), server.auth_token(), &fingerprint)
                .unwrap();
        let receiver = client.subscribe();

        server.shutdown();

        wait_for_disconnect(&receiver, Duration::from_secs(5));
    }

    /// 지문 불일치 → TLS 핸드셰이크에서 검증기가 거부 → attach Err. 서버는 생존해 올바른 지문의
    /// 재접속을 정상 처리한다.
    #[test]
    fn tls_지문_불일치는_거부되고_서버는_생존() {
        let identity = test_identity();
        let correct_fp = identity.fingerprint();
        let server = RemoteRuntimeServer::serve_tls(
            test_backend("tls-badfp"),
            "127.0.0.1:0".parse().unwrap(),
            identity,
            false,
        )
        .unwrap();

        // 다른 인증서의 지문 = 불일치.
        let wrong_fp = test_identity().fingerprint();
        assert!(
            RemoteRuntimeClient::attach_tls(server.local_addr(), server.auth_token(), &wrong_fp)
                .is_err(),
            "지문 불일치는 attach 실패여야 한다"
        );

        // 서버 생존 확인 — 올바른 지문으로 붙어 명령이 통한다.
        let client =
            RemoteRuntimeClient::attach_tls(server.local_addr(), server.auth_token(), &correct_fp)
                .unwrap();
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
            events
                .iter()
                .any(|e| matches!(e, RuntimeEvent::ShellSpawned { .. }))
        });
        drop(client);
        server.shutdown();
    }

    /// 잘못된 토큰(지문은 맞음) → TLS는 서지만 ServerHello 없이 끊긴다 → attach Err. 서버 생존.
    #[test]
    fn tls_잘못된_토큰은_거부() {
        let identity = test_identity();
        let fp = identity.fingerprint();
        let server = RemoteRuntimeServer::serve_tls(
            test_backend("tls-badtoken"),
            "127.0.0.1:0".parse().unwrap(),
            identity,
            false,
        )
        .unwrap();
        assert!(
            RemoteRuntimeClient::attach_tls(server.local_addr(), "wrong-token", &fp).is_err(),
            "잘못된 토큰은 attach 실패여야 한다"
        );
        // 서버 생존 — 올바른 토큰으로는 붙는다.
        let client =
            RemoteRuntimeClient::attach_tls(server.local_addr(), server.auth_token(), &fp).unwrap();
        drop(client);
        server.shutdown();
    }

    /// 유휴 기간을 건너도 TLS 접속이 살아 있다 — 유휴 후 명령/이벤트 왕복이 계속 성립한다.
    /// (15s heartbeat 프레임 자체는 서버 pump의 write_frame(&[]) 경로와 구조적으로 동일하며
    /// 여기선 liveness만 짧게 확인한다.)
    #[test]
    fn tls_유휴후에도_접속_유지() {
        let identity = test_identity();
        let fp = identity.fingerprint();
        let server = RemoteRuntimeServer::serve_tls(
            test_backend("tls-idle"),
            "127.0.0.1:0".parse().unwrap(),
            identity,
            false,
        )
        .unwrap();
        let client =
            RemoteRuntimeClient::attach_tls(server.local_addr(), server.auth_token(), &fp).unwrap();
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
            events
                .iter()
                .any(|e| matches!(e, RuntimeEvent::ShellSpawned { .. }))
        });
        let first = seen
            .iter()
            .filter(|e| matches!(e, RuntimeEvent::ShellSpawned { .. }))
            .count();

        // 유휴 — 아무것도 오가지 않는 구간.
        std::thread::sleep(Duration::from_secs(1));

        // 유휴 후에도 명령이 통하고 이벤트가 돌아온다 = 접속 살아있음.
        client
            .send_command(RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
            })
            .unwrap();
        wait_for(&rx, &mut seen, Duration::from_secs(10), |events| {
            events
                .iter()
                .filter(|e| matches!(e, RuntimeEvent::ShellSpawned { .. }))
                .count()
                > first
        });
        drop(client);
        server.shutdown();
    }

    #[test]
    fn tls_client_drop_joins_io_liveness_thread() {
        let identity = test_identity();
        let fp = identity.fingerprint();
        let server = RemoteRuntimeServer::serve_tls(
            test_backend("tls-client-drop"),
            "127.0.0.1:0".parse().unwrap(),
            identity,
            false,
        )
        .unwrap();
        let client =
            RemoteRuntimeClient::attach_tls(server.local_addr(), server.auth_token(), &fp).unwrap();
        let done = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&done);
        let handle = std::thread::spawn(move || {
            drop(client);
            flag.store(true, Ordering::SeqCst);
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        while !done.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "TLS client drop did not join");
            std::thread::sleep(Duration::from_millis(20));
        }

        handle.join().unwrap();
        server.shutdown();
    }

    /// slow-consumer 스모크: 한 세션이 대량 출력을 뿜어 이벤트가 서버 쪽에 쌓이는 동안에도
    /// 서버는 두 번째 명령을 계속 읽어 처리한다 (write backpressure가 read를 굶기지 않음, §2.4).
    /// 단일 IO 루프가 매 tick write 진척과 무관하게 read를 먼저 수행하고, 나갈 배치는 앱계층
    /// 큐로 상한(코얼레싱)하기에 데드락이 구조적으로 없다 — 이 테스트는 그 결과를 거칠게 확인한다.
    #[test]
    fn tls_느린_소비자에도_명령이_처리된다() {
        let backend = test_backend_cmd(
            "tls-slow",
            vec![
                "-c".into(),
                "for i in $(seq 1 4000); do echo line $i; done; sleep 5".into(),
            ],
        );
        let identity = test_identity();
        let fp = identity.fingerprint();
        let server = RemoteRuntimeServer::serve_tls(
            backend,
            "127.0.0.1:0".parse().unwrap(),
            identity,
            false,
        )
        .unwrap();
        let client =
            RemoteRuntimeClient::attach_tls(server.local_addr(), server.auth_token(), &fp).unwrap();
        let rx = client.subscribe();

        // 세션 1 — 대량 출력을 뿜는다.
        client
            .send_command(RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines: 1000,
            })
            .unwrap();

        // rx를 드레인하지 않고 이벤트가 쌓이게 둔다 (서버→클라 backpressure 유도).
        std::thread::sleep(Duration::from_millis(500));

        // 이벤트 부하 중에 두 번째 명령을 보낸다 — 서버가 이걸 읽어 처리해야 한다.
        client
            .send_command(RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines: 1000,
            })
            .unwrap();

        // 두 세션 모두 spawn됐음을 확인 = 두 번째 명령이 굶지 않고 처리됨.
        let mut seen = Vec::new();
        wait_for(&rx, &mut seen, Duration::from_secs(15), |events| {
            events
                .iter()
                .filter(|e| matches!(e, RuntimeEvent::ShellSpawned { .. }))
                .count()
                >= 2
        });
        drop(client);
        server.shutdown();
    }

    /// send_command의 backpressure/종료 surface (codex HIGH): 유계 sync_channel이 가득 차면
    /// try_send가 즉시 명시적 Err(블록/silent drop 없음), IO 스레드 종료(Receiver drop) 후에는
    /// 종료 Err. 소비자 없는 transport를 직접 조립해 두 경로를 결정적으로 검증한다.
    #[test]
    fn tls_send_command는_큐포화와_스레드종료를_err로_surface() {
        let (client_sock, _server_sock) = socket_pair();
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(2);
        let client = RemoteRuntimeClient {
            transport: ClientTransport::Tls {
                commands: tx,
                shutdown: client_sock,
            },
            subscribers: Arc::default(),
            connected: Arc::new(AtomicBool::new(true)),
            reader_thread: None,
            codec: Codec::Delta,
        };
        let cmd = || RuntimeCommand::SpawnShell {
            cols: 80,
            rows: 24,
            scrollback_lines: 100,
        };
        // 소비자(IO 스레드)가 없는 상태에서 용량 2를 채운다 — 셋째는 가득참 Err.
        client.send_command(cmd()).unwrap();
        client.send_command(cmd()).unwrap();
        let err = client.send_command(cmd()).unwrap_err();
        assert!(
            format!("{err:#}").contains("가득"),
            "큐 포화는 가득참 Err여야 한다: {err:#}"
        );
        assert_eq!(
            err.downcast_ref::<crate::RuntimeCommandSendError>(),
            Some(&crate::RuntimeCommandSendError::Backpressure)
        );
        // IO 스레드 죽음(Receiver drop) → 이후 send는 종료 Err.
        drop(rx);
        let err = client.send_command(cmd()).unwrap_err();
        assert!(
            format!("{err:#}").contains("종료"),
            "IO 스레드 종료는 종료 Err여야 한다: {err:#}"
        );
        assert_eq!(
            err.downcast_ref::<crate::RuntimeCommandSendError>(),
            Some(&crate::RuntimeCommandSendError::Disconnected)
        );
    }

    #[test]
    fn tls_event_disconnect_is_published_after_command_channel_closes() {
        let (client_sock, _server_sock) = socket_pair();
        let (commands, command_receiver) = std::sync::mpsc::sync_channel::<Vec<u8>>(8);
        let subscribers = Arc::default();
        let connected = Arc::new(AtomicBool::new(true));
        let client = RemoteRuntimeClient {
            transport: ClientTransport::Tls {
                commands,
                shutdown: client_sock,
            },
            subscribers: Arc::clone(&subscribers),
            connected: Arc::clone(&connected),
            reader_thread: None,
            codec: Codec::Delta,
        };
        let events = client.subscribe();
        let disconnect = std::thread::spawn(move || {
            publish_tls_disconnect(command_receiver, subscribers, connected);
        });

        wait_for_disconnect(&events, Duration::from_secs(5));

        std::thread::scope(|scope| {
            let start = Arc::new(std::sync::Barrier::new(9));
            let mut sends = Vec::new();
            for _ in 0..8 {
                let start = Arc::clone(&start);
                let client = &client;
                sends.push(scope.spawn(move || {
                    start.wait();
                    client.send_command(RuntimeCommand::SpawnShell {
                        cols: 80,
                        rows: 24,
                        scrollback_lines: 100,
                    })
                }));
            }
            start.wait();
            for send in sends {
                let err = send.join().unwrap().unwrap_err();
                assert!(format!("{err:#}").contains("종료"), "{err:#}");
            }
        });
        disconnect.join().unwrap();
    }

    /// 명령 폭주 스모크 (codex HIGH): send_command를 대량 호출해 큐를 압박해도(가득참 Err는
    /// backpressure surface로 허용) IO 루프는 read 인터리브를 유지해 이벤트 수신이 계속되고,
    /// 폭주가 끝나면 큐가 회복돼 새 명령 왕복이 성립한다(기아/메모리 무한 성장 없음).
    #[test]
    fn tls_명령_폭주에도_이벤트_수신_지속() {
        let identity = test_identity();
        let fp = identity.fingerprint();
        let server = RemoteRuntimeServer::serve_tls(
            test_backend("tls-flood"),
            "127.0.0.1:0".parse().unwrap(),
            identity,
            false,
        )
        .unwrap();
        let client =
            RemoteRuntimeClient::attach_tls(server.local_addr(), server.auth_token(), &fp).unwrap();
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
            events
                .iter()
                .any(|e| matches!(e, RuntimeEvent::ShellSpawned { .. }))
        });
        let session = seen
            .iter()
            .find_map(|e| match e {
                RuntimeEvent::ShellSpawned { session } => Some(*session),
                _ => None,
            })
            .unwrap();

        // 폭주: Resize를 대량 전송. 가득참 Err는 silent drop이 아니라 backpressure surface —
        // 허용하고 계속 민다(순서 보존, 코얼레싱 없음이 계약).
        for i in 0..3000u16 {
            let _ = client.send_command(RuntimeCommand::Resize {
                session,
                cols: 80 + (i % 3),
                rows: 24,
            });
        }

        // 폭주 후 큐가 배수되면 새 명령이 다시 들어간다(회복) — 가득참 동안은 재시도.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match client.send_command(RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
            }) {
                Ok(()) => break,
                Err(_) => {
                    assert!(
                        Instant::now() < deadline,
                        "폭주 후 send_command가 회복되지 않음"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }

        // 폭주를 건너서도 이벤트 수신이 계속된다 — 두 번째 ShellSpawned 왕복 확인.
        wait_for(&rx, &mut seen, Duration::from_secs(15), |events| {
            events
                .iter()
                .filter(|e| matches!(e, RuntimeEvent::ShellSpawned { .. }))
                .count()
                >= 2
        });
        drop(client);
        server.shutdown();
    }

    /// read cap 검증 (codex HIGH — slow-consumer의 역방향): 서버가 이벤트를 연속 폭주시켜
    /// 클라 소켓이 계속 readable해도, 클라 IO 루프의 read 단계 상한([`TLS_READ_STEPS_MAX`])이
    /// 명령 drain/write 단계를 보장해 폭주 **도중** 보낸 명령이 서버에 도달한다.
    #[test]
    fn tls_이벤트_폭주중에도_클라_명령이_도달() {
        // 세션 1이 대량 출력을 뿜어 서버→클라 이벤트 스트림을 포화시킨다.
        let backend = test_backend_cmd(
            "tls-rev-flood",
            vec![
                "-c".into(),
                "for i in $(seq 1 8000); do echo flood line $i; done; sleep 5".into(),
            ],
        );
        let identity = test_identity();
        let fp = identity.fingerprint();
        let server = RemoteRuntimeServer::serve_tls(
            backend,
            "127.0.0.1:0".parse().unwrap(),
            identity,
            false,
        )
        .unwrap();
        let client =
            RemoteRuntimeClient::attach_tls(server.local_addr(), server.auth_token(), &fp).unwrap();
        let rx = client.subscribe();

        client
            .send_command(RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines: 1000,
            })
            .unwrap();
        // 출력 폭주가 흐르기 시작할 때까지 잠깐 — 이후 명령은 수신 폭주와 경합한다.
        let mut seen = Vec::new();
        wait_for(&rx, &mut seen, Duration::from_secs(10), |events| {
            events
                .iter()
                .any(|e| matches!(e, RuntimeEvent::ShellSpawned { .. }))
        });

        // 폭주 도중 두 번째 명령 — read cap 덕에 클라 IO 루프가 write 단계에 도달해야 한다.
        client
            .send_command(RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines: 1000,
            })
            .unwrap();
        wait_for(&rx, &mut seen, Duration::from_secs(15), |events| {
            events
                .iter()
                .filter(|e| matches!(e, RuntimeEvent::ShellSpawned { .. }))
                .count()
                >= 2
        });
        drop(client);
        server.shutdown();
    }

    /// oversized 명령은 enqueue 전에 즉시 Err(codex MED) — Ok를 받아 놓고 IO 스레드가
    /// write_frame에서 접속을 죽이는 일이 없다. 이후 정상 명령 왕복으로 접속 생존 확인.
    #[test]
    fn tls_초과크기_명령은_err_접속은_생존() {
        let identity = test_identity();
        let fp = identity.fingerprint();
        let server = RemoteRuntimeServer::serve_tls(
            test_backend("tls-oversize"),
            "127.0.0.1:0".parse().unwrap(),
            identity,
            false,
        )
        .unwrap();
        let client =
            RemoteRuntimeClient::attach_tls(server.local_addr(), server.auth_token(), &fp).unwrap();
        let rx = client.subscribe();

        // 인코딩이 MAX_FRAME_BYTES를 넘는 명령 (args에 17MiB 문자열).
        let oversized = RuntimeCommand::SpawnAgent {
            cols: 80,
            rows: 24,
            scrollback_lines: 100,
            agent_config_id: None,
            command: "x".into(),
            args: vec!["a".repeat(MAX_FRAME_BYTES + 1024)],
            env_plain: Vec::new(),
            env_secrets: Vec::new(),
            waiting_regex: None,
            approval_regex: None,
            error_regex: None,
            done_regex: None,
        };
        let err = client.send_command(oversized).unwrap_err();
        assert!(
            format!("{err:#}").contains("상한"),
            "초과 크기는 상한 Err여야 한다: {err:#}"
        );

        // 접속 생존 — 정상 명령이 여전히 왕복한다.
        client
            .send_command(RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
            })
            .unwrap();
        let mut seen = Vec::new();
        wait_for(&rx, &mut seen, Duration::from_secs(10), |events| {
            events
                .iter()
                .any(|e| matches!(e, RuntimeEvent::ShellSpawned { .. }))
        });
        drop(client);
        server.shutdown();
    }
}
