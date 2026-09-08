#![cfg(unix)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        // assertion 실패 때도 이 테스트가 만든 프로세스만 정리한다.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_server() -> (Server, TcpStream) {
    let address = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    // 루프백 테스트 전용 공개 값이며 실제 배포 자격증명이 아니다.
    let routes = format!("{}:{}", "00".repeat(16), "00".repeat(32));
    let mut server = Server(
        Command::new(env!("CARGO_BIN_EXE_relay-server"))
            .env("DEPPY_RELAY_BIND", address.to_string())
            .env("DEPPY_RELAY_ROUTES", routes)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    let stream = loop {
        if let Ok(stream) = TcpStream::connect(address) {
            break stream;
        }
        assert!(server.0.try_wait().unwrap().is_none(), "서버 시작 실패");
        assert!(Instant::now() < deadline, "서버 시작 시간 초과");
        std::thread::sleep(Duration::from_millis(10));
    };
    stream.set_nodelay(true).unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    (server, stream)
}

#[test]
fn fragmented_http_handshake_survives_the_accept_poll() {
    let (_server, mut stream) = start_server();
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    stream.write_all(b"GET / HTTP/1.1\r\nHost: ").unwrap();
    std::thread::sleep(Duration::from_millis(150));
    let sent = stream.write_all(b"localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n");
    let mut response = [0; 12];
    let received = stream.read_exact(&mut response);
    assert!(
        sent.is_ok() && received.is_ok() && &response == b"HTTP/1.1 101",
        "분할 헤더가 거절됐다: sent={sent:?}, received={received:?}"
    );
}

#[test]
fn malformed_admission_receives_a_rejection_before_eof() {
    let (_server, stream) = start_server();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let (mut socket, _) = tungstenite::client("ws://localhost/", stream).unwrap();
    socket
        .send(tungstenite::Message::Binary(vec![0; 8].into()))
        .unwrap();
    let response = socket.read().expect("EOF 전에 거절 프레임이 와야 한다");
    let tungstenite::Message::Binary(bytes) = response else {
        panic!("이진 거절 프레임이 아니다");
    };
    let (frame, _) = relay_protocol::RelayFrame::decode(&bytes).unwrap();
    assert_eq!(
        frame.rejection_code(),
        Some(relay_protocol::RejectionCode::MalformedFrame)
    );
    assert!(matches!(
        socket.read(),
        Ok(tungstenite::Message::Close(_))
            | Err(tungstenite::Error::ConnectionClosed
                | tungstenite::Error::AlreadyClosed
                | tungstenite::Error::Protocol(
                    tungstenite::error::ProtocolError::ResetWithoutClosingHandshake
                ))
    ));
}

#[test]
fn sigterm_stops_a_worker_receiving_a_slow_http_handshake() {
    let (mut server, mut stream) = start_server();
    stream.write_all(b"GET / HTTP/1.1\r\nHost: ").unwrap();
    // accept 폴링을 통과시키되 읽기 idle 시한(250ms) 전에 다음 바이트를 보낸다.
    std::thread::sleep(Duration::from_millis(150));
    stream.write_all(b"a").unwrap();
    // SAFETY: 이 테스트가 소유한 자식 PID에만 정상 종료 신호를 보낸다.
    assert_eq!(
        unsafe { libc::kill(server.0.id() as libc::pid_t, libc::SIGTERM) },
        0
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    let status = loop {
        if let Some(status) = server.0.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() >= deadline {
            break None;
        }
        // 헤더를 끝내지 않고 idle 시한을 계속 갱신한다.
        let _ = stream.write_all(b"a");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(
        status.is_some_and(|status| status.success()),
        "핸드셰이크 중 SIGTERM 종료가 멎었다: {status:?}"
    );
}
