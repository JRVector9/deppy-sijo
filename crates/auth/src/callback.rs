//! LocalhostCallbackServer (설계 §9) — 127.0.0.1 loopback에 한 번의
//! authorization redirect를 받는 최소 HTTP 서버.
//! axum/tokio는 v1+ Streamable HTTP 몫 — 콜백 1회 수신엔 std TcpListener면 충분.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};

/// redirect로 돌아온 query 파라미터.
#[derive(Debug, PartialEq)]
pub struct CallbackParams {
    pub code: String,
    pub state: String,
}

/// DCR redirect_uris에 등록하는 고정 loopback 포트 (PR-H4).
/// redirect URI 정확 일치를 요구하는 비스펙 AS 대비 — [`LocalhostCallbackServer::bind`]가
/// 이 포트를 먼저 시도한다. 임의 선정 deppy 고유 포트로, 점유 중이면 임의 포트로 내려간다.
pub const FIXED_CALLBACK_PORT: u16 = 47456;

/// RFC 7591 등록에 병기할 redirect URI 형태: 고정 포트 + 포트 생략
/// (RFC 8252 §7.3 — loopback redirect는 포트 무시 매칭이 원칙이라 임의 포트를 허용).
pub fn registration_redirect_uris() -> [String; 2] {
    [
        format!("http://127.0.0.1:{FIXED_CALLBACK_PORT}/callback"),
        "http://127.0.0.1/callback".to_owned(),
    ]
}

pub struct LocalhostCallbackServer {
    listener: TcpListener,
    redirect_uri: String,
}

impl LocalhostCallbackServer {
    /// loopback에 bind한다. redirect URI는 `http://127.0.0.1:{port}/callback`.
    /// [`FIXED_CALLBACK_PORT`]를 먼저 시도하고(DCR redirect_uris와 정확 일치),
    /// 점유 중이면 임의 포트로 폴백한다.
    pub fn bind() -> anyhow::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", FIXED_CALLBACK_PORT))
            .or_else(|_| TcpListener::bind(("127.0.0.1", 0)))
            .context("callback 서버 bind 실패")?;
        let port = listener.local_addr()?.port();
        Ok(Self {
            listener,
            redirect_uri: format!("http://127.0.0.1:{port}/callback"),
        })
    }

    /// 수동 등록한 데스크톱 OAuth client용 고정 callback. provider 설정에 미리 등록할
    /// 수 있도록 포트 fallback을 허용하지 않고, PKCE provider가 desktop redirect로
    /// 분류하는 `localhost` host를 사용한다.
    pub fn bind_fixed_localhost() -> anyhow::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", FIXED_CALLBACK_PORT))
            .context("고정 OAuth callback 포트 bind 실패")?;
        Ok(Self {
            listener,
            redirect_uri: format!("http://localhost:{FIXED_CALLBACK_PORT}/callback"),
        })
    }

    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// `/callback` 요청이 올 때까지 기다린다 (favicon 등 다른 경로는 404 후 계속).
    /// `expected_state`와 다른 state의 콜백(이전 시도의 잔여 탭 등)은 소비하지 않고
    /// 계속 기다린다 — 정당한 redirect가 늦게 와도 flow가 살아 있도록.
    /// authorization server가 error를 돌려주면 에러로 끝낸다.
    pub fn wait_for_callback(
        self,
        timeout: Duration,
        expected_state: &str,
    ) -> anyhow::Result<CallbackParams> {
        self.wait_for_callback_cancellable(timeout, expected_state, &AtomicBool::new(false))
    }

    /// [`Self::wait_for_callback`]과 같지만 UI가 flow를 취소하면 짧은 polling 주기 안에
    /// listener를 drop한다. 고정 redirect 포트를 쓰는 데스크톱 OAuth에서 취소 직후
    /// 재시도가 이전 listener와 충돌하지 않게 한다.
    pub fn wait_for_callback_cancellable(
        self,
        timeout: Duration,
        expected_state: &str,
        cancelled: &AtomicBool,
    ) -> anyhow::Result<CallbackParams> {
        let deadline = Instant::now() + timeout;
        // accept에 타임아웃이 없으므로 nonblocking + 짧은 sleep 폴링
        self.listener
            .set_nonblocking(true)
            .context("callback 서버 nonblocking 설정 실패")?;
        loop {
            if cancelled.load(Ordering::Acquire) {
                bail!("authorization 취소됨");
            }
            if Instant::now() >= deadline {
                bail!("authorization 대기 시간 초과 ({timeout:?})");
            }
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if let Some(result) = handle_request(stream, expected_state)? {
                        return result;
                    }
                    // /callback이 아니거나 state가 다른 요청 — 계속 대기
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(e).context("callback accept 실패"),
            }
        }
    }
}

/// 한 요청을 처리한다. `/callback`이면 Some(결과), 아니면 404 응답 후 None.
/// state 불일치 콜백도 None (400 응답 후 계속 대기).
fn handle_request(
    stream: TcpStream,
    expected_state: &str,
) -> anyhow::Result<Option<anyhow::Result<CallbackParams>>> {
    // BSD/macOS에서는 nonblocking listener에서 accept한 socket이 nonblocking을
    // 상속할 수 있다 — read가 WouldBlock으로 정당한 redirect를 버리지 않게 복원
    stream
        .set_nonblocking(false)
        .context("callback stream blocking 복원 실패")?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .context("callback read timeout 설정 실패")?;
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return Ok(None); // 끊긴 연결 — 계속 대기
    }
    // "GET /callback?code=..&state=.. HTTP/1.1"
    let path = request_line.split_whitespace().nth(1).unwrap_or("");
    let url = match oauth2::url::Url::parse(&format!("http://localhost{path}")) {
        Ok(url) => url,
        Err(_) => {
            respond(reader.into_inner(), 400, "잘못된 요청입니다.");
            return Ok(None);
        }
    };
    if url.path() != "/callback" {
        respond(reader.into_inner(), 404, "not found");
        return Ok(None);
    }

    let param = |key: &str| {
        url.query_pairs()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.into_owned())
    };
    if let Some(error) = param("error") {
        // state가 이번 flow의 것일 때만 종료 — 잔여 탭/무관 요청이 flow를 죽이지 못하게
        if param("state").as_deref() != Some(expected_state) {
            respond(reader.into_inner(), 400, "state가 일치하지 않습니다.");
            return Ok(None);
        }
        let description = param("error_description").unwrap_or_default();
        respond(
            reader.into_inner(),
            200,
            "연결이 거부되었습니다. 창을 닫아주세요.",
        );
        return Ok(Some(Err(anyhow::anyhow!(
            "authorization 거부: {error} {description}"
        ))));
    }
    match (param("code"), param("state")) {
        (Some(_), Some(state)) if state != expected_state => {
            // 이전 시도의 잔여 브라우저 탭/무관한 로컬 요청 — 이번 flow를 끝내지 않는다
            respond(reader.into_inner(), 400, "state가 일치하지 않습니다.");
            Ok(None)
        }
        (Some(code), Some(state)) => {
            respond(
                reader.into_inner(),
                200,
                "연결되었습니다. 이 창을 닫아주세요.",
            );
            Ok(Some(Ok(CallbackParams { code, state })))
        }
        _ => {
            respond(reader.into_inner(), 400, "code/state 파라미터가 없습니다.");
            Ok(Some(Err(anyhow::anyhow!("callback에 code/state 없음"))))
        }
    }
}

fn respond(mut stream: TcpStream, status: u16, body: &str) {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        _ => "Bad Request",
    };
    let html = format!("<html><body><p>{body}</p></body></html>");
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
        html.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn http_get(addr: &str, path: &str) -> String {
        let mut stream = TcpStream::connect(addr).unwrap();
        write!(stream, "GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
        response
    }

    #[test]
    fn 콜백_왕복과_다른_경로_무시() {
        let server = LocalhostCallbackServer::bind().unwrap();
        let addr = server.redirect_uri().trim_start_matches("http://")
            [..server.redirect_uri().len() - "http://".len() - "/callback".len()]
            .to_owned();
        let handle =
            std::thread::spawn(move || server.wait_for_callback(Duration::from_secs(10), "xyz"));

        // 브라우저의 favicon 요청 같은 다른 경로는 무시하고 계속 기다린다
        let response = http_get(&addr, "/favicon.ico");
        assert!(response.starts_with("HTTP/1.1 404"));

        // 이전 시도의 잔여 탭(다른 state)도 flow를 끝내지 못한다
        let response = http_get(&addr, "/callback?code=stale&state=old-state");
        assert!(response.starts_with("HTTP/1.1 400"));

        let response = http_get(&addr, "/callback?code=abc&state=xyz");
        assert!(response.starts_with("HTTP/1.1 200"));

        let params = handle.join().unwrap().unwrap();
        assert_eq!(
            params,
            CallbackParams {
                code: "abc".to_owned(),
                state: "xyz".to_owned()
            }
        );
    }

    #[test]
    fn error_파라미터는_거부로_끝난다() {
        let server = LocalhostCallbackServer::bind().unwrap();
        let uri = server.redirect_uri().to_owned();
        let addr = uri
            .trim_start_matches("http://")
            .trim_end_matches("/callback")
            .to_owned();
        let handle =
            std::thread::spawn(move || server.wait_for_callback(Duration::from_secs(10), "s"));
        // state 없는/다른 error는 flow를 죽이지 못한다
        let response = http_get(&addr, "/callback?error=access_denied");
        assert!(response.starts_with("HTTP/1.1 400"));
        let response = http_get(&addr, "/callback?error=access_denied&state=other");
        assert!(response.starts_with("HTTP/1.1 400"));
        // 이번 flow의 state가 실린 error만 정당한 거부다
        http_get(&addr, "/callback?error=access_denied&state=s");
        let result = handle.join().unwrap();
        assert!(result.is_err());
        assert!(format!("{:#}", result.unwrap_err()).contains("access_denied"));
    }

    #[test]
    fn 대기_시간_초과() {
        let server = LocalhostCallbackServer::bind().unwrap();
        let result = server.wait_for_callback(Duration::from_millis(120), "s");
        assert!(result.is_err());
    }

    #[test]
    fn 취소하면_callback_listener를_즉시_해제한다() {
        use std::sync::Arc;

        let server = LocalhostCallbackServer::bind().unwrap();
        let port: u16 = oauth2::url::Url::parse(server.redirect_uri())
            .unwrap()
            .port()
            .unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = cancelled.clone();
        let handle = std::thread::spawn(move || {
            server.wait_for_callback_cancellable(
                Duration::from_secs(10),
                "state",
                &worker_cancelled,
            )
        });

        cancelled.store(true, Ordering::Release);
        let error = handle.join().unwrap().unwrap_err();
        assert!(format!("{error:#}").contains("취소"));
        let rebound = TcpListener::bind(("127.0.0.1", port)).unwrap();
        drop(rebound);
    }

    #[test]
    fn 고정_포트_점유_시_임의_포트로_폴백() {
        // 고정 포트를 확보할 때까지 재시도 — 병렬 테스트의 bind()와 무관하게
        // 폴백 경로를 결정적으로 강제한다
        let _occupier = loop {
            match TcpListener::bind(("127.0.0.1", FIXED_CALLBACK_PORT)) {
                Ok(listener) => break listener,
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        };
        let server = LocalhostCallbackServer::bind().unwrap();
        let port: u16 = server
            .redirect_uri()
            .trim_start_matches("http://127.0.0.1:")
            .trim_end_matches("/callback")
            .parse()
            .unwrap();
        assert_ne!(port, FIXED_CALLBACK_PORT);
    }

    #[test]
    fn 등록용_redirect_uri는_고정_포트와_포트_생략_병기() {
        let uris = registration_redirect_uris();
        assert_eq!(
            uris[0],
            format!("http://127.0.0.1:{FIXED_CALLBACK_PORT}/callback")
        );
        assert_eq!(uris[1], "http://127.0.0.1/callback");
    }
}
