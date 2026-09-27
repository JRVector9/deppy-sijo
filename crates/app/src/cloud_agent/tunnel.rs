//! App-owned bundled tunnel companion; no terminal session or AI agent is created.

use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

const MAX_LINE: usize = 16 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Address(String),
    Ready(String),
    Failed(&'static str),
    Stopped,
}

type Updates = Arc<Mutex<(u64, Option<Event>)>>;

pub struct Tunnel {
    cancel: Arc<AtomicBool>,
    updates: Updates,
    observed: u64,
    worker: Option<JoinHandle<()>>,
    #[cfg(test)]
    pid: Arc<AtomicU32>,
}

pub fn companion() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    let directory = executable.parent()?;
    let adjacent = directory.join("deppy-cloudflared");
    if adjacent.is_file() {
        return Some(adjacent);
    }
    // Cargo test binaries live in deps; ordinary apps never search global PATH.
    if directory.file_name().is_some_and(|name| name == "deps") {
        let candidate = directory.parent()?.join("deppy-cloudflared");
        return candidate.is_file().then_some(candidate);
    }
    None
}

impl Tunnel {
    pub fn start(
        path: PathBuf,
        port: u16,
        timeout: Duration,
        wake: impl Fn() + Send + 'static,
    ) -> std::io::Result<Self> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(2)))
            .max_redirects(0)
            .build()
            .into();
        Self::start_using_probe(path, port, timeout, wake, move |host| {
            public_metadata_ready(&agent, host)
        })
    }
    fn start_using_probe(
        path: PathBuf,
        port: u16,
        timeout: Duration,
        wake: impl Fn() + Send + 'static,
        probe: impl Fn(&str) -> bool + Send + 'static,
    ) -> std::io::Result<Self> {
        let cancel = Arc::new(AtomicBool::new(false));
        let updates = Arc::new(Mutex::new((0, None)));
        let pid = Arc::new(AtomicU32::new(0));
        let (stop, output, child_pid) = (cancel.clone(), updates.clone(), pid.clone());
        let worker = std::thread::Builder::new()
            .name("mcp-tunnel".into())
            .spawn(move || {
                let publish = |event| {
                    let mut state = output.lock().unwrap();
                    state.0 += 1;
                    state.1 = Some(event);
                    drop(state);
                    wake();
                };
                let event = run(&path, port, timeout, &stop, child_pid, &publish, &probe);
                publish(event);
            })?;
        Ok(Self {
            cancel,
            updates,
            observed: 0,
            worker: Some(worker),
            #[cfg(test)]
            pid,
        })
    }
    pub fn poll(&mut self) -> Option<Event> {
        let state = self.updates.lock().unwrap();
        if self.observed == state.0 {
            return None;
        }
        self.observed = state.0;
        state.1.clone()
    }
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
    }
    pub fn finished(&self) -> bool {
        self.worker.as_ref().is_none_or(JoinHandle::is_finished)
    }
    pub fn shutdown(&mut self) {
        self.cancel();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
impl Drop for Tunnel {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct ConfigFile(PathBuf);
impl ConfigFile {
    fn create() -> std::io::Result<Self> {
        use std::io::Write;
        let path = std::env::temp_dir().join(format!("deppy-tunnel-{}.yml", uuid::Uuid::new_v4()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let guard = Self(path.clone());
        options.open(&path)?.write_all(b"{}\n")?;
        Ok(guard)
    }
}
impl Drop for ConfigFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn run(
    path: &Path,
    port: u16,
    timeout: Duration,
    stop: &AtomicBool,
    pid: Arc<AtomicU32>,
    publish: &impl Fn(Event),
    probe: &impl Fn(&str) -> bool,
) -> Event {
    let config = match ConfigFile::create() {
        Ok(config) => config,
        Err(_) => return Event::Failed("tunnel_config_failed"),
    };
    if stop.load(Ordering::Acquire) {
        return Event::Stopped;
    }
    let mut command = Command::new(path);
    command
        .args(["tunnel", "--config"])
        .arg(&config.0)
        .args([
            "--no-autoupdate",
            "--url",
            &format!("http://127.0.0.1:{port}"),
            "--protocol",
            "http2",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    // Existing named-tunnel credentials/config must not select another tunnel.
    for (key, _) in std::env::vars_os() {
        if key.to_str().is_some_and(|key| key.starts_with("TUNNEL_")) {
            command.env_remove(key);
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = match command.spawn() {
        Ok(child) => OwnedChild(child),
        Err(_) => return Event::Failed("tunnel_spawn_failed"),
    };
    pid.store(child.0.id(), Ordering::Release);
    let Some(stderr) = child.0.stderr.take() else {
        return Event::Failed("tunnel_output_failed");
    };
    let host = Arc::new(Mutex::new(None));
    let address = host.clone();
    let reader = match std::thread::Builder::new()
        .name("mcp-tunnel-log".into())
        .spawn(move || {
            read_lines(stderr, |line| {
                if let Some(value) = generated_host(line) {
                    let mut slot = address.lock().unwrap();
                    if slot.is_none() {
                        *slot = Some(value);
                    }
                }
            })
        }) {
        Ok(reader) => reader,
        Err(_) => return Event::Failed("tunnel_reader_failed"),
    };
    let mut discovered = None;
    let mut ready = false;
    let mut deadline = Instant::now() + timeout;
    let mut next_probe = Instant::now();
    let outcome = loop {
        if stop.load(Ordering::Acquire) {
            break Event::Stopped;
        }
        match child.0.try_wait() {
            Ok(Some(_)) => break Event::Failed("tunnel_exited"),
            Err(_) => break Event::Failed("tunnel_wait_failed"),
            Ok(None) => {}
        }
        if discovered.is_none()
            && let Some(value) = host.lock().unwrap().clone()
        {
            publish(Event::Address(value.clone()));
            discovered = Some(value);
        }
        if !ready && Instant::now() >= deadline {
            break Event::Failed("tunnel_timeout");
        }
        if Instant::now() >= next_probe {
            if let Some(value) = &discovered {
                let available = probe(value);
                if stop.load(Ordering::Acquire) {
                    break Event::Stopped;
                }
                if available && !ready {
                    publish(Event::Ready(value.clone()));
                }
                if !available && ready {
                    deadline = Instant::now() + timeout;
                    publish(Event::Address(value.clone()));
                }
                ready = available;
            }
            next_probe = Instant::now() + Duration::from_secs(if ready { 10 } else { 1 });
        }
        std::thread::sleep(Duration::from_millis(if ready { 250 } else { 50 }));
    };
    drop(child); // Close the pipe by killing/reaping our exact child before joining reader.
    let _ = reader.join();
    outcome
}

fn public_metadata_ready(agent: &ureq::Agent, host: &str) -> bool {
    let url = format!("https://{host}/.well-known/oauth-protected-resource/mcp");
    let Ok(response) = agent.get(&url).call() else {
        return false;
    };
    if response.status() != 200 {
        return false;
    }
    let mut text = String::new();
    let mut reader = response
        .into_body()
        .into_reader()
        .take((MAX_LINE + 1) as u64);
    reader.read_to_string(&mut text).is_ok()
        && text.len() <= MAX_LINE
        && valid_metadata(&text, host)
}
fn valid_metadata(text: &str, host: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .is_some_and(|value| {
            value["resource"].as_str() == Some(format!("https://{host}/mcp").as_str())
        })
}
fn generated_host(line: &str) -> Option<String> {
    line.split_whitespace().find_map(|word| {
        let host = word.strip_prefix("https://")?;
        let label = host.strip_suffix(".trycloudflare.com")?;
        (!label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'))
        .then(|| host.to_owned())
    })
}
fn read_lines(mut reader: impl Read, mut visit: impl FnMut(&str)) -> std::io::Result<()> {
    let mut line = Vec::with_capacity(1024);
    let mut oversized = false;
    let mut buffer = [0; 4096];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            return Ok(());
        }
        for byte in &buffer[..count] {
            if *byte == b'\n' {
                if !oversized && let Ok(text) = std::str::from_utf8(&line) {
                    visit(text);
                }
                line.clear();
                oversized = false;
            } else if !oversized {
                if line.len() == MAX_LINE {
                    oversized = true;
                    line.clear();
                } else {
                    line.push(*byte);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn oversized_lines_are_drained_and_next_address_is_observed() {
        let text = format!(
            "{} https://ignored.trycloudflare.com\nhttps://good.trycloudflare.com\n",
            "a".repeat(65536)
        );
        let mut hosts = Vec::new();
        read_lines(text.as_bytes(), |line| {
            if let Some(host) = generated_host(line) {
                hosts.push(host);
            }
        })
        .unwrap();
        assert_eq!(hosts, ["good.trycloudflare.com"]);
    }

    #[cfg(unix)]
    fn fixture(script: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!("deppy-tunnel-test-{}", uuid::Uuid::new_v4()));
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_reaps_only_owned_child_before_startup_finishes() {
        let path = fixture("exec /bin/sleep 30");
        let mut tunnel = Tunnel::start(path.clone(), 8739, Duration::from_secs(30), || {}).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while tunnel.pid.load(Ordering::Acquire) == 0 {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        let pid = tunnel.pid.load(Ordering::Acquire);
        tunnel.cancel();
        while !tunnel.finished() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(tunnel.poll(), Some(Event::Stopped));
        drop(tunnel);
        assert_eq!(
            unsafe { libc::kill(pid as i32, 0) },
            -1,
            "helper was not reaped"
        );
        std::fs::remove_file(path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn unexpected_exit_is_a_failure_and_timeout_reaps_child() {
        for (script, expected) in [
            ("exit 0", "tunnel_exited"),
            ("exec /bin/sleep 30", "tunnel_timeout"),
        ] {
            let path = fixture(script);
            // Exit detection must not race the test runner's process scheduling.
            let timeout = if expected == "tunnel_exited" {
                Duration::from_secs(2)
            } else {
                Duration::from_millis(120)
            };
            let mut tunnel = Tunnel::start(path.clone(), 8739, timeout, || {}).unwrap();
            let deadline = Instant::now() + Duration::from_secs(3);
            while !tunnel.finished() {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(tunnel.poll(), Some(Event::Failed(expected)));
            drop(tunnel);
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn local_resource_metadata_is_verified_exactly() {
        assert!(valid_metadata(
            r#"{"resource":"https://abc.trycloudflare.com/mcp"}"#,
            "abc.trycloudflare.com"
        ));
        assert!(!valid_metadata(
            r#"{"resource":"http://127.0.0.1:8739/mcp"}"#,
            "abc.trycloudflare.com"
        ));
        assert!(!valid_metadata(
            r#"{"resource":"https://evil.trycloudflare.com/mcp"}"#,
            "abc.trycloudflare.com"
        ));
    }
    #[cfg(unix)]
    #[test]
    fn healthy_public_probe_survives_connection_unregister_logs() {
        let path = fixture(
            "printf 'https://healthy.trycloudflare.com\\nRegistered tunnel connection\\nUnregistered tunnel connection\\n' >&2\nexec /bin/sleep 30",
        );
        let mut tunnel =
            Tunnel::start_using_probe(path.clone(), 8739, Duration::from_secs(2), || {}, |_| true)
                .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if matches!(tunnel.poll(), Some(Event::Ready(_))) {
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(Duration::from_millis(250));
        assert!(
            !tunnel.finished(),
            "healthy helper was terminated after an unrelated unregister log"
        );
        tunnel.shutdown();
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn accepts_only_generated_https_host_without_paths_or_credentials() {
        assert_eq!(
            generated_host("| https://abc-123.trycloudflare.com |"),
            Some("abc-123.trycloudflare.com".into())
        );
        for line in [
            "https://abc.trycloudflare.com.evil.test",
            "http://abc.trycloudflare.com",
            "https://user@abc.trycloudflare.com",
            "https://abc.trycloudflare.com/path",
            "https://abc.trycloudflare.com?token=secret",
            "https://-bad.trycloudflare.com",
            "https://trycloudflare.com",
        ] {
            assert_eq!(generated_host(line), None, "{line}");
        }
        assert_eq!(
            generated_host(&format!("https://{}.trycloudflare.com", "a".repeat(64))),
            None
        );
    }
}
