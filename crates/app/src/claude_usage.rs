use std::sync::{Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use pty::PtyBackend as _;

const REFRESH_INTERVAL: Duration = Duration::from_secs(60);
const PROBE_TIMEOUT: Duration = Duration::from_secs(25);
const STARTUP_DELAY: Duration = Duration::from_secs(2);
const SETTLE_DELAY: Duration = Duration::from_secs(2);
const ENTER_INTERVAL: Duration = Duration::from_millis(800);
const MAX_OUTPUT_BYTES: usize = 100_000;

#[derive(Default)]
struct UsageState {
    pending: Option<mpsc::Receiver<Option<crate::app::ProviderUsage>>>,
    /// 마지막으로 성공한 프로브 값과 **잰 시각**. 시각을 같이 들고 있어야 프로브가
    /// 계속 실패할 때 옛 값이 현재값 행세를 하며 굳는 것을 막는다.
    usage: Option<(Instant, crate::app::ProviderUsage)>,
    last_request: Option<Instant>,
}

pub fn current(ctx: &egui::Context) -> Option<crate::app::ProviderUsage> {
    static STATE: OnceLock<Mutex<UsageState>> = OnceLock::new();
    let state = STATE.get_or_init(|| Mutex::new(UsageState::default()));
    let Ok(mut state) = state.lock() else {
        return None;
    };

    if let Some(receiver) = state.pending.as_ref() {
        match receiver.try_recv() {
            Ok(usage) => {
                if let Some(usage) = usage {
                    state.usage = Some((Instant::now(), usage));
                }
                state.pending = None;
            }
            Err(mpsc::TryRecvError::Disconnected) => state.pending = None,
            Err(mpsc::TryRecvError::Empty) => {}
        }
    }

    let refresh_due = state
        .last_request
        .is_none_or(|requested| requested.elapsed() >= REFRESH_INTERVAL);
    if state.pending.is_none() && refresh_due {
        let (sender, receiver) = mpsc::sync_channel(1);
        let repaint = ctx.clone();
        if std::thread::Builder::new()
            .name("claude-usage-probe".to_owned())
            .spawn(move || {
                let usage = fetch_claude_usage().ok().flatten();
                let _ = sender.send(usage);
                repaint.request_repaint();
            })
            .is_ok()
        {
            state.pending = Some(receiver);
            state.last_request = Some(Instant::now());
        }
    }
    let (measured_at, usage) = state.usage?;
    crate::app::fresh_usage_after(usage, measured_at.elapsed())
}

fn fetch_claude_usage() -> anyhow::Result<Option<crate::app::ProviderUsage>> {
    let backend = pty::PortablePtyBackend;
    let probe_dir = crate::paths::home_dir()
        .map(|home| home.join(".deppy-sijo").join("usage-probe"))
        .unwrap_or_else(std::env::temp_dir);
    std::fs::create_dir_all(&probe_dir)?;

    #[cfg(windows)]
    let command = pty::CommandSpec {
        program: "cmd.exe".to_owned(),
        args: vec!["/d".to_owned(), "/c".to_owned(), "claude".to_owned()],
        env: vec![("TERM".to_owned(), "xterm-256color".to_owned())],
        cwd: Some(probe_dir),
    };
    #[cfg(not(windows))]
    let command = pty::CommandSpec {
        program: resolve_claude_command(),
        args: Vec::new(),
        env: vec![("TERM".to_owned(), "xterm-256color".to_owned())],
        cwd: Some(probe_dir),
    };

    let mut session = backend.spawn(&command, 120, 40)?;
    let output = session
        .take_output()
        .ok_or_else(|| anyhow::anyhow!("Claude usage PTY output unavailable"))?;
    std::thread::sleep(STARTUP_DELAY);
    session.write_input(b"/usage\r")?;

    let started = Instant::now();
    let mut next_enter = Instant::now() + ENTER_INTERVAL;
    let mut settle_at = None;
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
                if lower.contains("do you trust")
                    || lower.contains("trust the files")
                    || lower.contains("safety check")
                {
                    let _ = session.write_input(b"y\r");
                }
                if lower.contains("show plan") || lower.contains("usage limits") {
                    let _ = session.write_input(b"\r");
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
            let _ = session.write_input(b"\r");
            next_enter = Instant::now() + ENTER_INTERVAL;
        }
    }
    let _ = session.kill();

    let clean = strip_terminal_control_sequences(&String::from_utf8_lossy(&bytes));
    Ok(parse_usage(&clean))
}

#[cfg(not(windows))]
fn resolve_claude_command() -> String {
    let mut candidates = Vec::new();
    if let Some(home) = crate::paths::home_dir() {
        candidates.push(home.join(".local/bin/claude"));
    }
    candidates.extend([
        std::path::PathBuf::from("/opt/homebrew/bin/claude"),
        std::path::PathBuf::from("/usr/local/bin/claude"),
    ]);
    candidates
        .into_iter()
        .find(|path| path.is_file())
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| "claude".to_owned())
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
