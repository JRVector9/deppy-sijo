//! 모델 목록 조회만 수행한다. 프롬프트/작업 실행은 보내지 않는다.
use crate::agent_launcher::{AgentKind, ModelChoice};
use crate::agent_model_catalog::CatalogLoad;
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

const OUTPUT_MAX: usize = 256 * 1024;
const REQUEST_ID: &str = "deppy-model-catalog";

#[derive(Clone)]
pub(crate) struct ProbeRequest {
    pub(crate) generation: u64,
    pub(crate) kind: AgentKind,
    pub(crate) executable: PathBuf,
    pub(crate) launch_path: Option<String>,
    pub(crate) executable_revision: Option<u64>,
}

pub(crate) struct ProbeResult {
    pub(crate) request: ProbeRequest,
    pub(crate) catalog: CatalogLoad,
    pub(crate) default_model: Option<String>,
}

fn parse_cursor(text: &str) -> Result<(Vec<ModelChoice>, Option<String>), ()> {
    if text.len() > OUTPUT_MAX {
        return Err(());
    }
    let text = strip_ansi(text);
    let mut header = false;
    let mut models = Vec::new();
    let mut default = None;
    for line in text.lines().map(str::trim) {
        if line == "Available models" {
            header = true;
            continue;
        }
        if !header || line.is_empty() {
            continue;
        }
        if line.starts_with("Tip:") {
            break;
        }
        let Some((id, label)) = line.split_once(" - ") else {
            continue;
        };
        if !id.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.:/[]+,=".contains(&b))
        {
            continue;
        }
        let is_default = label.ends_with(" (default)");
        let label = label.strip_suffix(" (default)").unwrap_or(label);
        let Some(choice) = ModelChoice::new(id, label, Vec::new(), None) else {
            continue;
        };
        if models.iter().any(|m: &ModelChoice| m.value() == id) {
            continue;
        }
        if is_default {
            default = Some(id.to_owned());
        }
        if models.len() < 64 {
            models.push(choice);
        } else if is_default {
            models[63] = choice;
        }
    }
    if !header || models.is_empty() {
        return Err(());
    }
    if let Some(index) = default
        .as_deref()
        .and_then(|id| models.iter().position(|m| m.value() == id))
    {
        let choice = models.remove(index);
        models.insert(0, choice);
    }
    Ok((models, default))
}

pub(crate) fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            match chars.next() {
                Some('[') => {
                    for ch in chars.by_ref() {
                        if ('@'..='~').contains(&ch) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    let mut escaped = false;
                    for ch in chars.by_ref() {
                        if ch == '\u{7}' || (escaped && ch == '\\') {
                            break;
                        }
                        escaped = ch == '\u{1b}';
                    }
                }
                _ => {}
            }
        } else {
            out.push(ch);
        }
    }
    out
}

fn claude_response(text: &str) -> Option<serde_json::Value> {
    text.lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|v| {
            v.get("type").and_then(|v| v.as_str()) == Some("control_response")
                && v.pointer("/response/request_id").and_then(|v| v.as_str()) == Some(REQUEST_ID)
        })
}

fn parse_claude(text: &str) -> Result<Vec<ModelChoice>, ()> {
    if text.len() > OUTPUT_MAX {
        return Err(());
    }
    let response = claude_response(text).ok_or(())?;
    if response
        .pointer("/response/subtype")
        .and_then(|v| v.as_str())
        != Some("success")
    {
        return Err(());
    }
    let values = response
        .pointer("/response/response/models")
        .and_then(|v| v.as_array())
        .ok_or(())?;
    let mut models = Vec::new();
    let mut seen = HashSet::new();
    for value in values {
        let Some(id) = value.get("value").and_then(|v| v.as_str()) else {
            continue;
        };
        let label = value
            .get("displayName")
            .and_then(|v| v.as_str())
            .unwrap_or(id);
        let efforts = if value.get("supportsEffort").and_then(|v| v.as_bool()) == Some(true) {
            value
                .get("supportedEffortLevels")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str())
                .filter_map(crate::agent_model_catalog::effort_from_value)
                .fold(Vec::new(), |mut values, effort| {
                    if !values.contains(&effort) {
                        values.push(effort);
                    }
                    values
                })
        } else {
            Vec::new()
        };
        if let Some(choice) = ModelChoice::new(id, label, efforts, None)
            && seen.insert(choice.value().to_owned())
        {
            models.push(choice);
            if models.len() == 64 {
                break;
            }
        }
    }
    if !values.is_empty() && models.is_empty() {
        return Err(());
    }
    Ok(models)
}

pub(crate) fn probe(request: ProbeRequest) -> ProbeResult {
    let deadline = Instant::now() + Duration::from_secs(10);
    let result = (|| {
        if request.executable_revision.is_none()
            || request.executable_revision
                != crate::agent_launcher::executable_revision(&request.executable)
        {
            return Err(());
        }
        let directory = ProbeDirectory::new()?;
        match request.kind {
            AgentKind::Cursor => {
                let text = run_limited(&request, &["--list-models"], None, &directory.0, deadline)?;
                parse_cursor(&text)
            }
            AgentKind::Claude => {
                let help = run_limited(&request, &["--help"], None, &directory.0, deadline)?;
                for flag in [
                    "--safe-mode",
                    "--no-session-persistence",
                    "--strict-mcp-config",
                    "--settings",
                    "--tools",
                ] {
                    if !help.contains(flag) {
                        return Err(());
                    }
                }
                let input = format!(
                    "{{\"type\":\"control_request\",\"request_id\":\"{REQUEST_ID}\",\"request\":{{\"subtype\":\"initialize\",\"hooks\":{{}},\"agents\":{{}},\"skills\":[]}}}}\n"
                );
                let text = run_limited(
                    &request,
                    &[
                        "--safe-mode",
                        "--print",
                        "--verbose",
                        "--output-format",
                        "stream-json",
                        "--input-format",
                        "stream-json",
                        "--no-session-persistence",
                        "--strict-mcp-config",
                        "--mcp-config",
                        "{\"mcpServers\":{}}",
                        "--settings",
                        "{\"disableAllHooks\":true}",
                        "--tools",
                        "",
                    ],
                    Some(input.as_bytes()),
                    &directory.0,
                    deadline,
                )?;
                Ok((parse_claude(&text)?, None))
            }
            _ => Err(()),
        }
    })();
    let result = if request.executable_revision
        == crate::agent_launcher::executable_revision(&request.executable)
    {
        result
    } else {
        Err(())
    };
    let (catalog, default_model) = match result {
        Ok((models, default)) => (CatalogLoad::Ready(models), default),
        Err(()) => (CatalogLoad::Unavailable, None),
    };
    ProbeResult {
        request,
        catalog,
        default_model,
    }
}

struct ProbeDirectory(PathBuf);
impl ProbeDirectory {
    fn new() -> Result<Self, ()> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "deppy-model-probe-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| ())?
                .as_nanos(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&path).map_err(|_| ())?;
        Ok(Self(path))
    }
}
impl Drop for ProbeDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(not(unix))]
fn run_limited(
    _: &ProbeRequest,
    _: &[&str],
    _: Option<&[u8]>,
    _: &std::path::Path,
    _: Instant,
) -> Result<String, ()> {
    Err(())
}

#[cfg(unix)]
fn run_limited(
    request: &ProbeRequest,
    args: &[&str],
    input: Option<&[u8]>,
    directory: &std::path::Path,
    deadline: Instant,
) -> Result<String, ()> {
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    if Instant::now() >= deadline {
        return Err(());
    }
    struct OwnedChild {
        child: std::process::Child,
        reaped: bool,
    }
    impl OwnedChild {
        fn finish(&mut self) -> Result<bool, ()> {
            // 자식을 회수하기 전에 소유한 그룹을 종료해 PID 재사용을 피한다.
            unsafe {
                libc::kill(-(self.child.id() as i32), libc::SIGKILL);
            }
            let status = self.child.wait().map_err(|_| ())?;
            self.reaped = true;
            Ok(status.success())
        }
    }
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            if !self.reaped {
                let _ = self.finish();
            }
        }
    }
    fn nonblocking(file: &impl AsRawFd) -> Result<(), ()> {
        // 유효한 파이프 fd의 기존 플래그를 보존한 채 비차단 읽기를 설정한다.
        unsafe {
            let flags = libc::fcntl(file.as_raw_fd(), libc::F_GETFL);
            if flags < 0
                || libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) < 0
            {
                return Err(());
            }
        }
        Ok(())
    }
    fn drain(reader: &mut impl Read, output: &mut Vec<u8>) -> Result<(), ()> {
        let mut bytes = [0; 8192];
        loop {
            match reader.read(&mut bytes) {
                Ok(0) => return Ok(()),
                Ok(n) => {
                    if output.len() + n > OUTPUT_MAX {
                        return Err(());
                    }
                    output.extend_from_slice(&bytes[..n]);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(()),
            }
        }
    }
    let mut command = Command::new(&request.executable);
    command
        .args(args)
        .current_dir(directory)
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    if let Some(path) = &request.launch_path {
        command.env("PATH", path);
    }
    let mut owned = OwnedChild {
        child: command.spawn().map_err(|_| ())?,
        reaped: false,
    };
    let mut stdout = owned.child.stdout.take().ok_or(())?;
    let mut stderr = owned.child.stderr.take().ok_or(())?;
    nonblocking(&stdout)?;
    nonblocking(&stderr)?;
    if let Some(input) = input {
        owned
            .child
            .stdin
            .as_mut()
            .ok_or(())?
            .write_all(input)
            .map_err(|_| ())?;
    }
    let mut out = Vec::new();
    let mut err = Vec::new();
    loop {
        if Instant::now() >= deadline {
            return Err(());
        }
        let prior_len = out.len();
        drain(&mut stdout, &mut out)?;
        drain(&mut stderr, &mut err)?;
        if input.is_some()
            && out.len() != prior_len
            && let Ok(text) = std::str::from_utf8(&out)
            && claude_response(text).is_some()
        {
            let _ = owned.finish();
            return Ok(text.to_owned());
        }
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        // WNOWAIT로 PID 소유를 유지한 채 종료를 확인하고 finish에서 그룹 종료/회수한다.
        let exited = unsafe {
            if libc::waitid(
                libc::P_PID,
                owned.child.id() as libc::id_t,
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            ) != 0
            {
                return Err(());
            }
            info.assume_init().si_pid() != 0
        };
        if exited {
            let success = owned.finish()?;
            drain(&mut stdout, &mut out)?;
            drain(&mut stderr, &mut err)?;
            return if success {
                String::from_utf8(out).map_err(|_| ())
            } else {
                Err(())
            };
        }
        std::thread::sleep(
            Duration::from_millis(10).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_launcher::ReasoningEffort;

    #[test]
    fn model_probe_cursor_accepts_only_catalog_rows_and_keeps_default() {
        let text = "Available models\n\nauto - Auto (default)\ngrok-4.7-xhigh - Grok 4.7 Extra High\ngrok-4.7-xhigh - duplicate\n\nTip: use --model <id> to switch.\n";
        let (models, default) = parse_cursor(text).unwrap();
        assert_eq!(default.as_deref(), Some("auto"));
        assert_eq!(models.len(), 2);
        assert_eq!(models[1].value(), "grok-4.7-xhigh");
        assert!(models[1].efforts().is_empty());
        for text in ["", "Please sign in", "error - denied", "Available models\n"] {
            assert!(parse_cursor(text).is_err());
        }
    }

    #[test]
    fn model_probe_claude_matches_response_id_and_declared_capabilities() {
        let text = r#"{"type":"control_response","response":{"subtype":"success","request_id":"deppy-model-catalog","response":{"models":[{"value":"new-model","displayName":"New","supportsEffort":true,"supportedEffortLevels":["high","xhigh"]},{"value":"plain","displayName":"Plain"}]}}}"#;
        let models = parse_claude(text).unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(
            models[0].efforts(),
            &[ReasoningEffort::High, ReasoningEffort::XHigh]
        );
        assert!(models[1].efforts().is_empty());
        assert!(parse_claude(&text.replace(REQUEST_ID, "another-request")).is_err());
    }

    #[test]
    fn model_probe_cursor_caps_output_and_promotes_a_late_default() {
        let mut text = String::from("\u{1b}[32mAvailable models\u{1b}[0m\n");
        for i in 0..100 {
            text.push_str(&format!("model-{i} - Model {i}\n"));
        }
        text.push_str("current - Current (default)\n");
        let (models, default) = parse_cursor(&text).unwrap();
        assert_eq!(models.len(), 64);
        assert_eq!(models[0].value(), "current");
        assert_eq!(default.as_deref(), Some("current"));
        assert!(parse_cursor(&"x".repeat(OUTPUT_MAX + 1)).is_err());
        assert!(parse_cursor("Available models\n--bad - Invalid\n").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn model_probe_process_bounds_timeout_and_both_output_streams() {
        let dir = ProbeDirectory::new().unwrap();
        let request = ProbeRequest {
            generation: 1,
            kind: AgentKind::Cursor,
            executable: PathBuf::from("/bin/sh"),
            launch_path: None,
            executable_revision: crate::agent_launcher::executable_revision(std::path::Path::new(
                "/bin/sh",
            )),
        };
        let deadline = || Instant::now() + Duration::from_secs(2);
        assert_eq!(
            run_limited(&request, &["-c", "printf ok"], None, &dir.0, deadline()).unwrap(),
            "ok"
        );
        for script in ["yes x", "yes x >&2", "exit 1"] {
            assert!(run_limited(&request, &["-c", script], None, &dir.0, deadline()).is_err());
        }
        let started = Instant::now();
        assert!(
            run_limited(
                &request,
                &["-c", "sleep 10"],
                None,
                &dir.0,
                started + Duration::from_millis(80)
            )
            .is_err()
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        // 자식이 부모 종료 후 파이프를 잡고 있어도 그룹을 회수해 읽기 스레드가 남지 않는다.
        let started = Instant::now();
        let pid = run_limited(
            &request,
            &["-c", "sleep 10 & printf %s $!"],
            None,
            &dir.0,
            deadline(),
        )
        .unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
        let pid: i32 = pid.parse().unwrap();
        let until = Instant::now() + Duration::from_secs(1);
        while unsafe { libc::kill(pid, 0) } == 0 && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_ne!(
            unsafe { libc::kill(pid, 0) },
            0,
            "조회가 만든 자식 프로세스가 남았다"
        );
    }

    #[test]
    #[ignore = "사용자가 설치한 CLI에서 프롬프트 없는 목록 조회를 수동 검증"]
    fn model_probe_installed_cli_contract() {
        let kind = match std::env::var("DEPPY_MODEL_PROBE_KIND").unwrap().as_str() {
            "cursor" => AgentKind::Cursor,
            "claude" => AgentKind::Claude,
            _ => panic!("지원하지 않는 probe"),
        };
        let executable = PathBuf::from(std::env::var_os("DEPPY_MODEL_PROBE_EXECUTABLE").unwrap());
        let result = probe(ProbeRequest {
            executable_revision: crate::agent_launcher::executable_revision(&executable),
            generation: 1,
            kind,
            executable,
            launch_path: std::env::var("PATH").ok(),
        });
        let CatalogLoad::Ready(models) = result.catalog else {
            panic!("설치 CLI 모델 조회 실패");
        };
        assert!(!models.is_empty());
        assert!(models.len() <= 64);
        eprintln!("{}: {} models", kind.id(), models.len());
    }
}
