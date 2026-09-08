mod chrome_support;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

#[test]
fn fragmented_loopback_request_keeps_its_static_response() {
    let mut files = chrome_support::StaticFiles::new();
    files.insert(
        "/probe".to_owned(),
        ("text/plain", b"fixture-ready".to_vec()),
    );
    let server = chrome_support::StaticServer::start(files);
    let mut stream = TcpStream::connect(server.origin.strip_prefix("http://").unwrap()).unwrap();
    stream.set_nodelay(true).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream.write_all(b"GET /probe HTTP/1.1\r\n").unwrap();
    // listener의 accept 폴링 뒤에도 헤더가 아직 덜 도착한 상황이다.
    std::thread::sleep(Duration::from_millis(30));
    let sent = stream.write_all(b"Host: localhost\r\nConnection: close\r\n\r\n");
    let mut response = String::new();
    let received = stream.read_to_string(&mut response);
    assert!(
        sent.is_ok() && received.is_ok() && response.ends_with("fixture-ready"),
        "분할 요청의 응답이 사라졌다: sent={sent:?}, received={received:?}"
    );
}
