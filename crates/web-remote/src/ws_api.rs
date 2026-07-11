//! WS API 계층 (계획 v3.3 P2 — 승인/상태 대시보드의 전송 계층).
//! [`crate::static_srv`]와 모듈 경계를 분리해 둔다: 방법 B(클라우드 앱 셸) 이전 시
//! static_srv만 교체되고 이 모듈은 데스크톱에 남는다.
//!
//! 흐름: HTTP head는 [`crate::http`]가 이미 파싱했으므로, 여기서는 핸드셰이크 응답(101)만
//! 직접 쓰고 tungstenite `from_raw_socket`으로 프레이밍을 넘겨받는다(tungstenite `accept`가
//! 소켓을 다시 읽으려는 이중 읽기 회피). 첫 프레임은 `{v, token}` 인증(5초·상수시간 비교),
//! 이후 15초 ping으로 idle 절단(serve 프록시)과 keepalive를 겸한다.
//!
//! 소켓은 이 스레드가 단독 소유한다(읽기+쓰기). 브리지 스레드는 소켓을 만지지 않고 발행
//! 스냅샷만 갱신하며, 이 스레드가 tick마다 버전을 비교해 push한다.

use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tungstenite::Message;
use tungstenite::handshake::derive_accept_key;
use tungstenite::protocol::{Role, WebSocket};

use crate::dashboard::DashboardHandle;
use crate::http::{RequestHead, Response};
use crate::protocol::{ClientMsg, PROTOCOL_VERSION, ServerMsg};

/// 첫 프레임(인증) 수신 데드라인 — remote.rs AUTH_TIMEOUT 관례.
const AUTH_TIMEOUT: Duration = Duration::from_secs(5);
/// ping 주기 — serve 프록시 idle 절단 대응 겸 keepalive(remote.rs HEARTBEAT 관례).
const PING_INTERVAL: Duration = Duration::from_secs(15);
/// 소켓 read 데드라인(tick). 이 주기로 깨어 발행 스냅샷 push/ping을 확인한다 —
/// 승인/상태 반영 ≤1s 요건을 여유 있게 만족(≈3 tick/s, 유휴 CPU 미미).
const WS_TICK: Duration = Duration::from_millis(300);
/// 클라이언트 텍스트 프레임 상한(제어 메시지만 — 큰 페이로드 거부). 입력/붙여넣기(P5)가
/// 생기면 별도 상한으로 확장한다.
const MAX_CLIENT_FRAME_BYTES: usize = 64 * 1024;
/// tungstenite 프레임/메시지 크기 상한 — 기본(16MB/64MB) 대신 1MB로 낮춰 인증 전 대용량
/// 프레임의 메모리 점유를 유계로 둔다. 서버 대시보드/승인 프레임은 이보다 훨씬 작다.
const WS_SIZE_CAP: usize = 1024 * 1024;

/// WebSocket 업그레이드 요청인가 (`Upgrade: websocket`).
pub fn is_upgrade_request(head: &RequestHead) -> bool {
    head.header("upgrade")
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
}

/// 업그레이드 요청 하나를 WS로 승격해 대시보드 세션을 처리한다. head는 이미 Host 검증·
/// 파싱을 마친 상태로 넘어온다. 반환 시 접속은 닫힌다(스레드 종료).
pub fn serve(
    stream: TcpStream,
    head: &RequestHead,
    token: &str,
    dashboard: &DashboardHandle,
    stop: &AtomicBool,
) {
    // 핸드셰이크 필수 헤더 검증 — key 부재/버전 불일치는 400.
    let Some(key) = head.header("sec-websocket-key") else {
        write_bad_request(stream);
        return;
    };
    if head
        .header("sec-websocket-version")
        .is_none_or(|v| v.trim() != "13")
    {
        write_bad_request(stream);
        return;
    }
    let accept = derive_accept_key(key.as_bytes());

    let mut stream = stream;
    // 101 Switching Protocols — CSP/캐시 헤더가 붙는 http::write_response와 달리 직접 쓴다.
    let handshake = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    use std::io::Write;
    if stream.write_all(handshake.as_bytes()).is_err() || stream.flush().is_err() {
        return;
    }
    // 장수 접속용 타임아웃 — read는 tick 주기로 깨어나고, write는 응답 안 읽는 피어 방어.
    if stream.set_read_timeout(Some(WS_TICK)).is_err()
        || stream.set_write_timeout(Some(AUTH_TIMEOUT)).is_err()
    {
        return;
    }
    let config = tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(WS_SIZE_CAP))
        .max_frame_size(Some(WS_SIZE_CAP));
    let mut ws = WebSocket::from_raw_socket(stream, Role::Server, Some(config));

    // 1) 인증 — 첫 텍스트 프레임의 토큰을 상수시간 비교. 실패/타임아웃은 close.
    if !authenticate(&mut ws, token, stop) {
        let _ = ws.send(Message::Text(
            ServerMsg::Error {
                message: "unauthorized".into(),
            }
            .encode()
            .into(),
        ));
        let _ = ws.close(None);
        let _ = ws.flush();
        return;
    }

    // 2) 등록(연결 수 +1, 즉시 폴링) — Drop 시 자동 해제. 이후 스냅샷 스트림.
    let _guard = dashboard.register_connection();
    if ws
        .send(Message::Text(
            ServerMsg::Welcome {
                v: PROTOCOL_VERSION,
            }
            .encode()
            .into(),
        ))
        .is_err()
    {
        return;
    }

    stream_loop(&mut ws, dashboard, stop);
    let _ = ws.close(None);
    let _ = ws.flush();
}

/// 첫 프레임에서 토큰을 받아 인증한다. 데드라인 내 유효 토큰이면 true.
fn authenticate(ws: &mut WebSocket<TcpStream>, token: &str, stop: &AtomicBool) -> bool {
    let deadline = Instant::now() + AUTH_TIMEOUT;
    while Instant::now() < deadline {
        if stop.load(Ordering::SeqCst) {
            return false;
        }
        match ws.read() {
            Ok(Message::Text(text)) => {
                if text.len() > MAX_CLIENT_FRAME_BYTES {
                    return false;
                }
                return match ClientMsg::parse(text.as_str()) {
                    Some(ClientMsg::Auth {
                        token: provided, ..
                    }) => crate::static_srv::token_matches(token, provided.as_bytes()),
                    _ => false, // 첫 프레임이 auth가 아니면 거부
                };
            }
            // 인증 전 다른 프레임은 무시하고 계속 기다린다(브라우저 자동 pong 등).
            Ok(Message::Ping(_) | Message::Pong(_) | Message::Binary(_) | Message::Frame(_)) => {}
            Ok(Message::Close(_)) => return false,
            Err(e) if is_would_block(&e) => {} // tick 타임아웃 — 데드라인까지 재시도
            Err(_) => return false,
        }
    }
    false
}

/// 인증 후 스트림 루프: 발행 스냅샷 push + ping + 클라 메시지(resolve) 처리.
fn stream_loop(ws: &mut WebSocket<TcpStream>, dashboard: &DashboardHandle, stop: &AtomicBool) {
    let mut last_dash = 0u64;
    let mut last_appr = 0u64;
    let mut last_ping = Instant::now();

    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }

        // 발행 스냅샷이 갱신됐으면(버전 증가) 전송. 실패(피어 종료)는 루프 종료.
        if let Some((version, json)) = dashboard.dashboard_if_newer(last_dash) {
            if ws.send(Message::Text(json.into())).is_err() {
                return;
            }
            last_dash = version;
        }
        if let Some((version, json)) = dashboard.approvals_if_newer(last_appr) {
            if ws.send(Message::Text(json.into())).is_err() {
                return;
            }
            last_appr = version;
        }

        // keepalive ping(serve 프록시 idle 절단 대응).
        if last_ping.elapsed() >= PING_INTERVAL {
            if ws.send(Message::Ping(Vec::new().into())).is_err() {
                return;
            }
            last_ping = Instant::now();
        }

        // 클라 메시지 한 건 처리(없으면 tick 타임아웃).
        match ws.read() {
            Ok(Message::Text(text)) => {
                if text.len() <= MAX_CLIENT_FRAME_BYTES
                    && let Some(ClientMsg::Resolve {
                        id,
                        allowed,
                        remember,
                    }) = ClientMsg::parse(text.as_str())
                {
                    dashboard.resolve(&id, allowed, remember);
                }
            }
            Ok(Message::Close(_)) => return,
            // Ping은 tungstenite가 자동 pong을 큐잉 → 아래 flush로 내보낸다. 그 외 무시.
            Ok(Message::Ping(_) | Message::Pong(_) | Message::Binary(_) | Message::Frame(_)) => {}
            Err(e) if is_would_block(&e) => {} // tick — 다음 루프에서 push/ping 재확인
            Err(_) => return,
        }
        // 자동 pong·버퍼된 쓰기를 밀어낸다.
        if ws.flush().is_err() {
            return;
        }
    }
}

/// tungstenite 오류가 read 타임아웃(WouldBlock/TimedOut)인가 — tick으로 취급.
fn is_would_block(err: &tungstenite::Error) -> bool {
    matches!(
        err,
        tungstenite::Error::Io(io)
            if io.kind() == std::io::ErrorKind::WouldBlock
                || io.kind() == std::io::ErrorKind::TimedOut
    )
}

/// 핸드셰이크 실패 시 400을 쓰고 접속을 닫는다.
fn write_bad_request(mut stream: TcpStream) {
    let _ =
        crate::http::write_response(&mut stream, &Response::plain(400, "bad websocket upgrade"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    #[test]
    fn upgrade_헤더를_대소문자_무시로_판별한다() {
        let head = crate::http::read_request_head(&mut BufReader::new(
            b"GET /ws HTTP/1.1\r\nUpgrade: WebSocket\r\nConnection: Upgrade\r\n\r\n".as_slice(),
        ))
        .unwrap();
        assert!(is_upgrade_request(&head));

        let head = crate::http::read_request_head(&mut BufReader::new(
            b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n".as_slice(),
        ))
        .unwrap();
        assert!(!is_upgrade_request(&head));
    }

    #[test]
    fn 표준_accept_키_유도() {
        // RFC 6455 예시 벡터 — derive_accept_key 배선 확인.
        assert_eq!(
            derive_accept_key(b"dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }
}
