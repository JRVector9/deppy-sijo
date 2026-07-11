//! web-remote — 모바일 PWA용 내장 웹서버 (mobile-pwa 계획 v3.3 PR-P1 스캐폴드).
//!
//! 계층 (방법 B(클라우드 앱 셸) 이전 대비 모듈 경계):
//!   - [`http`]: 수제 최소 HTTP/1.1 파서/응답 — 프레임워크 없음 (auth callback.rs 관례)
//!   - [`static_srv`]: 임베드 정적 셸 + 페어링 토큰 게이트 — 이전 시 이 계층만 교체
//!   - [`ws_api`]: WS API 자리 (P2) — P1은 Upgrade 판별 후 501
//!   - [`pairing`]: 페어링 토큰 keyring 영속 + 접속 URL
//!
//! 스레드 모델: tokio 금지 — 단일 accept 스레드 + 접속당 블로킹 스레드
//! (runtime remote.rs spawn_accept 관례). 접속은 요청 1개 처리 후 Connection: close.
//! **OFF(서버 미생성) = 스레드/소켓 0. ON + 접속 0 = accept 블로킹 대기만 (idle CPU 0,
//! egui repaint 유발 없음).**
//!
//! 바인딩: serve 모드(기본) = 127.0.0.1 평문 + `tailscale serve`가 HTTPS 종단.
//! 비-loopback 평문 bind는 거부(remote-tls-delta §2.5) — cert 모드(자체 TLS)는 후속.

use std::io::BufReader;
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Context;

pub mod http;
pub mod pairing;
pub mod static_srv;
pub mod ws_api;

/// 동시 접속 스레드 상한. 초과 접속은 503으로 거부하지 않고 슬롯이 빌 때까지
/// OS backlog에서 대기시킨다 — 브라우저의 병렬 자산 요청(보통 ≤6)이 깨지지 않는다.
pub const MAX_CONNECTIONS: usize = 3;

/// 요청 head 수신 타임아웃 — 침묵 peer(slowloris)가 접속 슬롯을 오래 점유하지 못하게.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// 서버 기동 옵션.
pub struct ServeOptions {
    /// 페어링 토큰 — `/?token=` 게이트가 상수시간 비교로 검증한다.
    pub token: String,
    /// 허용 Host(ts.net 호스트명). None/빈 값이면 loopback 계열 Host만 허용.
    pub allowed_host: Option<String>,
}

/// 접속 스레드들이 공유하는 불변 컨텍스트.
struct ConnCtx {
    token: String,
    /// 소문자 정규화된 허용 호스트명.
    allowed_host: Option<String>,
}

/// 살아있는 접속 하나 — shutdown 시 소켓 종료 + join 대상 (remote.rs ConnEntry 관례).
struct ConnEntry {
    stream: TcpStream,
    handle: JoinHandle<()>,
}

/// 접속 목록 + 슬롯 반납 신호. accept 스레드는 상한 초과 시 Condvar에서 기다린다.
type ConnSet = (Mutex<Vec<ConnEntry>>, Condvar);

/// 실행 중인 웹서버. Drop/shutdown이 accept 루프·접속 스레드를 모두 정리한다.
pub struct WebRemoteServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    accept_thread: Option<JoinHandle<()>>,
    connections: Arc<ConnSet>,
}

impl WebRemoteServer {
    /// serve 모드: 평문 bind. **비-loopback 평문은 거부**(remote-tls-delta §2.5 관례) —
    /// HTTPS 종단은 `tailscale serve` 몫이고, cert 모드(자체 TLS + 비-loopback bind)는
    /// 후속 구현이다(설정 자리만 예약).
    pub fn serve(addr: SocketAddr, options: ServeOptions) -> anyhow::Result<Self> {
        anyhow::ensure!(
            addr.ip().is_loopback(),
            "비-loopback 평문 bind({addr}) 거부 — HTTPS 없이는 열 수 없습니다. \
             tailscale serve(127.0.0.1 프록시)를 쓰거나 cert 모드(후속)를 기다리세요"
        );
        let listener = TcpListener::bind(addr).context("web-remote bind 실패")?;
        let addr = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let connections: Arc<ConnSet> = Arc::new((Mutex::new(Vec::new()), Condvar::new()));
        let ctx = Arc::new(ConnCtx {
            token: options.token,
            allowed_host: options
                .allowed_host
                .map(|host| host.trim().to_ascii_lowercase())
                .filter(|host| !host.is_empty()),
        });
        let accept_thread =
            spawn_accept(listener, ctx, Arc::clone(&stop), Arc::clone(&connections))?;
        tracing::info!(%addr, "web-remote 서버 시작");
        Ok(Self {
            addr,
            stop,
            accept_thread: Some(accept_thread),
            connections,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// 현재 살아있는 접속 스레드 수 (표시/테스트용).
    pub fn active_connections(&self) -> usize {
        self.connections.0.lock().expect("connections lock").len()
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
        let conns = std::mem::take(&mut *lock.lock().expect("connections lock"));
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
                let mut conns = lock.lock().expect("connections lock");
                // 동시 접속 상한: 슬롯이 빌 때까지 대기 — 그동안 새 접속은 OS backlog에
                // 쌓인다(연결 거부 아님). 접속 스레드 종료/shutdown이 notify로 깨운다.
                while conns.len() >= MAX_CONNECTIONS && !stop.load(Ordering::SeqCst) {
                    conns = cvar.wait(conns).expect("connections wait");
                }
                if stop.load(Ordering::SeqCst) {
                    drop(conns);
                    let _ = stream.shutdown(Shutdown::Both);
                    break;
                }
                let handle = match std::thread::Builder::new()
                    .name("web-remote-conn".into())
                    .spawn(move || {
                        handle_connection(stream, &conn_ctx);
                        // 접속 종료 — 자기 항목을 스스로 제거하고 슬롯 반납을 알린다
                        // (자기 join은 데드락이라 remove만 — remote.rs 관례).
                        let id = std::thread::current().id();
                        let (lock, cvar) = &*conn_conns;
                        lock.lock()
                            .expect("connections lock")
                            .retain(|entry| entry.handle.thread().id() != id);
                        cvar.notify_one();
                    }) {
                    Ok(handle) => handle,
                    Err(e) => {
                        drop(conns);
                        tracing::warn!("web-remote 접속 스레드 생성 실패: {e}");
                        continue;
                    }
                };
                conns.push(ConnEntry {
                    stream: shutdown_clone,
                    handle,
                });
            }
        })
        .context("web-remote accept 스레드 생성 실패")
}

/// 접속 하나 = 요청 하나 (Connection: close). head 파싱 → Host 검증 → 라우팅.
fn handle_connection(stream: TcpStream, ctx: &ConnCtx) {
    let peer = stream
        .peer_addr()
        .map(|addr| addr.to_string())
        .unwrap_or_else(|_| "?".to_owned());
    // BSD/macOS에서 accept된 소켓의 blocking 상태를 명시 복원 + head 수신 타임아웃
    // (auth callback.rs 관례 — slowloris 슬롯 점유 방지 겸용).
    if stream.set_nonblocking(false).is_err()
        || stream.set_read_timeout(Some(READ_TIMEOUT)).is_err()
    {
        return;
    }
    let mut reader = BufReader::new(stream);
    let head = match http::read_request_head(&mut reader) {
        Ok(head) => head,
        Err(http::HeadError::Closed) => return, // 침묵/절단 peer — 응답 없이 종료
        Err(http::HeadError::TooLarge) => {
            tracing::warn!(peer, "web-remote: 요청 head 상한 초과 — 431");
            respond(
                reader.into_inner(),
                &http::Response::plain(431, "request head too large"),
                &peer,
                "?",
            );
            return;
        }
        Err(http::HeadError::Malformed) => {
            respond(
                reader.into_inner(),
                &http::Response::plain(400, "bad request"),
                &peer,
                "?",
            );
            return;
        }
    };
    let stream = reader.into_inner();

    // Host 검증 — DNS rebinding 차단 (remote-tls-delta §1.5 Origin 지침의 HTTP 적용).
    if !host_allowed(head.header("host"), ctx.allowed_host.as_deref()) {
        tracing::warn!(peer, host = ?head.header("host"), "web-remote: Host 불일치 — 403");
        respond(
            stream,
            &http::Response::plain(403, "forbidden host"),
            &peer,
            &head.path,
        );
        return;
    }
    // WS 업그레이드는 ws_api 계층 몫 (P1은 자리 응답).
    if ws_api::is_upgrade_request(&head) {
        respond(stream, &ws_api::not_ready_response(), &peer, &head.path);
        return;
    }
    if head.method != "GET" {
        respond(
            stream,
            &http::Response::plain(405, "method not allowed"),
            &peer,
            &head.path,
        );
        return;
    }
    let response = static_srv::respond(&head.path, &head.query, &ctx.token);
    respond(stream, &response, &peer, &head.path);
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
    use std::time::Instant;

    const TEST_TOKEN: &str = "0123456789abcdef0123456789abcdef";

    fn start(allowed_host: Option<&str>) -> WebRemoteServer {
        WebRemoteServer::serve(
            SocketAddr::from(([127, 0, 0, 1], 0)),
            ServeOptions {
                token: TEST_TOKEN.to_owned(),
                allowed_host: allowed_host.map(str::to_owned),
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
    fn upgrade_요청은_501_자리응답() {
        let server = start(None);
        let response = request(
            server.local_addr(),
            "GET /ws HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n",
        );
        assert!(response.starts_with("HTTP/1.1 501"), "{response}");
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
}
