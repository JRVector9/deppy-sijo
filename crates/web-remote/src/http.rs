//! 수제 최소 HTTP/1.1 (GET + Upgrade 판별용) — 서버 프레임워크 의존 없음
//! (auth callback.rs의 미니 HTTP 서버, runtime remote.rs의 수제 프레이밍 관례).
//! 요청 라인/헤더만 파싱한다. body는 읽지 않는다 — GET 전용, 그 외 메서드는 상위에서 405.

use std::borrow::Cow;
use std::io::{BufRead, Write};

/// 요청 head(요청 라인 + 헤더) 총량 상한 — 폭주 할당 방지 (remote.rs MAX_FRAME_BYTES 관례).
pub const MAX_HEAD_BYTES: usize = 8 * 1024;
/// 헤더 개수 상한.
pub const MAX_HEADERS: usize = 64;

/// 파싱된 요청 head. `query`에는 페어링 토큰이 실릴 수 있다 — **로그 금지**.
#[derive(Debug)]
pub struct RequestHead {
    pub method: String,
    /// '?' 앞 경로 (예: "/app.js").
    pub path: String,
    /// '?' 뒤 쿼리 (없으면 빈 문자열).
    pub query: String,
    /// (소문자 이름, 값). 같은 이름의 중복 헤더는 도착 순서대로 저장.
    headers: Vec<(String, String)>,
}

impl RequestHead {
    /// 첫 번째 일치 헤더 값 (`name`은 소문자로 줄 것).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// head 읽기 실패 분류 — 상위가 응답 코드를 정한다.
#[derive(Debug, PartialEq, Eq)]
pub enum HeadError {
    /// EOF/타임아웃/IO 오류 — 응답 없이 접속 종료.
    Closed,
    /// head가 상한(크기/헤더 수)을 초과 — 431.
    TooLarge,
    /// 문법 위반 — 400.
    Malformed,
}

/// 요청 라인과 헤더를 읽는다. 전체 head에 [`MAX_HEAD_BYTES`] 예산을 강제한다 —
/// `take`가 한 줄짜리 폭주 입력(개행 없는 초대형 라인)도 예산에서 끊는다.
pub fn read_request_head<R: BufRead>(reader: &mut R) -> Result<RequestHead, HeadError> {
    // UFCS로 `&mut R`에 대한 Read::take를 강제한다 — 메서드 해석이 `*reader`를 move하지 않게.
    let mut limited = <&mut R as std::io::Read>::take(reader, MAX_HEAD_BYTES as u64);
    let request_line = read_head_line(&mut limited)?;
    let mut parts = request_line.split_whitespace();
    let (Some(method), Some(target), Some(version)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err(HeadError::Malformed);
    };
    if !version.starts_with("HTTP/1.") {
        return Err(HeadError::Malformed);
    }
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path, query),
        None => (target, ""),
    };
    if path.is_empty() {
        return Err(HeadError::Malformed);
    }

    let mut headers = Vec::new();
    loop {
        let line = read_head_line(&mut limited)?;
        if line.is_empty() {
            break; // 빈 줄 = head 끝
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(HeadError::Malformed);
        };
        if headers.len() >= MAX_HEADERS {
            return Err(HeadError::TooLarge);
        }
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
    }
    Ok(RequestHead {
        method: method.to_owned(),
        path: path.to_owned(),
        query: query.to_owned(),
        headers,
    })
}

/// CRLF 한 줄을 읽는다 (LF 단독도 허용). 상한 도달/절단/타임아웃을 구분한다.
fn read_head_line<R: BufRead>(limited: &mut std::io::Take<R>) -> Result<String, HeadError> {
    let mut line = String::new();
    match limited.read_line(&mut line) {
        Ok(0) => Err(HeadError::Closed), // head 도중 EOF
        Ok(_) => {
            if !line.ends_with('\n') {
                // 개행 없이 끝났다 — 예산 소진이면 상한 초과, 아니면 절단
                return Err(if limited.limit() == 0 {
                    HeadError::TooLarge
                } else {
                    HeadError::Closed
                });
            }
            line.truncate(line.trim_end_matches(['\r', '\n']).len());
            Ok(line)
        }
        // read_line은 비UTF-8 입력에 InvalidData를 준다 — HTTP head는 ASCII여야 한다.
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => Err(HeadError::Malformed),
        // 타임아웃(slowloris)/절단 — 응답 없이 종료.
        Err(_) => Err(HeadError::Closed),
    }
}

/// HTTP 응답 (Connection: close 고정 — 접속당 요청 1개, keep-alive 없음).
pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Cow<'static, [u8]>,
}

impl Response {
    pub fn plain(status: u16, body: &'static str) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8",
            body: Cow::Borrowed(body.as_bytes()),
        }
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        431 => "Request Header Fields Too Large",
        501 => "Not Implemented",
        _ => "Error",
    }
}

/// 응답을 쓴다. 보안 헤더는 전 응답 공통 — CSP(`default-src 'self'`, 인라인 스크립트
/// 금지 + `frame-ancestors 'none'`: default-src는 frame-ancestors에 상속되지 않으므로
/// 명시해 타 출처 iframe 임베드(clickjacking)를 차단)와 nosniff. `connect-src 'self'`는
/// P2 대시보드의 동일 출처 WS(wss)를 명시 허용한다 — default-src 상속으로도 되지만 일부
/// WebKit(iOS 타깃) 버전의 ws 스킴 매칭 이슈를 피하려 명시한다. HTTP 캐시는
/// no-cache — 셸 캐싱은 SW가 담당한다(버전 키 갱신은 P3).
pub fn write_response(stream: &mut impl Write, response: &Response) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-cache\r\nX-Content-Type-Options: nosniff\r\nContent-Security-Policy: default-src 'self'; connect-src 'self'; frame-ancestors 'none'\r\n\r\n",
        response.status,
        reason(response.status),
        response.content_type,
        response.body.len()
    )?;
    stream.write_all(&response.body)?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    fn parse(raw: &str) -> Result<RequestHead, HeadError> {
        read_request_head(&mut BufReader::new(raw.as_bytes()))
    }

    #[test]
    fn 요청_라인과_헤더를_파싱한다() {
        let head = parse(
            "GET /app.js?token=abc HTTP/1.1\r\nHost: 127.0.0.1:8737\r\nUpgrade: WebSocket\r\n\r\n",
        )
        .unwrap();
        assert_eq!(head.method, "GET");
        assert_eq!(head.path, "/app.js");
        assert_eq!(head.query, "token=abc");
        // 헤더 이름은 소문자 정규화, 값은 trim
        assert_eq!(head.header("host"), Some("127.0.0.1:8737"));
        assert_eq!(head.header("upgrade"), Some("WebSocket"));
        assert_eq!(head.header("missing"), None);
    }

    #[test]
    fn 쿼리_없는_경로는_빈_쿼리() {
        let head = parse("GET / HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!(head.path, "/");
        assert_eq!(head.query, "");
    }

    #[test]
    fn lf단독_개행도_허용() {
        let head = parse("GET / HTTP/1.1\nHost: localhost\n\n").unwrap();
        assert_eq!(head.header("host"), Some("localhost"));
    }

    #[test]
    fn 기형_요청은_malformed() {
        assert_eq!(parse("GARBAGE\r\n\r\n").unwrap_err(), HeadError::Malformed);
        assert_eq!(
            parse("GET / SPDY/3\r\n\r\n").unwrap_err(),
            HeadError::Malformed
        );
        assert_eq!(
            parse("GET / HTTP/1.1\r\nno-colon-header\r\n\r\n").unwrap_err(),
            HeadError::Malformed
        );
    }

    #[test]
    fn head_도중_eof는_closed() {
        assert_eq!(parse("").unwrap_err(), HeadError::Closed);
        assert_eq!(
            parse("GET / HTTP/1.1\r\nHost: a\r\n").unwrap_err(),
            HeadError::Closed
        );
    }

    #[test]
    fn head_상한_초과는_toolarge() {
        // 개행 없는 초대형 한 줄도 예산에서 끊는다
        let raw = format!("GET /?a={} HTTP/1.1\r\n\r\n", "x".repeat(MAX_HEAD_BYTES));
        assert_eq!(parse(&raw).unwrap_err(), HeadError::TooLarge);
        // 헤더 누적으로 예산을 넘겨도 동일
        let raw = format!(
            "GET / HTTP/1.1\r\nx-pad: {}\r\n\r\n",
            "y".repeat(MAX_HEAD_BYTES)
        );
        assert_eq!(parse(&raw).unwrap_err(), HeadError::TooLarge);
    }

    #[test]
    fn 헤더_개수_상한_초과는_toolarge() {
        let mut raw = String::from("GET / HTTP/1.1\r\n");
        for i in 0..=MAX_HEADERS {
            raw.push_str(&format!("h{i}: v\r\n"));
        }
        raw.push_str("\r\n");
        assert_eq!(parse(&raw).unwrap_err(), HeadError::TooLarge);
    }

    #[test]
    fn 응답은_보안_헤더와_content_length를_싣는다() {
        let mut out = Vec::new();
        write_response(&mut out, &Response::plain(404, "not found")).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("HTTP/1.1 404 Not Found\r\n"), "{text}");
        assert!(text.contains("Content-Length: 9\r\n"), "{text}");
        assert!(text.contains("Connection: close\r\n"), "{text}");
        assert!(
            text.contains(
                "Content-Security-Policy: default-src 'self'; connect-src 'self'; frame-ancestors 'none'\r\n"
            ),
            "{text}"
        );
        assert!(
            text.contains("X-Content-Type-Options: nosniff\r\n"),
            "{text}"
        );
        assert!(text.ends_with("\r\n\r\nnot found"), "{text}");
    }

    #[test]
    fn csp가_frame_ancestors로_iframe_임베드를_차단한다() {
        let mut out = Vec::new();
        write_response(&mut out, &Response::plain(200, "ok")).unwrap();
        let text = String::from_utf8(out).unwrap();
        // default-src는 frame-ancestors에 상속되지 않는다 — clickjacking 방어는 명시가 필수
        assert!(text.contains("frame-ancestors 'none'"), "{text}");
    }
}
