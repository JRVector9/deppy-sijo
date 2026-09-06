use std::path::Path;
use std::sync::{Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use pty::PtyBackend as _;

const REFRESH_INTERVAL: Duration = Duration::from_secs(60);
const PROBE_TIMEOUT: Duration = Duration::from_secs(25);
const STARTUP_DELAY: Duration = Duration::from_secs(2);
const SETTLE_DELAY: Duration = Duration::from_secs(2);
const ENTER_INTERVAL: Duration = Duration::from_millis(800);
const MAX_OUTPUT_BYTES: usize = 100_000;

type CompletedProbe = Option<(Instant, crate::app::ProviderUsage)>;

#[derive(Default)]
struct UsageState {
    pending: Option<mpsc::Receiver<CompletedProbe>>,
    /// 마지막으로 성공한 프로브 값과 **잰 시각**. 시각을 같이 들고 있어야 프로브가
    /// 계속 실패할 때 옛 값이 현재값 행세를 하며 굳는 것을 막는다.
    usage: Option<(Instant, crate::app::ProviderUsage)>,
    last_request: Option<Instant>,
}

pub(crate) fn current(
    ctx: &egui::Context,
    agent: Option<&crate::agent_launcher::DetectedAgent>,
) -> Option<crate::app::ProviderUsage> {
    let agent = agent?;
    static STATE: OnceLock<Mutex<UsageState>> = OnceLock::new();
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
            .name("claude-usage-probe".to_owned())
            .spawn(move || {
                let usage = fetch_claude_usage(&executable, detected_launch_path.as_deref())
                    .ok()
                    .flatten();
                let _ = sender.send(usage.map(|usage| (Instant::now(), usage)));
                repaint.request_repaint();
            })
            .is_ok();
        state.last_request = Some(last_request_after_spawn(attempted));
        if spawned {
            state.pending = Some(receiver);
        }
    }
    let (measured_at, usage) = state.usage?;
    crate::app::fresh_usage_after(usage, measured_at.elapsed())
}

fn last_request_after_spawn(attempted: Instant) -> Instant {
    attempted
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

fn fetch_claude_usage(
    executable: &Path,
    detected_launch_path: Option<&std::ffi::OsStr>,
) -> anyhow::Result<Option<crate::app::ProviderUsage>> {
    let backend = pty::PortablePtyBackend;
    let probe_dir = crate::paths::home_dir()
        .map(|home| home.join(".deppy-sijo").join("usage-probe"))
        .unwrap_or_else(std::env::temp_dir);
    std::fs::create_dir_all(&probe_dir)?;

    let command = claude_probe_command(executable, probe_dir, detected_launch_path)?;

    let mut session = backend.spawn(&command, 120, 40)?;
    let output = session
        .take_output()
        .ok_or_else(|| anyhow::anyhow!("Claude usage PTY output unavailable"))?;
    std::thread::sleep(STARTUP_DELAY);
    write_required_input(session.as_mut(), b"/usage\r")?;

    let started = Instant::now();
    let mut next_enter = Instant::now() + ENTER_INTERVAL;
    let mut settle_at = None;
    let mut trust_confirmed = false;
    let mut plan_confirmed = false;
    let mut bytes = Vec::new();
    while started.elapsed() < PROBE_TIMEOUT {
        match output.recv_timeout(Duration::from_millis(100)) {
            Ok(chunk) => {
                bytes.extend_from_slice(&chunk);
                if bytes.len() > MAX_OUTPUT_BYTES {
                    bytes.drain(..bytes.len() - MAX_OUTPUT_BYTES);
                }
                let clean = strip_terminal_control_sequences(&String::from_utf8_lossy(&bytes));
                let lower = clean.to_ascii_lowercase();
                if !trust_confirmed
                    && (lower.contains("do you trust")
                        || lower.contains("trust the files")
                        || lower.contains("safety check"))
                {
                    write_required_input(session.as_mut(), b"y\r")?;
                    trust_confirmed = true;
                }
                if !plan_confirmed
                    && (lower.contains("show plan") || lower.contains("usage limits"))
                {
                    write_required_input(session.as_mut(), b"\r")?;
                    plan_confirmed = true;
                }
                if settle_at.is_none() && usage_panel_rendered(&lower) {
                    settle_at = Some(Instant::now() + SETTLE_DELAY);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
        if settle_at.is_some_and(|deadline| Instant::now() >= deadline) {
            break;
        }
        if settle_at.is_none() && Instant::now() >= next_enter {
            write_required_input(session.as_mut(), b"\r")?;
            next_enter = Instant::now() + ENTER_INTERVAL;
        }
    }
    let _ = session.kill();

    let clean = strip_terminal_control_sequences(&String::from_utf8_lossy(&bytes));
    Ok(parse_usage(&clean))
}

fn claude_probe_command(
    executable: &Path,
    probe_dir: std::path::PathBuf,
    detected_launch_path: Option<&std::ffi::OsStr>,
) -> anyhow::Result<pty::CommandSpec> {
    let (program, args) =
        crate::provider_usage_command::probe_program_and_args(executable, cfg!(windows), "Claude")?;
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
            .map_err(|_| anyhow::anyhow!("Claude probe PATH could not be constructed"))?
    }
    .into_string()
    .map_err(|_| anyhow::anyhow!("Claude probe PATH is not UTF-8"))?;
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

fn required_input_result(result: pty::PtyInputEnqueueResult) -> anyhow::Result<()> {
    anyhow::ensure!(
        result.is_accepted(),
        "Claude usage PTY input was not accepted"
    );
    Ok(())
}

fn write_required_input(session: &mut dyn pty::PtySession, bytes: &[u8]) -> anyhow::Result<()> {
    required_input_result(session.write_input(bytes)?)
}

fn usage_panel_rendered(lower: &str) -> bool {
    let compact = compact_label(lower);
    [
        "currentweekallmodels",
        "currentweekopus",
        "currentweeksonnet",
        "weeklylimits",
        "weeklylimit",
        "weeklyusage",
        "7day",
        "currentsession",
        "failedtoloadusagedata",
    ]
    .into_iter()
    .any(|needle| compact.contains(needle))
}

fn parse_usage(output: &str) -> Option<crate::app::ProviderUsage> {
    let lines = output.split(['\r', '\n']).collect::<Vec<_>>();
    let session = extract_percent_after_label(&lines, |line| {
        compact_label(line).contains("currentsession")
    });
    let weekly = extract_percent_after_label(&lines, |line| {
        let line = compact_label(line);
        !line.contains("fable")
            && (line.contains("currentweek")
                || line.contains("weeklylimit")
                || line.contains("weeklyusage")
                || line.contains("weeklyratelimit")
                || line.contains("7day"))
    });
    // 한쪽 라벨만 그려진 패널에서도 읽어낸 쪽은 살린다.
    (session.is_some() || weekly.is_some()).then_some((session, weekly))
}

fn compact_label(text: &str) -> String {
    text.chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .map(|character| character.to_ascii_lowercase())
        .collect()
}

fn extract_percent_after_label(lines: &[&str], matches_label: impl Fn(&str) -> bool) -> Option<u8> {
    static PERCENT: OnceLock<regex::Regex> = OnceLock::new();
    let percent = PERCENT.get_or_init(|| {
        regex::Regex::new(r"(?i)(\d{1,3})(?:\.\d+)?\s*%\s*(used|consumed|left|remaining|available)")
            .expect("static Claude usage regex")
    });
    // Claude TUI는 같은 패널을 여러 번 다시 그린다. 첫 프레임의 임시 0%가 아니라
    // 가장 마지막으로 그려진 안정된 값을 사용한다.
    for (index, line) in lines.iter().enumerate().rev() {
        if !matches_label(line) {
            continue;
        }
        for candidate in lines.iter().skip(index).take(12) {
            let Some(captures) = percent.captures(candidate) else {
                continue;
            };
            let raw = captures.get(1)?.as_str().parse::<u8>().ok()?.min(100);
            let orientation = captures.get(2)?.as_str().to_ascii_lowercase();
            return Some(if orientation == "used" || orientation == "consumed" {
                raw
            } else {
                100 - raw
            });
        }
    }
    None
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

    #[test]
    fn probe_command는_런처가_감지한_claude와_path를_쓴다() {
        let command = claude_probe_command(
            std::path::Path::new("/custom/bin/claude"),
            std::path::PathBuf::from("/tmp/deppy-claude-probe"),
            Some(std::ffi::OsStr::new("/custom/bin:/usr/bin:/bin")),
        )
        .expect("Claude probe command");

        assert_eq!(command.program, "/custom/bin/claude");
        assert!(
            command
                .env
                .iter()
                .any(|(key, value)| { key == "PATH" && value.starts_with("/custom/bin:") })
        );
    }

    #[test]
    fn windows_cmd_shim은_cmd_exe로_감싸서_실행한다() {
        let (program, args) = crate::provider_usage_command::probe_program_and_args(
            std::path::Path::new(
                r"C:\Program Files (x86)\A&B\%SDK%\Preview=One\Caret^Name\claude.cmd",
            ),
            true,
            "Claude",
        )
        .expect("Windows Claude probe executable");

        assert_eq!(
            std::path::PathBuf::from(&program),
            crate::provider_usage_command::trusted_windows_command_processor_for_test()
                .expect("trusted Windows command processor"),
            "Win32가 알려 준 시스템 디렉터리의 cmd.exe여야 한다"
        );
        assert_eq!(
            args,
            vec![
                "/d".to_owned(),
                "/e:on".to_owned(),
                "/v:off".to_owned(),
                "/s".to_owned(),
                "/c".to_owned(),
                r"C:\Program^ Files^ ^(x86^)\A^&B\%%cd:~,%%SDK%%cd:~,%%\Preview^=One\Caret^^Name\claude.cmd".to_owned(),
            ]
        );
    }

    #[test]
    fn 필수_pty_입력은_backpressure를_성공으로_취급하지_않는다() {
        assert!(required_input_result(pty::PtyInputEnqueueResult::Accepted).is_ok());
        assert!(required_input_result(backpressured_input()).is_err());
    }

    #[test]
    fn worker_spawn_실패도_마지막_시도_시각을_기록한다() {
        let attempted = Instant::now();
        assert_eq!(last_request_after_spawn(attempted), attempted);
    }

    #[test]
    fn 완료된_probe의_측정시각을_ui수신시각으로_바꾸지_않는다() {
        let measured_at = Instant::now() - Duration::from_secs(60);
        let usage = (Some(10), Some(20));
        let (sender, receiver) = mpsc::sync_channel(1);
        sender.send(Some((measured_at, usage))).unwrap();
        let mut state = UsageState {
            pending: Some(receiver),
            ..UsageState::default()
        };

        receive_pending_probe(&mut state);

        assert_eq!(state.usage, Some((measured_at, usage)));
        assert!(state.pending.is_none());
    }

    #[test]
    fn usage_화면에서_세션과_주간_사용률을_읽는다() {
        let output = "Current session\n  12% used\nCurrent week (all models)\n  34% used\n";
        assert_eq!(parse_usage(output), Some((Some(12), Some(34))));
    }
}
