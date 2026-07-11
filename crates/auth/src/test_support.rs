//! 테스트 전용 유틸 (cfg(test)): std TcpListener 스레드 목 HTTP 서버 +
//! 인메모리 SecretStore. 프로젝트 관례대로 tokio 없이 sync로만 동작한다.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use secret::{SecretStore, SecretString};

/// 목 서버가 받은 요청 한 건.
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    /// (키 소문자, 값)
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl RecordedRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        let lower = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| *k == lower)
            .map(|(_, v)| v.as_str())
    }
}

/// 핸들러가 돌려줄 응답.
pub struct MockResponse {
    pub status: u16,
    pub content_type: &'static str,
    pub body: String,
}

impl MockResponse {
    pub fn json(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: body.into(),
        }
    }

    pub fn text(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            content_type: "text/plain",
            body: body.into(),
        }
    }
}

/// 요청마다 핸들러를 부르는 초소형 HTTP 서버. Drop 시 스레드를 정리한다.
pub struct MockHttpServer {
    base_url: String,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl MockHttpServer {
    pub fn start(
        handler: impl Fn(&RecordedRequest) -> MockResponse + Send + Sync + 'static,
    ) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("mock 서버 bind");
        listener.set_nonblocking(true).expect("mock nonblocking");
        let port = listener.local_addr().expect("mock addr").port();
        let requests: Arc<Mutex<Vec<RecordedRequest>>> = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let handle = {
            let requests = Arc::clone(&requests);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            if let Some((request, stream)) = read_request(stream) {
                                requests
                                    .lock()
                                    .expect("requests lock")
                                    .push(request.clone());
                                let response = handler(&request);
                                write_response(stream, &response);
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            })
        };
        Self {
            base_url: format!("http://127.0.0.1:{port}"),
            requests,
            stop,
            handle: Some(handle),
        }
    }

    /// `http://127.0.0.1:{port}` (경로 없음).
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// base_url + path.
    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    /// 지금까지 받은 요청 스냅샷 (수신 순서).
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().expect("requests lock").clone()
    }
}

impl Drop for MockHttpServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// 요청 head + Content-Length 본문을 읽는다. 실패하면 None (연결 무시).
fn read_request(stream: TcpStream) -> Option<(RecordedRequest, TcpStream)> {
    // macOS에서 nonblocking listener의 accept 소켓이 nonblocking을 상속할 수 있다
    stream.set_nonblocking(false).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).ok()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_owned();
    let path = parts.next()?.to_owned();

    let mut headers = Vec::new();
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim().to_ascii_lowercase();
            let value = value.trim().to_owned();
            if key == "content-length" {
                content_length = value.parse().unwrap_or(0);
            }
            headers.push((key, value));
        }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).ok()?;
    }
    Some((
        RecordedRequest {
            method,
            path,
            headers,
            body: String::from_utf8_lossy(&body).into_owned(),
        },
        reader.into_inner(),
    ))
}

fn write_response(mut stream: TcpStream, response: &MockResponse) {
    let reason = match response.status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Response",
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        response.status,
        response.content_type,
        response.body.len(),
        response.body
    );
}

/// 인메모리 SecretStore (테스트용).
#[derive(Default)]
pub struct MemStore(Mutex<HashMap<String, String>>);

impl MemStore {
    /// 테스트 준비용 직접 주입.
    pub fn seed(&self, id: &str, value: &str) {
        self.0
            .lock()
            .expect("mem store lock")
            .insert(id.to_owned(), value.to_owned());
    }

    /// 저장된 평문 조회 (없으면 None).
    pub fn value(&self, id: &str) -> Option<String> {
        self.0.lock().expect("mem store lock").get(id).cloned()
    }
}

impl SecretStore for MemStore {
    fn set_secret(&self, id: &str, secret: &SecretString) -> anyhow::Result<()> {
        self.seed(id, secret.expose());
        Ok(())
    }

    fn get_secret(&self, id: &str) -> anyhow::Result<SecretString> {
        self.value(id)
            .map(SecretString::new)
            .ok_or_else(|| anyhow::anyhow!("no entry: {id}"))
    }

    fn delete_secret(&self, id: &str) -> anyhow::Result<()> {
        self.0.lock().expect("mem store lock").remove(id);
        Ok(())
    }

    fn has_secret(&self, id: &str) -> anyhow::Result<bool> {
        Ok(self.value(id).is_some())
    }
}
