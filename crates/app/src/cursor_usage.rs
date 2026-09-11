//! Cursor 구독 사용량 — 공식 CLI의 `/usage` 화면을 격리 PTY에서 읽는다.
//!
//! Cursor 개인 플랜은 주간 창 대신 월간 결제 주기의 Included 사용률을 제공한다. 이
//! 모듈은 그 값을 주간 사용률로 바꾸지 않고 그대로 보존한다. CLI가 이미 인증해 그린
//! 화면만 읽으며 Cursor 토큰·로컬 DB·비공개 HTTP/RPC는 건드리지 않는다.

use std::path::Path;
use std::sync::{Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use pty::PtyBackend as _;

const REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);
const STALE_AFTER: Duration = Duration::from_secs(15 * 60);
const PROBE_TIMEOUT: Duration = Duration::from_secs(25);
const STARTUP_DELAY: Duration = Duration::from_secs(2);
const COMMAND_CONFIRM_DELAY: Duration = Duration::from_millis(700);
const SETTLE_DELAY: Duration = Duration::from_secs(2);
const MAX_OUTPUT_BYTES: usize = 100_000;
const DETAIL_MAX_CHARS: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CursorUsage {
    pub(crate) included_percent_used: u8,
    pub(crate) auto_percent_used: Option<u8>,
    pub(crate) api_percent_used: Option<u8>,
    pub(crate) plan_name: Option<String>,
    pub(crate) reset_label: Option<String>,
    pub(crate) on_demand_enabled: Option<bool>,
}

type CompletedProbe = Option<(Instant, CursorUsage)>;

#[derive(Default)]
struct UsageState {
    usage: Option<(Instant, CursorUsage)>,
    pending: Option<mpsc::Receiver<CompletedProbe>>,
    last_request: Option<Instant>,
}

pub(crate) fn current(
    ctx: &egui::Context,
    agent: Option<&crate::agent_launcher::DetectedAgent>,
) -> Option<CursorUsage> {
    static STATE: OnceLock<Mutex<UsageState>> = OnceLock::new();
    let agent = agent?;
    let state = STATE.get_or_init(|| Mutex::new(UsageState::default()));
    let Ok(mut state) = state.lock() else {
        return None;
    };

    receive_pending_probe(&mut state);
    let refresh_due = state
        .last_request
        .is_none_or(|requested| requested.elapsed() >= REFRESH_INTERVAL);
    if state.pending.is_none() && refresh_due {
        let executable = agent.executable().to_path_buf();
        let detected_launch_path = agent.launch_path().map(std::ffi::OsString::from);
        let (sender, receiver) = mpsc::sync_channel(1);
        let repaint = ctx.clone();
        let attempted = Instant::now();
        let spawned = std::thread::Builder::new()
            .name("cursor-usage-probe".to_owned())
            .spawn(move || {
                let usage = fetch_cursor_usage(&executable, detected_launch_path.as_deref())
                    .ok()
                    .flatten();
                let _ = sender.send(usage.map(|usage| (Instant::now(), usage)));
                repaint.request_repaint();
            })
            .is_ok();
        state.last_request = Some(attempted);
        if spawned {
            state.pending = Some(receiver);
        }
    }

    let (measured_at, usage) = state.usage.as_ref()?;
    (measured_at.elapsed() <= STALE_AFTER).then(|| usage.clone())
}

fn receive_pending_probe(state: &mut UsageState) {
    let Some(receiver) = state.pending.as_ref() else {
        return;
    };
    match receiver.try_recv() {
        Ok(Some(completed)) => {
            state.usage = Some(completed);
            state.pending = None;
        }
        Ok(None) | Err(mpsc::TryRecvError::Disconnected) => state.pending = None,
        Err(mpsc::TryRecvError::Empty) => {}
    }
}

fn fetch_cursor_usage(
    executable: &Path,
    detected_launch_path: Option<&std::ffi::OsStr>,
) -> anyhow::Result<Option<CursorUsage>> {
    let probe_dir = crate::paths::home_dir()
        .map(|home| home.join(".deppy-sijo").join("usage-probe"))
        .unwrap_or_else(std::env::temp_dir);
    std::fs::create_dir_all(&probe_dir)?;
    let command = cursor_probe_command(executable, probe_dir, detected_launch_path)?;
    let backend = pty::PortablePtyBackend;
    let mut session = backend.spawn(&command, 120, 40)?;
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let result = run_cursor_usage_probe(session.as_mut(), deadline);
    let kill_result = session.kill();
    match (result, kill_result) {
        (Ok(usage), Ok(())) => Ok(usage),
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
    }
}

fn cursor_probe_command(
    executable: &Path,
    probe_dir: std::path::PathBuf,
    detected_launch_path: Option<&std::ffi::OsStr>,
) -> anyhow::Result<pty::CommandSpec> {
    let (program, mut args) =
        crate::provider_usage_command::probe_program_and_args(executable, cfg!(windows), "Cursor")?;
    // 격리 디렉터리만 신뢰한다. 사용자 프로젝트의 trust 상태는 읽거나 바꾸지 않는다.
    args.push("--trust".to_owned());
    let search_path = if let Some(path) = detected_launch_path {
        path.to_os_string()
    } else {
        let mut paths = executable
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .into_iter()
            .map(Path::to_path_buf)
            .collect::<Vec<_>>();
        if let Some(path) = std::env::var_os("PATH") {
            paths.extend(std::env::split_paths(&path));
        }
        std::env::join_paths(paths)
            .map_err(|_| anyhow::anyhow!("Cursor probe PATH could not be constructed"))?
    }
    .into_string()
    .map_err(|_| anyhow::anyhow!("Cursor probe PATH is not UTF-8"))?;

    Ok(pty::CommandSpec {
        program,
        args,
        env: vec![
            ("TERM".to_owned(), "xterm-256color".to_owned()),
            ("PATH".to_owned(), search_path),
        ],
        cwd: Some(probe_dir),
    })
}

fn run_cursor_usage_probe(
    session: &mut dyn pty::PtySession,
    deadline: Instant,
) -> anyhow::Result<Option<CursorUsage>> {
    let output = session
        .take_output()
        .ok_or_else(|| anyhow::anyhow!("Cursor usage PTY output unavailable"))?;
    std::thread::sleep(STARTUP_DELAY.min(deadline.saturating_duration_since(Instant::now())));
    if Instant::now() >= deadline {
        return Ok(None);
    }
    write_required_input(session, b"/usage\r")?;

    let mut confirm_at = Some(Instant::now() + COMMAND_CONFIRM_DELAY);
    let mut settle_at = None;
    let mut bytes = Vec::new();
    while Instant::now() < deadline {
        let now = Instant::now();
        if confirm_at.is_some_and(|at| now >= at) {
            // 첫 Enter는 명령 팔레트에서 `/usage`를 고르고, 두 번째 Enter가 연다.
            write_required_input(session, b"\r")?;
            confirm_at = None;
        }

        let wait = deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(100));
        if wait.is_zero() {
            break;
        }
        match output.recv_timeout(wait) {
            Ok(chunk) => {
                bytes.extend_from_slice(&chunk);
                if bytes.len() > MAX_OUTPUT_BYTES {
                    bytes.drain(..bytes.len() - MAX_OUTPUT_BYTES);
                }
                let clean = strip_terminal_control_sequences(&String::from_utf8_lossy(&bytes));
                if settle_at.is_none() && usage_panel_rendered(&clean) {
                    settle_at = Some(Instant::now() + SETTLE_DELAY);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if settle_at.is_some_and(|at| Instant::now() >= at) {
            break;
        }
    }

    Ok(parse_usage(&String::from_utf8_lossy(&bytes)))
}

fn write_required_input(session: &mut dyn pty::PtySession, bytes: &[u8]) -> anyhow::Result<()> {
    let result = session.write_input(bytes)?;
    anyhow::ensure!(
        result.is_accepted(),
        "Cursor usage PTY input was not accepted"
    );
    Ok(())
}

fn usage_panel_rendered(output: &str) -> bool {
    let compact = compact_label(output);
    compact.contains("monthlyplanandondemandusage")
        || compact.contains("included") && compact.contains("used")
        || compact.contains("failedtoloadusage")
}

fn parse_usage(output: &str) -> Option<CursorUsage> {
    let clean = strip_terminal_control_sequences(output);
    let lines = clean.split(['\r', '\n']).collect::<Vec<_>>();
    Some(CursorUsage {
        included_percent_used: extract_percent_after_label(&lines, "included")?,
        auto_percent_used: extract_percent_after_label(&lines, "auto"),
        api_percent_used: extract_percent_after_label(&lines, "api"),
        plan_name: extract_plan_name(&lines),
        reset_label: extract_reset_label(&lines),
        on_demand_enabled: extract_on_demand(&lines),
    })
}

fn extract_percent_after_label(lines: &[&str], label: &str) -> Option<u8> {
    static PERCENT: OnceLock<regex::Regex> = OnceLock::new();
    let percent = PERCENT.get_or_init(|| {
        regex::Regex::new(r"(?i)(\d{1,3})(?:\.\d+)?\s*%\s*used")
            .expect("static Cursor percent regex")
    });
    for (index, line) in lines.iter().enumerate().rev() {
        let compact = compact_label(line);
        if !compact.contains(label) {
            continue;
        }
        for (offset, candidate) in lines.iter().skip(index).take(3).enumerate() {
            if offset > 0 && is_usage_section_label(&compact_label(candidate)) {
                break;
            }
            if let Some(captures) = percent.captures(candidate) {
                return captures
                    .get(1)?
                    .as_str()
                    .parse::<u8>()
                    .ok()
                    .map(|value| value.min(100));
            }
        }
    }
    None
}

fn is_usage_section_label(compact: &str) -> bool {
    ["included", "auto", "api", "ondemand"]
        .into_iter()
        .any(|label| compact == label || compact.starts_with(label))
}

fn extract_plan_name(lines: &[&str]) -> Option<String> {
    for line in lines.iter().rev() {
        if let Some((_, suffix)) = line.split_once(['•', '·'])
            && compact_label(line).contains("usage")
        {
            return bounded_detail(suffix);
        }
    }
    None
}

fn extract_reset_label(lines: &[&str]) -> Option<String> {
    static RESET: OnceLock<regex::Regex> = OnceLock::new();
    let reset = RESET.get_or_init(|| {
        regex::Regex::new(r"(?i)\bresets?\s+(.+)$").expect("static Cursor reset regex")
    });
    lines.iter().rev().find_map(|line| {
        reset
            .captures(line)
            .and_then(|captures| captures.get(1))
            .and_then(|value| bounded_detail(value.as_str()))
    })
}

fn extract_on_demand(lines: &[&str]) -> Option<bool> {
    for (index, line) in lines.iter().enumerate().rev() {
        if !compact_label(line).contains("ondemand") {
            continue;
        }
        for candidate in lines.iter().skip(index).take(3) {
            let compact = compact_label(candidate);
            if compact.contains("disabled") {
                return Some(false);
            }
            if compact.contains("enabled") {
                return Some(true);
            }
        }
    }
    None
}

fn bounded_detail(text: &str) -> Option<String> {
    let value = text
        .trim()
        .chars()
        .take(DETAIL_MAX_CHARS)
        .collect::<String>();
    (!value.is_empty()).then_some(value)
}

fn compact_label(text: &str) -> String {
    text.chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .map(|character| character.to_ascii_lowercase())
        .collect()
}

fn strip_terminal_control_sequences(output: &str) -> String {
    static OSC: OnceLock<regex::Regex> = OnceLock::new();
    static CSI: OnceLock<regex::Regex> = OnceLock::new();
    let osc = OSC.get_or_init(|| {
        regex::Regex::new(r"\x1b\][^\x07]*(?:\x07|\x1b\\)").expect("static OSC regex")
    });
    let csi = CSI
        .get_or_init(|| regex::Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]").expect("static CSI regex"));
    csi.replace_all(&osc.replace_all(output, ""), "")
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backpressured_input() -> pty::PtyInputEnqueueResult {
        pty::PtyInputEnqueueResult::Backpressured {
            pressure: pty::PtyInputPressure {
                attempted_bytes: 8,
                queued_bytes: 8,
                queued_messages: 1,
                max_bytes: 8,
                max_messages: 1,
                reason: pty::PtyInputRejectReason::QueueFull,
            },
        }
    }

    const REAL_PANEL: &str = "\
Usage • Pro+
Monthly plan and on-demand usage
Resets Oct 4

Included
13% used
Auto
14% used
API
3% used

On-Demand
Disabled
";

    #[test]
    fn 실측_usage_화면에서_월간_사용량을_읽는다() {
        assert_eq!(
            parse_usage(REAL_PANEL),
            Some(CursorUsage {
                included_percent_used: 13,
                auto_percent_used: Some(14),
                api_percent_used: Some(3),
                plan_name: Some("Pro+".to_owned()),
                reset_label: Some("Oct 4".to_owned()),
                on_demand_enabled: Some(false),
            })
        );
    }

    #[test]
    fn 다시_그린_패널의_마지막_값을_사용한다() {
        let output = format!(
            "{}\nContext window 87% used\n{}",
            REAL_PANEL,
            REAL_PANEL.replace("13% used", "19% used")
        );

        let usage = parse_usage(&output).expect("마지막 패널을 읽어야 한다");
        assert_eq!(usage.included_percent_used, 19);
        assert_eq!(usage.auto_percent_used, Some(14));
        assert_eq!(usage.api_percent_used, Some(3));
    }

    #[test]
    fn included가_없는_불완전_화면은_사용량으로_인정하지_않는다() {
        let incomplete = "\
Usage • Pro+
Monthly plan and on-demand usage
Auto 14% used
API 3% used
";

        assert_eq!(parse_usage(incomplete), None);
        assert_eq!(parse_usage("Context window 87% used"), None);
    }

    #[test]
    fn included_값이_없으면_다음_auto_값을_가져오지_않는다() {
        let incomplete = "\
Monthly plan and on-demand usage
Included
Auto
14% used
API
3% used
";

        assert_eq!(parse_usage(incomplete), None);
    }

    #[test]
    fn 터미널_제어문자를_제거한_뒤에도_패널을_읽는다() {
        let output = "\x1b[2J\x1b[HUsage • Pro+\r\n\x1b[32mMonthly plan and on-demand usage\x1b[0m\r\nIncluded 13% used\r\n";
        let clean = strip_terminal_control_sequences(output);

        assert_eq!(
            parse_usage(&clean).map(|usage| usage.included_percent_used),
            Some(13)
        );
    }

    #[test]
    fn 월간_제목이_없어도_included_값이_그려지면_패널_완료로_본다() {
        assert!(usage_panel_rendered("Included\n13% used\n"));
    }

    #[test]
    fn probe_command는_감지한_cursor와_path만_사용한다() {
        let command = cursor_probe_command(
            Path::new("/custom/bin/cursor-agent"),
            std::path::PathBuf::from("/tmp/deppy-cursor-probe"),
            Some(std::ffi::OsStr::new("/custom/bin:/usr/bin:/bin")),
        )
        .expect("Cursor probe command");

        assert_eq!(command.program, "/custom/bin/cursor-agent");
        assert_eq!(command.args, vec!["--trust"]);
        assert!(
            command
                .env
                .iter()
                .any(|(key, value)| { key == "PATH" && value == "/custom/bin:/usr/bin:/bin" })
        );
    }

    #[test]
    fn 필수_pty_입력은_backpressure를_성공으로_취급하지_않는다() {
        let accepted = pty::PtyInputEnqueueResult::Accepted;
        assert!(accepted.is_accepted());
        assert!(!backpressured_input().is_accepted());
    }

    #[test]
    fn 완료된_probe는_실제_측정시각과_값을_보존한다() {
        let measured_at = Instant::now() - Duration::from_secs(60);
        let usage = parse_usage(REAL_PANEL).expect("실측 패널");
        let (sender, receiver) = mpsc::sync_channel(1);
        sender.send(Some((measured_at, usage.clone()))).unwrap();
        let mut state = UsageState {
            pending: Some(receiver),
            ..UsageState::default()
        };

        receive_pending_probe(&mut state);

        assert_eq!(state.usage, Some((measured_at, usage)));
        assert!(state.pending.is_none());
    }

    #[test]
    #[ignore = "실제 Cursor CLI를 띄워 /usage를 읽는다(최대 25초)"]
    fn cursor_실측_프로브가_월간_사용량을_읽는다() {
        let excluded = crate::agent_shim::shim_path();
        let snapshot = crate::agent_launcher::detect_installed_agents(excluded.as_deref());
        let agent = snapshot
            .find(crate::agent_launcher::AgentKind::Cursor)
            .expect("현재 기기에서 Cursor CLI를 감지해야 한다");
        let usage = fetch_cursor_usage(
            agent.executable(),
            agent.launch_path().map(std::ffi::OsStr::new),
        )
        .expect("Cursor 프로브가 오류 없이 끝나야 한다")
        .expect("Cursor Included 사용량을 읽어야 한다");

        assert!(usage.included_percent_used <= 100);
        assert!(usage.auto_percent_used.is_none_or(|value| value <= 100));
        assert!(usage.api_percent_used.is_none_or(|value| value <= 100));
    }
}
