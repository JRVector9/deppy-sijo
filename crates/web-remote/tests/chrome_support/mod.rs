//! 격리된 실제 Chromium을 띄우고 러너 문서의 결과를 **loopback HTTP로 되받는** 공용 도우미.
//!
//! `--dump-dom`은 load 직후 페이지를 얼려 IndexedDB·WebCrypto 뒤의 비동기 결과를 놓친다(이 저장소의
//! 실측). 그래서 러너는 `<body data-status="ok|error">`를 찍고, 여기서 끼워 넣는 리포터가 그 순간
//! `/report`로 본문을 보낸다. 정적 파일과 리포트 슬롯뿐인 서버이며 바깥 네트워크는 없다.

#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 러너 문서 끝에 끼워 넣는 리포터. `data-status`가 `ok`/`error`가 되는 순간 본문을 보낸다.
pub const REPORTER_SCRIPT: &str = r#"<script>(() => {
  let sent = false;
  const post = () => {
    const status = document.body.dataset.status;
    if (sent || (status !== "ok" && status !== "error")) return;
    sent = true;
    fetch("/report", { method: "POST", body: `${status}\n${document.body.textContent}`, keepalive: true })
      .catch(() => {});
  };
  new MutationObserver(post).observe(document.body, { attributes: true, attributeFilter: ["data-status"] });
  post();
})();</script>"#;

/// 러너 한 번의 결과 본문(`ok` 상태일 때). 실패 표식이 오면 곧바로 panic한다.
pub fn run_headless(
    chrome: &Path,
    profile: &Path,
    url: &str,
    server: &StaticServer,
    label: &str,
) -> String {
    let stderr_path = profile.join("chrome.stderr");
    let mut child = Command::new(chrome)
        .arg("--headless=new")
        .arg("--disable-gpu")
        .arg("--disable-background-networking")
        .arg("--disable-component-update")
        .arg("--disable-sync")
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg(url)
        .stdout(Stdio::null())
        .stderr(Stdio::from(
            std::fs::File::create(&stderr_path).expect("create Chrome stderr capture"),
        ))
        .spawn()
        .unwrap_or_else(|error| panic!("launch {}: {error}", chrome.display()));
    let deadline = Instant::now() + Duration::from_secs(60);
    let outcome = loop {
        if let Some(report) = server.take_report() {
            break Some(report);
        }
        if let Some(status) = child.try_wait().expect("poll isolated Chrome runner") {
            panic!(
                "{label}: Chrome exited ({status}) before reporting\nstderr:\n{}",
                read_capture(&stderr_path)
            );
        }
        if Instant::now() >= deadline {
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    child.kill().expect("stop isolated Chrome");
    let _ = child.wait();
    let Some(report) = outcome else {
        panic!(
            "{label} timed out without a report\nstderr:\n{}",
            read_capture(&stderr_path)
        );
    };
    let (status, body) = report.split_once('\n').unwrap_or((report.as_str(), ""));
    assert_eq!(status, "ok", "{label} failed in the browser:\n{body}");
    body.to_owned()
}

pub fn read_capture(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_else(|error| format!("<failed to read {}: {error}>", path.display()))
}

pub fn chrome_path() -> Option<PathBuf> {
    for variable in ["CHROME_PATH", "CHROME_BIN"] {
        if let Some(path) = std::env::var_os(variable).map(PathBuf::from) {
            return path.is_file().then_some(path);
        }
    }
    [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/usr/bin/google-chrome",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
    ]
    .into_iter()
    .map(PathBuf::from)
    .find(|path| path.is_file())
}

pub fn require_chrome() -> PathBuf {
    chrome_path().unwrap_or_else(|| {
        panic!("set CHROME_PATH/CHROME_BIN or install Google Chrome/Chromium to run this gate")
    })
}

/// 러너마다 새 프로필. IndexedDB·캐시가 이전 실행에서 넘어오지 않는다.
pub struct BrowserTempDir {
    pub path: PathBuf,
}

impl BrowserTempDir {
    pub fn new(prefix: &str) -> Self {
        let unique = format!(
            "{prefix}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time after Unix epoch")
                .as_nanos()
        );
        let path = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&path).expect("create browser temp dir");
        Self { path }
    }
}

impl Drop for BrowserTempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// 16진 문자열 → 바이트. 러너가 남긴 서명 등을 되읽는다.
pub fn decode_hex(value: &str) -> Result<Vec<u8>, String> {
    if !value.len().is_multiple_of(2) {
        return Err("hex value has odd length".to_owned());
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let pair = std::str::from_utf8(pair).map_err(|error| error.to_string())?;
            u8::from_str_radix(pair, 16).map_err(|error| error.to_string())
        })
        .collect()
}

pub type StaticFiles = HashMap<String, (&'static str, Vec<u8>)>;

/// loopback 정적 파일 서버 + `/report` 슬롯. 표에 없는 경로는 404, 그 밖의 동작은 없다.
pub struct StaticServer {
    pub origin: String,
    report: Arc<Mutex<Option<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl StaticServer {
    pub fn start(files: StaticFiles) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback static server");
        listener
            .set_nonblocking(true)
            .expect("nonblocking static listener");
        let origin = format!("http://{}", listener.local_addr().expect("local addr"));
        let stop = Arc::new(AtomicBool::new(false));
        let report = Arc::new(Mutex::new(None));
        let files = Arc::new(files);
        let thread = {
            let stop = Arc::clone(&stop);
            let report = Arc::clone(&report);
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => serve_one(stream, &files, &report),
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            })
        };
        Self {
            origin,
            report,
            stop,
            thread: Some(thread),
        }
    }

    pub fn take_report(&self) -> Option<String> {
        self.report
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }
}

impl Drop for StaticServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve_one(mut stream: TcpStream, files: &StaticFiles, report: &Mutex<Option<String>>) {
    // macOS의 accepted socket 비차단 상속을 해제해야 분할 HTTP 요청도 끝까지 읽는다.
    if stream.set_nonblocking(false).is_err() {
        return;
    }
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let mut raw = Vec::new();
    let mut buffer = [0u8; 4096];
    let head_end = loop {
        if let Some(end) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
            break Some(end + 4);
        }
        if raw.len() > 64 * 1024 {
            break None;
        }
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => break None,
            Ok(count) => raw.extend_from_slice(&buffer[..count]),
        }
    };
    let Some(head_end) = head_end else {
        return;
    };
    let head = String::from_utf8_lossy(&raw[..head_end]).into_owned();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let target = parts.next().unwrap_or_default();
    let path = target.split('?').next().unwrap_or(target).to_owned();
    let content_length = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);

    let response = if method == "POST" && path == "/report" {
        let mut body = raw[head_end..].to_vec();
        while body.len() < content_length {
            match stream.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(count) => body.extend_from_slice(&buffer[..count]),
            }
        }
        *report
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(String::from_utf8_lossy(&body).into_owned());
        b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n".to_vec()
    } else {
        match files.get(&path) {
            Some((mime, body)) => {
                let mut response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\n\
                     Cache-Control: no-store\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .into_bytes();
                response.extend_from_slice(body);
                response
            }
            None => {
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
            }
        }
    };
    let _ = stream.write_all(&response);
    let _ = stream.flush();
}
