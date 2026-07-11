//! WS API 계층 자리 (P2) — [`crate::static_srv`]와 모듈 경계를 분리해 둔다.
//! 방법 B(클라우드 앱 셸) 이전 시 static_srv만 교체되고 이 모듈은 데스크톱에 남는다.
//!
//! P1은 Upgrade 판별 + 501 응답만 제공한다. P2에서 tungstenite(sync) 업그레이드,
//! 접속당 스레드, JSON 프로토콜(첫 프레임 토큰 인증·15s ping)이 여기에 온다.

use crate::http::{RequestHead, Response};

/// WebSocket 업그레이드 요청인가 (`Upgrade: websocket`).
pub fn is_upgrade_request(head: &RequestHead) -> bool {
    head.header("upgrade")
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
}

/// P1 자리 응답 — WS API는 P2에서 온다.
pub fn not_ready_response() -> Response {
    Response::plain(501, "WS API not available yet (P2)")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    #[test]
    fn upgrade_헤더를_대소문자_무시로_판별한다() {
        let head = crate::http::read_request_head(&mut BufReader::new(
            b"GET / HTTP/1.1\r\nUpgrade: WebSocket\r\nConnection: Upgrade\r\n\r\n".as_slice(),
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
    fn 자리_응답은_501() {
        assert_eq!(not_ready_response().status, 501);
    }
}
