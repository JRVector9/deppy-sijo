use std::path::Path;
use std::sync::{Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use pty::PtyBackend as _;

const REFRESH_INTERVAL: Duration = Duration::from_secs(60);
const STALE_AFTER: Duration = Duration::from_secs(10 * 60);
const PROBE_TIMEOUT: Duration = Duration::from_secs(25);
const STARTUP_DELAY: Duration = Duration::from_secs(2);
const SETTLE_DELAY: Duration = Duration::from_secs(2);
const MAX_OUTPUT_BYTES: usize = 100_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GrokCurrency {
    Usd,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GrokCredits {
    pub(crate) currency: GrokCurrency,
    pub(crate) minor_units: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct GrokUsage {
    pub(crate) weekly_remaining_percent: Option<u8>,
    pub(crate) monthly_remaining_percent: Option<u8>,
    pub(crate) credits_left: Option<GrokCredits>,
}

#[derive(Default)]
struct UsageState {
    usage: Option<(Instant, GrokUsage)>,
    pending: Option<mpsc::Receiver<Option<GrokUsage>>>,
    last_request: Option<Instant>,
}

fn should_start_probe(
    executable_present: bool,
    pending: bool,
    since_last_request: Option<Duration>,
) -> bool {
    executable_present
        && !pending
        && since_last_request.is_none_or(|elapsed| elapsed >= REFRESH_INTERVAL)
}

fn fresh_usage_after(usage: GrokUsage, elapsed: Duration) -> Option<GrokUsage> {
    (elapsed <= STALE_AFTER).then_some(usage)
}

fn last_request_after_spawn(
    previous: Option<Instant>,
    attempted: Instant,
    spawned: bool,
) -> Option<Instant> {
    if spawned { Some(attempted) } else { previous }
}

pub(crate) fn current(ctx: &egui::Context, executable: Option<&Path>) -> Option<GrokUsage> {
    static STATE: OnceLock<Mutex<UsageState>> = OnceLock::new();
    let executable = executable?.to_path_buf();
    let state = STATE.get_or_init(|| Mutex::new(UsageState::default()));
    let Ok(mut state) = state.lock() else {
        return None;
    };
    if let Some(receiver) = state.pending.as_ref() {
        match receiver.try_recv() {
            Ok(Some(usage)) => {
                state.usage = Some((Instant::now(), usage));
                state.pending = None;
            }
            Ok(None) | Err(mpsc::TryRecvError::Disconnected) => state.pending = None,
            Err(mpsc::TryRecvError::Empty) => {}
        }
    }
    let since_last_request = state.last_request.map(|requested| requested.elapsed());
    if should_start_probe(true, state.pending.is_some(), since_last_request) {
        let (sender, receiver) = mpsc::sync_channel(1);
        let repaint = ctx.clone();
        let attempted = Instant::now();
        let spawned = std::thread::Builder::new()
            .name("grok-usage-probe".to_owned())
            .spawn(move || {
                let usage = fetch_grok_usage(&executable).ok().flatten();
                let _ = sender.send(usage);
                repaint.request_repaint();
            })
            .is_ok();
        state.last_request = last_request_after_spawn(state.last_request, attempted, spawned);
        if spawned {
            state.pending = Some(receiver);
        }
    }
    let (measured_at, usage) = state.usage?;
    fresh_usage_after(usage, measured_at.elapsed())
}

fn parse_usage(output: &str) -> Option<GrokUsage> {
    let clean = strip_terminal_control_sequences(output);
    let lines = clean.split(['\r', '\n']).collect::<Vec<_>>();
    let usage = GrokUsage {
        weekly_remaining_percent: extract_window_remaining(&lines, "weeklylimit"),
        monthly_remaining_percent: extract_window_remaining(&lines, "monthlylimit"),
        credits_left: extract_credits_left(&lines),
    };
    (usage.weekly_remaining_percent.is_some()
        || usage.monthly_remaining_percent.is_some()
        || usage.credits_left.is_some())
    .then_some(usage)
}

fn extract_window_remaining(lines: &[&str], label: &str) -> Option<u8> {
    static PERCENT: OnceLock<regex::Regex> = OnceLock::new();
    static LIMIT: OnceLock<regex::Regex> = OnceLock::new();
    let percent = PERCENT.get_or_init(|| {
        regex::Regex::new(r"(?i)(\d+)(?:\.\d+)?\s*%\s*(used|left|remaining)")
            .expect("static Grok percent regex")
    });
    let limit = LIMIT.get_or_init(|| {
        regex::Regex::new(
            r"(?i)\$\s*([0-9][0-9,]*(?:\.\d{1,2})?)\s*(?:used\s*)?of\s*\$\s*([0-9][0-9,]*(?:\.\d{1,2})?)",
        )
        .expect("static Grok limit regex")
    });
    for (index, line) in lines.iter().enumerate().rev() {
        if !compact_label(line).contains(label) {
            continue;
        }
        for candidate in lines.iter().skip(index).take(4) {
            let compact = compact_label(candidate);
            if candidate != line
                && (compact.contains("weeklylimit") || compact.contains("monthlylimit"))
            {
                break;
            }
            if let Some(captures) = percent.captures(candidate) {
                let value = captures.get(1)?.as_str().parse::<u64>().ok()?.min(100) as u8;
                return match captures.get(2)?.as_str().to_ascii_lowercase().as_str() {
                    "used" => Some(100 - value),
                    "left" | "remaining" => Some(value),
                    _ => None,
                };
            }
            if let Some(captures) = limit.captures(candidate) {
                let used = parse_usd_minor(captures.get(1)?.as_str())?;
                let total = parse_usd_minor(captures.get(2)?.as_str())?;
                return remaining_percent(used, total);
            }
        }
    }
    None
}

fn extract_credits_left(lines: &[&str]) -> Option<GrokCredits> {
    static MONEY: OnceLock<regex::Regex> = OnceLock::new();
    let money = MONEY.get_or_init(|| {
        regex::Regex::new(r"\$\s*([0-9][0-9,]*(?:\.\d{1,2})?)").expect("static Grok money regex")
    });
    for (index, line) in lines.iter().enumerate().rev() {
        if !compact_label(line).contains("creditsleft") {
            continue;
        }
        for candidate in lines.iter().skip(index).take(4) {
            let compact = compact_label(candidate);
            if candidate != line
                && (compact.contains("weeklylimit")
                    || compact.contains("monthlylimit")
                    || compact.contains("creditsleft"))
            {
                break;
            }
            if let Some(captures) = money.captures(candidate) {
                return Some(GrokCredits {
                    currency: GrokCurrency::Usd,
                    minor_units: parse_usd_minor(captures.get(1)?.as_str())?,
                });
            }
        }
    }
    None
}

fn usage_panel_rendered(lower: &str) -> bool {
    let compact = compact_label(lower);
    [
        "weeklylimit",
        "monthlylimit",
        "creditsleft",
        "notauthenticated",
        "managebilling",
        "failedtoload",
    ]
    .into_iter()
    .any(|needle| compact.contains(needle))
}

fn fetch_grok_usage(executable: &Path) -> anyhow::Result<Option<GrokUsage>> {
    let program = executable
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Grok executable path is not UTF-8"))?;
    let probe_dir = crate::paths::home_dir()
        .map(|home| home.join(".deppy-sijo").join("usage-probe"))
        .unwrap_or_else(std::env::temp_dir);
    std::fs::create_dir_all(&probe_dir)?;
    let command = pty::CommandSpec {
        program: program.to_owned(),
        args: Vec::new(),
        env: vec![("TERM".to_owned(), "xterm-256color".to_owned())],
        cwd: Some(probe_dir),
    };
    let backend = pty::PortablePtyBackend;
    let mut session = backend.spawn(&command, 120, 40)?;
    let result = run_grok_usage_probe(&mut *session);
    let kill_result = session.kill();
    match (result, kill_result) {
        (Ok(usage), Ok(())) => Ok(usage),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
    }
}

fn run_grok_usage_probe(session: &mut dyn pty::PtySession) -> anyhow::Result<Option<GrokUsage>> {
    let output = session
        .take_output()
        .ok_or_else(|| anyhow::anyhow!("Grok usage PTY output unavailable"))?;
    std::thread::sleep(STARTUP_DELAY);
    write_required_input(session, b"/usage\r")?;

    let started = Instant::now();
    let mut settle_at = None;
    let mut trusted = false;
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
                if !trusted && lower.contains("trust this folder") {
                    write_required_input(session, b"\r")?;
                    trusted = true;
                    write_required_input(session, b"/usage\r")?;
                }
                if settle_at.is_none() && usage_panel_rendered(&lower) {
                    settle_at = Some(Instant::now() + SETTLE_DELAY);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if settle_at.is_some_and(|deadline| Instant::now() >= deadline) {
            break;
        }
    }

    let clean = strip_terminal_control_sequences(&String::from_utf8_lossy(&bytes));
    Ok(parse_usage(&clean))
}

fn write_required_input(session: &mut dyn pty::PtySession, bytes: &[u8]) -> anyhow::Result<()> {
    let result = session.write_input(bytes)?;
    if !result.is_accepted() {
        anyhow::bail!("Grok usage PTY input was not accepted");
    }
    Ok(())
}

fn parse_usd_minor(text: &str) -> Option<u64> {
    let normalized = text.replace(',', "");
    let (whole, fraction) = normalized.split_once('.').unwrap_or((&normalized, ""));
    let whole = whole.parse::<u64>().ok()?.checked_mul(100)?;
    let fraction = match fraction.len() {
        0 => 0,
        1 => fraction.parse::<u64>().ok()?.checked_mul(10)?,
        2 => fraction.parse::<u64>().ok()?,
        _ => return None,
    };
    whole.checked_add(fraction)
}

fn remaining_percent(used: u64, total: u64) -> Option<u8> {
    if total == 0 || used > total {
        return None;
    }
    let remaining = total.checked_sub(used)?;
    let scaled = remaining.checked_mul(100)?;
    let rounded = scaled.checked_add(total / 2)?.checked_div(total)?;
    u8::try_from(rounded.min(100)).ok()
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
    use std::time::Duration;

    const FULL_PANEL: &str = "\
Usage
  Context window  41% (205k / 500k)
  WEEKLY
    Weekly limit  30% used  Next reset: 4d 2h
  MONTHLY
    Monthly limit  $15.00 used of $100.00 limit
  Credits left: $12.34
";

    #[test]
    fn full_panel_returns_remaining_windows_and_exact_credits() {
        assert_eq!(
            parse_usage(FULL_PANEL),
            Some(GrokUsage {
                weekly_remaining_percent: Some(70),
                monthly_remaining_percent: Some(85),
                credits_left: Some(GrokCredits {
                    currency: GrokCurrency::Usd,
                    minor_units: 1_234,
                }),
            })
        );
    }

    #[test]
    fn partial_and_redrawn_panels_keep_only_labeled_latest_values() {
        let panel = "\
Weekly limit 90% used
Context window 4% used
Weekly limit
20% used
Monthly limit
15% left
Credits left: $1,234.50
";
        assert_eq!(
            parse_usage(panel),
            Some(GrokUsage {
                weekly_remaining_percent: Some(80),
                monthly_remaining_percent: Some(15),
                credits_left: Some(GrokCredits {
                    currency: GrokCurrency::Usd,
                    minor_units: 123_450,
                }),
            })
        );
    }

    #[test]
    fn invalid_or_accountless_panels_do_not_invent_usage() {
        assert_eq!(parse_usage("You are not authenticated"), None);
        assert_eq!(parse_usage("Manage billing to view usage"), None);
        assert_eq!(parse_usage("Context window 41% used"), None);
        assert_eq!(parse_usage("Weekly limit $1 used of $0 limit"), None);
        assert_eq!(parse_usage("Credits left: $18446744073709551616.00"), None);
    }

    #[test]
    fn credits_without_money_do_not_capture_later_section_money() {
        assert_eq!(
            parse_usage("Credits left:\nMonthly limit $15.00 used of $100.00 limit\n",),
            Some(GrokUsage {
                weekly_remaining_percent: None,
                monthly_remaining_percent: Some(85),
                credits_left: None,
            })
        );
    }

    #[test]
    fn ansi_is_removed_and_percentages_are_clamped_without_context_false_positives() {
        assert_eq!(
            parse_usage("\u{1b}[31mWeekly limit 999% used\u{1b}[0m"),
            Some(GrokUsage {
                weekly_remaining_percent: Some(0),
                monthly_remaining_percent: None,
                credits_left: None,
            })
        );
    }

    #[test]
    fn oversized_percentages_capture_the_full_integer_before_clamping() {
        assert_eq!(
            parse_usage("Weekly limit 1000% used"),
            Some(GrokUsage {
                weekly_remaining_percent: Some(0),
                monthly_remaining_percent: None,
                credits_left: None,
            })
        );
        assert_eq!(
            parse_usage("Weekly limit 184467440737095516160% used"),
            None
        );
    }

    #[test]
    fn probe_admission_requires_an_executable_due_refresh_and_no_pending_job() {
        assert!(!should_start_probe(false, false, None));
        assert!(should_start_probe(true, false, None));
        assert!(!should_start_probe(true, true, None));
        assert!(!should_start_probe(
            true,
            false,
            Some(Duration::from_secs(59))
        ));
        assert!(should_start_probe(
            true,
            false,
            Some(Duration::from_secs(60))
        ));
    }

    #[test]
    fn successful_usage_survives_failures_for_ten_minutes_only() {
        let usage = GrokUsage {
            weekly_remaining_percent: Some(70),
            monthly_remaining_percent: Some(85),
            credits_left: None,
        };
        assert_eq!(
            fresh_usage_after(usage, Duration::from_secs(599)),
            Some(usage)
        );
        assert_eq!(
            fresh_usage_after(usage, Duration::from_secs(600)),
            Some(usage)
        );
        assert_eq!(fresh_usage_after(usage, Duration::from_secs(601)), None);
    }

    #[test]
    fn failed_probe_spawn_does_not_advance_last_request() {
        let previous = Instant::now() - Duration::from_secs(120);
        let attempted = Instant::now();
        assert_eq!(last_request_after_spawn(None, attempted, false), None);
        assert_eq!(
            last_request_after_spawn(Some(previous), attempted, false),
            Some(previous)
        );
        assert_eq!(
            last_request_after_spawn(Some(previous), attempted, true),
            Some(attempted)
        );
    }

    #[test]
    #[ignore = "실제 Grok CLI를 최대 25초 띄운다"]
    fn grok_실측_프로브는_민감한_원문_없이_끝난다() {
        let Some(path) = std::env::var_os("DEPPY_GROK_EXECUTABLE").map(std::path::PathBuf::from)
        else {
            return;
        };
        let usage = fetch_grok_usage(&path).expect("bounded Grok probe");
        assert!(usage.is_none_or(|value| {
            value.weekly_remaining_percent.is_some()
                || value.monthly_remaining_percent.is_some()
                || value.credits_left.is_some()
        }));
    }
}
