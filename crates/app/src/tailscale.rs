//! Tailscale ts.net 호스트명 자동 감지 (모바일 웹 P1 후속 — 2026-07-11).
//!
//! `tailscale status --json`의 `Self.DNSName`을 읽어 설정의 ts.net 호스트명을 채운다.
//! macOS GUI 앱 설치는 CLI가 PATH에 없으므로(Tailscale.app 내장) 알려진 경로 후보를
//! 순서대로 시도한다. 감지는 설정 페이지 진입/버튼에서만 1회성 스레드로 돈다 — 상주
//! 폴링·타이머 없음(§14 예산 관례). status 출력에는 tailnet 피어 정보가 실리므로
//! 원문을 로그에 남기지 않는다.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

const CLI_TIMEOUT: Duration = Duration::from_secs(5);
const CLI_WAIT_INTERVAL: Duration = Duration::from_millis(5);
const MAX_CLI_STDOUT_BYTES: usize = 1024 * 1024;
const MAX_CLI_STDERR_BYTES: usize = 64 * 1024;
const MAX_DNS_NAME_BYTES: usize = 253;
const MAX_APPROVE_URL_BYTES: usize = 2 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CliRunError {
    SpawnFailed,
    PipeUnavailable,
    ReaderSpawnFailed,
    OutputTooLarge,
    ReadFailed,
    WaitFailed,
    TimedOut,
    ReaderPanicked,
}

struct CliOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    success: bool,
}

impl std::fmt::Debug for CliOutput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CliOutput")
            .field("stdout_bytes", &self.stdout.len())
            .field("stderr_bytes", &self.stderr.len())
            .field("success", &self.success)
            .finish()
    }
}

#[derive(Clone, Copy)]
struct CliLimits {
    timeout: Duration,
    stdout_bytes: usize,
    stderr_bytes: usize,
}

const CLI_LIMITS: CliLimits = CliLimits {
    timeout: CLI_TIMEOUT,
    stdout_bytes: MAX_CLI_STDOUT_BYTES,
    stderr_bytes: MAX_CLI_STDERR_BYTES,
};

/// 감지 결과. 실패를 두 층으로 나눠 설정 UI가 원인별 안내를 보여준다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Detected {
    /// 데몬 실행 중 + MagicDNS 호스트명 확보 (후행 '.' 제거본).
    Hostname(String),
    /// CLI는 찾았지만 미로그인/정지/MagicDNS off 등으로 호스트명이 없다.
    NoHostname,
    /// CLI를 찾지 못했다 — 미설치 또는 알 수 없는 설치 경로.
    CliNotFound,
}

/// CLI 후보 — PATH의 tailscale을 먼저, macOS는 GUI 앱 내장 CLI와 homebrew 경로를
/// 이어서 시도한다 (GUI 앱으로 뜬 프로세스의 PATH에는 homebrew가 없다).
fn cli_candidates() -> Vec<std::path::PathBuf> {
    let mut candidates = vec![std::path::PathBuf::from("tailscale")];
    if cfg!(target_os = "macos") {
        candidates.push("/Applications/Tailscale.app/Contents/MacOS/Tailscale".into());
        candidates.push("/opt/homebrew/bin/tailscale".into());
        candidates.push("/usr/local/bin/tailscale".into());
    }
    candidates
}

/// `status --json` 출력에서 ts.net 호스트명을 뽑는다 (순수 — 단위 테스트 대상).
/// 데몬이 Running이 아니거나 DNSName이 없으면 None.
fn parse_status_json(json: &str) -> Option<String> {
    if json.len() > MAX_CLI_STDOUT_BYTES {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    if value.get("BackendState")?.as_str()? != "Running" {
        return None;
    }
    let dns_name = value.get("Self")?.get("DNSName")?.as_str()?;
    let host = dns_name.trim_end_matches('.');
    (!host.is_empty() && host.len() <= MAX_DNS_NAME_BYTES).then(|| host.to_owned())
}

fn read_bounded(mut reader: impl std::io::Read, max_bytes: usize) -> Result<Vec<u8>, CliRunError> {
    let hard_limit = max_bytes
        .checked_add(1)
        .ok_or(CliRunError::OutputTooLarge)?;
    let mut output = Vec::with_capacity(max_bytes.min(8 * 1024));
    let mut chunk = [0u8; 8 * 1024];
    while output.len() < hard_limit {
        let remaining = hard_limit - output.len();
        let read_len = remaining.min(chunk.len());
        let count = reader
            .read(&mut chunk[..read_len])
            .map_err(|_| CliRunError::ReadFailed)?;
        if count == 0 {
            break;
        }
        output.extend_from_slice(&chunk[..count]);
    }
    if output.len() > max_bytes {
        Err(CliRunError::OutputTooLarge)
    } else {
        Ok(output)
    }
}

#[cfg(test)]
static ACTIVE_PIPE_READERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
struct ActivePipeReader;

#[cfg(test)]
impl ActivePipeReader {
    fn enter() -> Self {
        ACTIVE_PIPE_READERS.fetch_add(1, Ordering::SeqCst);
        Self
    }
}

#[cfg(test)]
impl Drop for ActivePipeReader {
    fn drop(&mut self) {
        ACTIVE_PIPE_READERS.fetch_sub(1, Ordering::SeqCst);
    }
}

fn spawn_pipe_reader<R: std::io::Read + Send + 'static>(
    name: &'static str,
    reader: R,
    max_bytes: usize,
    failed: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
) -> Result<std::thread::JoinHandle<Result<Vec<u8>, CliRunError>>, CliRunError> {
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            #[cfg(test)]
            let _active = ActivePipeReader::enter();
            let result = read_bounded(reader, max_bytes);
            if result.is_err() {
                failed.store(true, Ordering::Release);
            }
            finished.store(true, Ordering::Release);
            result
        })
        .map_err(|_| CliRunError::ReaderSpawnFailed)
}

fn kill_and_reap(child: &mut std::process::Child) -> Result<(), CliRunError> {
    #[cfg(unix)]
    let group_killed = {
        const SIGKILL: std::ffi::c_int = 9;
        unsafe extern "C" {
            fn kill(pid: std::ffi::c_int, signal: std::ffi::c_int) -> std::ffi::c_int;
        }

        std::ffi::c_int::try_from(child.id()).is_ok_and(|process_group| {
            // SAFETY: the child was spawned into a new process group whose id is its pid.
            // A negative pid targets that group and SIGKILL has no borrowed-memory contract.
            unsafe { kill(-process_group, SIGKILL) == 0 }
        })
    };
    #[cfg(not(unix))]
    let group_killed = false;

    if !group_killed {
        let _ = child.kill();
    }
    child.wait().map_err(|_| CliRunError::WaitFailed)?;
    Ok(())
}

fn join_pipe_reader(
    reader: std::thread::JoinHandle<Result<Vec<u8>, CliRunError>>,
) -> Result<Vec<u8>, CliRunError> {
    reader.join().map_err(|_| CliRunError::ReaderPanicked)?
}

fn run_command_bounded(
    program: &Path,
    args: &[&str],
    limits: CliLimits,
) -> Result<CliOutput, CliRunError> {
    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    let mut child = command.spawn().map_err(|_| CliRunError::SpawnFailed)?;
    let Some(stdout) = child.stdout.take() else {
        let _ = kill_and_reap(&mut child);
        return Err(CliRunError::PipeUnavailable);
    };
    let Some(stderr) = child.stderr.take() else {
        drop(stdout);
        let _ = kill_and_reap(&mut child);
        return Err(CliRunError::PipeUnavailable);
    };
    let reader_failed = Arc::new(AtomicBool::new(false));
    let stdout_finished = Arc::new(AtomicBool::new(false));
    let stderr_finished = Arc::new(AtomicBool::new(false));

    let stdout_reader = match spawn_pipe_reader(
        "tailscale-stdout",
        stdout,
        limits.stdout_bytes,
        Arc::clone(&reader_failed),
        Arc::clone(&stdout_finished),
    ) {
        Ok(reader) => reader,
        Err(error) => {
            drop(stderr);
            let _ = kill_and_reap(&mut child);
            return Err(error);
        }
    };
    let stderr_reader = match spawn_pipe_reader(
        "tailscale-stderr",
        stderr,
        limits.stderr_bytes,
        Arc::clone(&reader_failed),
        Arc::clone(&stderr_finished),
    ) {
        Ok(reader) => reader,
        Err(error) => {
            let _ = kill_and_reap(&mut child);
            let _ = join_pipe_reader(stdout_reader);
            return Err(error);
        }
    };

    let started = Instant::now();
    let mut terminal_error = None;
    let mut status = None;
    loop {
        if reader_failed.load(Ordering::Acquire) {
            terminal_error = Some(CliRunError::OutputTooLarge);
            break;
        }
        if status.is_none() {
            match child.try_wait() {
                Ok(Some(exit_status)) => status = Some(exit_status),
                Ok(None) => {}
                Err(_) => {
                    terminal_error = Some(CliRunError::WaitFailed);
                    break;
                }
            }
        }
        if status.is_some()
            && stdout_finished.load(Ordering::Acquire)
            && stderr_finished.load(Ordering::Acquire)
        {
            break;
        }
        if started.elapsed() >= limits.timeout {
            terminal_error = Some(CliRunError::TimedOut);
            break;
        }
        std::thread::sleep(CLI_WAIT_INTERVAL.min(limits.timeout));
    }

    if terminal_error.is_some() && kill_and_reap(&mut child).is_err() {
        terminal_error = Some(CliRunError::WaitFailed);
    }
    let stdout = join_pipe_reader(stdout_reader);
    let stderr = join_pipe_reader(stderr_reader);
    if let Some(error) = terminal_error {
        return Err(match (stdout, stderr, error) {
            (Err(CliRunError::ReadFailed), _, _) | (_, Err(CliRunError::ReadFailed), _) => {
                CliRunError::ReadFailed
            }
            (Err(CliRunError::ReaderPanicked), _, _) | (_, Err(CliRunError::ReaderPanicked), _) => {
                CliRunError::ReaderPanicked
            }
            (Err(CliRunError::OutputTooLarge), _, _) | (_, Err(CliRunError::OutputTooLarge), _) => {
                CliRunError::OutputTooLarge
            }
            _ => error,
        });
    }
    let stdout = stdout?;
    let stderr = stderr?;
    Ok(CliOutput {
        stdout,
        stderr,
        success: status.is_some_and(|status| status.success()),
    })
}

/// CLI 후보를 순서대로 실행해 호스트명을 감지한다. 실행 자체가 실패한 후보(미존재 등)는
/// 건너뛰고, 하나라도 실행됐지만 호스트명이 없으면 [`Detected::NoHostname`].
fn detect() -> Detected {
    let mut cli_found = false;
    for bin in cli_candidates() {
        let output = match run_command_bounded(&bin, &["status", "--json"], CLI_LIMITS) {
            Ok(output) => output,
            Err(CliRunError::SpawnFailed) => continue,
            Err(_) => {
                tracing::debug!("tailscale_status_command_rejected");
                cli_found = true;
                continue;
            }
        };
        cli_found = true;
        let Ok(stdout) = String::from_utf8(output.stdout) else {
            continue;
        };
        if let Some(host) = parse_status_json(&stdout) {
            return Detected::Hostname(host);
        }
    }
    if cli_found {
        Detected::NoHostname
    } else {
        Detected::CliNotFound
    }
}

/// 1회성 감지 스레드를 띄운다 — 완료 시 결과를 보내고 repaint로 UI를 깨운다
/// (agent_detect_worker 관례). 수신측은 try_recv로 폴링 없이 프레임 내에서 확인한다.
pub fn spawn_detect(ctx: egui::Context) -> mpsc::Receiver<Detected> {
    let (tx, rx) = mpsc::sync_channel(1);
    let spawned = std::thread::Builder::new()
        .name("ts-detect".into())
        .spawn(move || {
            if tx.send(detect()).is_ok() {
                ctx.request_repaint();
            }
        });
    if spawned.is_err() {
        // 스레드 생성 실패 → rx가 즉시 Disconnected — 수신측이 시도 종료로 처리한다.
        tracing::warn!("tailscale_detect_worker_spawn_failed");
    }
    rx
}

// ─────────────────────────────────────────────────────────────────────────────
// serve 온보딩 (O1) — 폰 접속의 마지막 관문
//
// 앱 웹서버는 127.0.0.1에만 bind한다(§2.5 — 비-loopback 평문 금지). 폰이 접속하려면
// `tailscale serve`가 HTTPS를 종단해 프록시해야 하는데, 그 설정 여부를 앱이 몰라서
// "QR을 찍어도 안 열린다"가 된다(2026-07-11 실기기에서 겪음). 여기서 진단하고
// 버튼 한 번으로 설정한다. CLI 자동 실행은 하지 않는다 — 사용자 클릭에서만.
// ─────────────────────────────────────────────────────────────────────────────

/// serve 진단 결과. UI는 이 상태별로 **다음 한 걸음만** 보여준다.
#[derive(Clone, PartialEq, Eq)]
pub enum ServeState {
    /// 이 포트로 프록시가 걸려 있다 — 폰 접속 준비 완료.
    Ready,
    /// serve는 걸려 있는데 **다른 포트**를 가리킨다(앱 포트 변경 등) — 재설정 필요.
    WrongPort(u16),
    /// serve 설정이 없다 — 설정 버튼으로 해결.
    NotConfigured,
    /// tailnet에서 Serve/HTTPS 기능이 꺼져 있다 — 관리 콘솔 1회 승인 필요(URL 동봉).
    NotEnabledOnTailnet { approve_url: Option<String> },
    /// CLI 없음/실행 실패 — 진단 불가(문서 안내로 폴백).
    Unknown,
}

impl std::fmt::Debug for ServeState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ready => formatter.write_str("Ready"),
            Self::WrongPort(port) => formatter.debug_tuple("WrongPort").field(port).finish(),
            Self::NotConfigured => formatter.write_str("NotConfigured"),
            Self::NotEnabledOnTailnet { approve_url } => formatter
                .debug_struct("NotEnabledOnTailnet")
                .field("approve_url_present", &approve_url.is_some())
                .finish(),
            Self::Unknown => formatter.write_str("Unknown"),
        }
    }
}

/// `serve status --json`을 파싱해 이 포트로 가는 프록시가 있는지 본다 (순수 — 테스트 대상).
///
/// 형식(v1.98 실측):
/// ```json
/// { "TCP": {"443": {"HTTPS": true}},
///   "Web": {"host:443": {"Handlers": {"/": {"Proxy": "http://127.0.0.1:8737"}}}} }
/// ```
/// 미설정이면 `Web`이 없거나 비어 있다. 텍스트 출력("No serve config") 대신 JSON을
/// 쓰는 이유: 로케일·버전에 덜 민감하다.
fn parse_serve_json(json: &str, port: u16) -> ServeState {
    if json.len() > MAX_CLI_STDOUT_BYTES {
        return ServeState::Unknown;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return ServeState::Unknown;
    };
    let handlers = value
        .get("Web")
        .and_then(|web| web.as_object())
        .into_iter()
        .flat_map(|web| web.values())
        .filter_map(|site| site.get("Handlers")?.as_object())
        .flat_map(|handlers| handlers.values());

    let mut other_port: Option<u16> = None;
    for handler in handlers {
        let Some(proxy) = handler.get("Proxy").and_then(|p| p.as_str()) else {
            continue;
        };
        match proxy_port(proxy) {
            Some(p) if p == port => return ServeState::Ready,
            Some(p) => other_port = Some(p),
            None => {}
        }
    }
    match other_port {
        Some(p) => ServeState::WrongPort(p),
        None => ServeState::NotConfigured,
    }
}

/// "http://127.0.0.1:8737" → 8737. loopback 대상만 인정한다(우리 서버는 loopback bind).
fn proxy_port(proxy: &str) -> Option<u16> {
    let rest = proxy.strip_prefix("http://")?;
    let (host, port) = rest.trim_end_matches('/').rsplit_once(':')?;
    if !matches!(host, "127.0.0.1" | "localhost" | "[::1]") {
        return None;
    }
    port.parse().ok()
}

/// tailnet에 Serve가 비활성일 때 CLI가 안내하는 승인 URL을 뽑는다 (순수).
/// 출력 예: "Serve is not enabled on your tailnet.\nTo enable, visit:\n\n  https://login.tailscale.com/f/serve?node=..."
fn parse_approve_url(text: &str) -> Option<String> {
    text.split_whitespace()
        .find(|token| token.starts_with("https://login.tailscale.com/"))
        .map(|url| url.trim_end_matches(['.', ',']))
        .filter(|url| url.len() <= MAX_APPROVE_URL_BYTES)
        .map(str::to_owned)
}

/// CLI가 "tailnet에 Serve 미활성"이라고 답했는가.
fn is_not_enabled(text: &str) -> bool {
    text.contains("not enabled on your tailnet") || text.contains("Serve is not enabled")
}

/// CLI 후보를 순서대로 실행해 인자를 넘긴다. 실행된 첫 후보의 (stdout, stderr, 성공여부).
fn run_cli(args: &[&str]) -> Option<(String, String, bool)> {
    for bin in cli_candidates() {
        let output = match run_command_bounded(&bin, args, CLI_LIMITS) {
            Ok(output) => output,
            Err(CliRunError::SpawnFailed) => continue,
            Err(_) => {
                tracing::debug!("tailscale_command_rejected");
                return Some((String::new(), String::new(), false));
            }
        };
        let Ok(stdout) = String::from_utf8(output.stdout) else {
            tracing::debug!("tailscale_stdout_utf8_invalid");
            return Some((String::new(), String::new(), false));
        };
        let Ok(stderr) = String::from_utf8(output.stderr) else {
            tracing::debug!("tailscale_stderr_utf8_invalid");
            return Some((String::new(), String::new(), false));
        };
        return Some((stdout, stderr, output.success));
    }
    None
}

/// serve 상태를 진단한다(읽기 전용 — 아무것도 바꾸지 않는다).
fn diagnose_serve(port: u16) -> ServeState {
    let Some((stdout, stderr, ok)) = run_cli(&["serve", "status", "--json"]) else {
        return ServeState::Unknown;
    };
    if !ok {
        // 미활성 tailnet은 status에서도 안내가 나올 수 있다 — 그 경우 승인 URL을 살린다.
        let combined = format!("{stdout}{stderr}");
        if is_not_enabled(&combined) {
            return ServeState::NotEnabledOnTailnet {
                approve_url: parse_approve_url(&combined),
            };
        }
        return ServeState::Unknown;
    }
    parse_serve_json(&stdout, port)
}

/// `tailscale serve --bg <port>`로 프록시를 건다(사용자 클릭에서만 호출).
/// 성공하면 재진단 결과를, tailnet 미활성이면 승인 URL을 담은 상태를 돌려준다.
fn configure_serve(port: u16) -> ServeState {
    let port_arg = port.to_string();
    let Some((stdout, stderr, ok)) = run_cli(&["serve", "--bg", &port_arg]) else {
        return ServeState::Unknown;
    };
    let combined = format!("{stdout}{stderr}");
    if !ok || is_not_enabled(&combined) {
        if is_not_enabled(&combined) {
            return ServeState::NotEnabledOnTailnet {
                approve_url: parse_approve_url(&combined),
            };
        }
        tracing::warn!("tailscale serve 설정 실패");
        return ServeState::Unknown;
    }
    // 설정 직후 재진단 — 실제로 걸렸는지 CLI에 되묻는다(낙관적 성공 표시 금지).
    diagnose_serve(port)
}

/// serve 진단을 1회성 스레드로 돌린다(상주 폴링 없음).
pub fn spawn_serve_check(ctx: egui::Context, port: u16) -> mpsc::Receiver<ServeState> {
    spawn_serve_task(ctx, move || diagnose_serve(port))
}

/// serve 설정을 1회성 스레드로 실행한다(사용자 클릭). 완료 시 재진단 결과가 온다.
pub fn spawn_serve_configure(ctx: egui::Context, port: u16) -> mpsc::Receiver<ServeState> {
    spawn_serve_task(ctx, move || configure_serve(port))
}

fn spawn_serve_task(
    ctx: egui::Context,
    task: impl FnOnce() -> ServeState + Send + 'static,
) -> mpsc::Receiver<ServeState> {
    let (tx, rx) = mpsc::sync_channel(1);
    let spawned = std::thread::Builder::new()
        .name("ts-serve".into())
        .spawn(move || {
            if tx.send(task()).is_ok() {
                ctx.request_repaint();
            }
        });
    if spawned.is_err() {
        tracing::warn!("tailscale_serve_worker_spawn_failed");
    }
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    static PROCESS_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[cfg(unix)]
    fn lock_process_tests() -> std::sync::MutexGuard<'static, ()> {
        PROCESS_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn running_상태의_dnsname을_후행_점_제거로_파싱한다() {
        let json = r#"{
            "BackendState": "Running",
            "Self": { "DNSName": "jr-macbookair.tail02799e.ts.net." }
        }"#;
        assert_eq!(
            parse_status_json(json),
            Some("jr-macbookair.tail02799e.ts.net".to_owned())
        );
    }

    #[test]
    fn 정지_상태나_dnsname_부재는_none() {
        // 데몬 정지 — DNSName이 있어도 신뢰하지 않는다
        let stopped = r#"{
            "BackendState": "Stopped",
            "Self": { "DNSName": "x.ts.net." }
        }"#;
        assert_eq!(parse_status_json(stopped), None);
        // Running이지만 Self/DNSName 없음 (미로그인·MagicDNS off)
        assert_eq!(parse_status_json(r#"{"BackendState":"Running"}"#), None);
        assert_eq!(
            parse_status_json(r#"{"BackendState":"Running","Self":{"DNSName":""}}"#),
            None
        );
        assert_eq!(
            parse_status_json(r#"{"BackendState":"Running","Self":{"DNSName":"."}}"#),
            None
        );
    }

    #[test]
    fn 기형_json은_none() {
        assert_eq!(parse_status_json("not json"), None);
        assert_eq!(parse_status_json("{}"), None);
    }

    // ── serve 온보딩 (O1) ────────────────────────────────────────────────

    /// 2026-07-12 실측(tailscale v1.98) — `serve status --json` 설정된 상태.
    const SERVE_JSON_READY: &str = r#"{
      "TCP": { "443": { "HTTPS": true } },
      "Web": {
        "jr-macbookair.tail02799e.ts.net:443": {
          "Handlers": { "/": { "Proxy": "http://127.0.0.1:8737" } }
        }
      }
    }"#;

    #[test]
    fn serve_json은_같은_포트_프록시를_ready로_본다() {
        assert_eq!(parse_serve_json(SERVE_JSON_READY, 8737), ServeState::Ready);
    }

    #[test]
    fn serve_json은_다른_포트를_wrong_port로_본다() {
        // 앱 포트를 바꿨는데 serve는 옛 포트를 가리키는 상태 — 재설정이 필요하다.
        assert_eq!(
            parse_serve_json(SERVE_JSON_READY, 9000),
            ServeState::WrongPort(8737)
        );
    }

    #[test]
    fn serve_json은_미설정과_기형을_구분한다() {
        // Web 없음 = serve 미설정
        assert_eq!(
            parse_serve_json(r#"{"TCP":{}}"#, 8737),
            ServeState::NotConfigured
        );
        assert_eq!(parse_serve_json("{}", 8737), ServeState::NotConfigured);
        // 기형 JSON = 진단 불가(하드 실패 금지 — 문서 안내로 폴백)
        assert_eq!(parse_serve_json("not json", 8737), ServeState::Unknown);
    }

    #[test]
    fn 비_loopback_프록시는_우리_서버가_아니다() {
        // 다른 서비스가 tailnet에 걸어둔 serve — 우리 포트로 오인하면 안 된다.
        let other = r#"{"Web":{"h:443":{"Handlers":{"/":{"Proxy":"http://192.168.0.5:8737"}}}}}"#;
        assert_eq!(parse_serve_json(other, 8737), ServeState::NotConfigured);
        assert_eq!(proxy_port("http://127.0.0.1:8737"), Some(8737));
        assert_eq!(proxy_port("http://localhost:80"), Some(80));
        assert_eq!(proxy_port("https://127.0.0.1:8737"), None); // http만
        assert_eq!(proxy_port("http://10.0.0.1:8737"), None);
    }

    #[test]
    fn tailnet_미활성_출력에서_승인_url을_뽑는다() {
        // 2026-07-11 실측 출력
        let text = "Serve is not enabled on your tailnet.\nTo enable, visit:\n\n         https://login.tailscale.com/f/serve?node=nD5Sgs4nBW11CNTRL\n";
        assert!(is_not_enabled(text));
        assert_eq!(
            parse_approve_url(text).as_deref(),
            Some("https://login.tailscale.com/f/serve?node=nD5Sgs4nBW11CNTRL")
        );
        // 승인 URL이 없는 출력도 안전하게 처리
        assert_eq!(
            parse_approve_url("Serve is not enabled on your tailnet."),
            None
        );
        assert!(!is_not_enabled(SERVE_JSON_READY));
    }

    #[test]
    fn 후보_경로는_path_우선이고_macos는_gui_앱_cli를_포함한다() {
        let candidates = cli_candidates();
        assert_eq!(candidates[0], std::path::PathBuf::from("tailscale"));
        if cfg!(target_os = "macos") {
            assert!(candidates.iter().any(|p| {
                p.to_string_lossy()
                    .contains("Tailscale.app/Contents/MacOS/Tailscale")
            }));
        }
    }

    #[test]
    fn pipe_바이트_상한은_정확히_허용하고_한_바이트_초과를_거부한다() {
        let exact = vec![b'x'; 64];
        assert_eq!(
            read_bounded(std::io::Cursor::new(exact), 64).unwrap().len(),
            64
        );
        assert_eq!(
            read_bounded(std::io::Cursor::new(vec![b'x'; 65]), 64),
            Err(CliRunError::OutputTooLarge)
        );
    }

    #[test]
    fn projected_fields는_정확한_상한만_허용한다() {
        let exact_host = "x".repeat(MAX_DNS_NAME_BYTES);
        let exact_json = serde_json::json!({
            "BackendState": "Running",
            "Self": {"DNSName": exact_host}
        });
        assert_eq!(
            parse_status_json(&exact_json.to_string()).unwrap().len(),
            MAX_DNS_NAME_BYTES
        );
        let oversized_json = serde_json::json!({
            "BackendState": "Running",
            "Self": {"DNSName": "x".repeat(MAX_DNS_NAME_BYTES + 1)}
        });
        assert_eq!(parse_status_json(&oversized_json.to_string()), None);

        let url_prefix = "https://login.tailscale.com/";
        let exact_url = format!(
            "{url_prefix}{}",
            "x".repeat(MAX_APPROVE_URL_BYTES - url_prefix.len())
        );
        assert_eq!(
            parse_approve_url(&exact_url).as_deref(),
            Some(exact_url.as_str())
        );
        let oversized_url = format!("{exact_url}x");
        assert_eq!(parse_approve_url(&oversized_url), None);
    }

    #[cfg(unix)]
    #[test]
    fn 성공한_명령도_두_reader를_join하고_제한된_결과만_반환한다() {
        let _test_guard = lock_process_tests();
        let output = run_command_bounded(
            Path::new("/bin/sh"),
            &["-c", "printf ok; printf warn >&2"],
            CliLimits {
                timeout: Duration::from_secs(1),
                stdout_bytes: 2,
                stderr_bytes: 4,
            },
        )
        .unwrap();
        assert_eq!(output.stdout, b"ok");
        assert_eq!(output.stderr, b"warn");
        assert!(output.success);
        assert_eq!(ACTIVE_PIPE_READERS.load(Ordering::SeqCst), 0);
    }

    #[cfg(unix)]
    #[test]
    fn stalled_output_timeout은_매번_child와_reader를_회수한다() {
        let _test_guard = lock_process_tests();
        let started = Instant::now();
        for _ in 0..8 {
            let result = run_command_bounded(
                Path::new("/bin/sh"),
                &["-c", "printf partial; (while :; do :; done) & exit 0"],
                CliLimits {
                    timeout: Duration::from_millis(30),
                    stdout_bytes: 64,
                    stderr_bytes: 64,
                },
            );
            assert_eq!(result.unwrap_err(), CliRunError::TimedOut);
            assert_eq!(ACTIVE_PIPE_READERS.load(Ordering::SeqCst), 0);
        }
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[cfg(unix)]
    #[test]
    fn 반복_output_overflow도_process와_reader를_남기지_않는다() {
        let _test_guard = lock_process_tests();
        for _ in 0..8 {
            let result = run_command_bounded(
                Path::new("/bin/sh"),
                &["-c", "(while :; do printf x; done) & exit 0"],
                CliLimits {
                    timeout: Duration::from_secs(1),
                    stdout_bytes: 64,
                    stderr_bytes: 64,
                },
            );
            assert_eq!(result.unwrap_err(), CliRunError::OutputTooLarge);
            assert_eq!(ACTIVE_PIPE_READERS.load(Ordering::SeqCst), 0);
        }
    }

    #[test]
    fn 소스는_무제한_command_output과_결과_channel을_사용하지_않는다() {
        let source = include_str!("tailscale.rs");
        let command_output = [".", "output()"].concat();
        let forbidden_wait_api = ["wait_with_", "output"].concat();
        let unbounded_channel = ["mpsc::", "channel()"].concat();
        let bounded_channel = ["mpsc::sync_", "channel(1)"].concat();
        assert!(!source.contains(&command_output));
        assert!(!source.contains(&forbidden_wait_api));
        assert!(!source.contains(&unbounded_channel));
        assert!(source.matches(&bounded_channel).count() >= 2);
    }

    #[test]
    fn subprocess_error는_원문없이_낮은_cardinality다() {
        assert_eq!(format!("{:?}", CliRunError::TimedOut), "TimedOut");
        assert_eq!(
            format!("{:?}", CliRunError::OutputTooLarge),
            "OutputTooLarge"
        );
        let state = ServeState::NotEnabledOnTailnet {
            approve_url: Some("https://login.tailscale.com/f/serve?node=sensitive".to_owned()),
        };
        let debug = format!("{state:?}");
        assert_eq!(debug, "NotEnabledOnTailnet { approve_url_present: true }");
        assert!(!debug.contains("sensitive"));
    }
}
