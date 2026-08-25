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

fn last_request_after_spawn(attempted: Instant) -> Instant {
    attempted
}

pub(crate) fn current(
    ctx: &egui::Context,
    agent: Option<&crate::agent_launcher::DetectedAgent>,
) -> Option<GrokUsage> {
    static STATE: OnceLock<Mutex<UsageState>> = OnceLock::new();
    let agent = agent?;
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
        // 감지 스냅샷의 PATH는 최대 32KiB다. 상태바 렌더 때마다 복사하지 않고 실제로
        // 60초 admission을 통과해 probe를 띄울 때만 worker 소유 값으로 만든다.
        let executable = agent.executable().to_path_buf();
        let detected_launch_path = agent.launch_path().map(std::ffi::OsString::from);
        let (sender, receiver) = mpsc::sync_channel(1);
        let repaint = ctx.clone();
        let attempted = Instant::now();
        let spawned = std::thread::Builder::new()
            .name("grok-usage-probe".to_owned())
            .spawn(move || {
                let usage = fetch_grok_usage(&executable, detected_launch_path.as_deref())
                    .ok()
                    .flatten();
                let _ = sender.send(usage);
                repaint.request_repaint();
            })
            .is_ok();
        state.last_request = Some(last_request_after_spawn(attempted));
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
        for (offset, candidate) in lines.iter().skip(index).take(4).enumerate() {
            let compact = compact_label(candidate);
            if offset > 0 && usage_section_boundary(&compact) {
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
        if !credits_label(&compact_label(line)) {
            continue;
        }
        for (offset, candidate) in lines.iter().skip(index).take(4).enumerate() {
            let compact = compact_label(candidate);
            if offset > 0 && usage_section_boundary(&compact) {
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

fn credits_label(compact: &str) -> bool {
    ["creditsleft", "credits"].into_iter().any(|prefix| {
        compact
            .strip_prefix(prefix)
            .is_some_and(|suffix| suffix.bytes().all(|byte| byte.is_ascii_digit()))
    })
}

fn usage_section_boundary(compact: &str) -> bool {
    compact.contains("weeklylimit")
        || compact.contains("monthlylimit")
        || compact.starts_with("credits")
        || compact.contains("autotopup")
        || compact.contains("payasyougo")
        || compact.contains("context")
        || compact.contains("token")
        || compact.contains("compression")
        || compact.contains("compaction")
}

fn usage_panel_rendered(lower: &str) -> bool {
    if lower
        .lines()
        .map(compact_label)
        .any(|line| credits_label(&line))
    {
        return true;
    }
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

fn fetch_grok_usage(
    executable: &Path,
    detected_launch_path: Option<&std::ffi::OsStr>,
) -> anyhow::Result<Option<GrokUsage>> {
    let probe_dir = crate::paths::home_dir()
        .map(|home| home.join(".deppy-sijo").join("usage-probe"))
        .unwrap_or_else(std::env::temp_dir);
    std::fs::create_dir_all(&probe_dir)?;
    let inherited_path = std::env::var_os("PATH");
    let command = grok_probe_command(
        executable,
        probe_dir,
        detected_launch_path,
        inherited_path.as_deref(),
    )?;
    let backend = pty::PortablePtyBackend;
    let mut session = backend.spawn(&command, 120, 40)?;
    // 전체 child 수명 예산은 startup 대기를 포함한다. spawn 직후 deadline을 고정해야
    // STARTUP_DELAY 2초 + PROBE_TIMEOUT 25초로 늘어나지 않는다.
    let deadline = probe_deadline(Instant::now());
    let result = run_grok_usage_probe(&mut *session, deadline);
    let kill_result = session.kill();
    match (result, kill_result) {
        (Ok(usage), Ok(())) => Ok(usage),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
    }
}

fn grok_probe_command(
    executable: &Path,
    probe_dir: std::path::PathBuf,
    detected_launch_path: Option<&std::ffi::OsStr>,
    inherited_path: Option<&std::ffi::OsStr>,
) -> anyhow::Result<pty::CommandSpec> {
    let program = executable
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Grok executable path is not UTF-8"))?;
    let search_path = if let Some(path) = detected_launch_path {
        // 감지 PATH는 실행 파일 디렉터리와 NVM 등 런타임 디렉터리를 이미 포함하며
        // 생성 시 32KiB로 제한된다. 그대로 전달해 중복·상한 초과를 만들지 않는다.
        path.to_os_string()
    } else {
        let mut search_paths = executable
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .into_iter()
            .map(Path::to_path_buf)
            .collect::<Vec<_>>();
        if let Some(path) = inherited_path {
            search_paths.extend(std::env::split_paths(path));
        }
        std::env::join_paths(search_paths)
            .map_err(|_| anyhow::anyhow!("Grok probe PATH could not be constructed"))?
    }
    .into_string()
    .map_err(|_| anyhow::anyhow!("Grok probe PATH is not UTF-8"))?;
    Ok(pty::CommandSpec {
        program: program.to_owned(),
        args: Vec::new(),
        env: vec![
            ("TERM".to_owned(), "xterm-256color".to_owned()),
            ("PATH".to_owned(), search_path),
        ],
        cwd: Some(probe_dir),
    })
}

fn probe_deadline(spawned_at: Instant) -> Instant {
    spawned_at + PROBE_TIMEOUT
}

fn run_grok_usage_probe(
    session: &mut dyn pty::PtySession,
    deadline: Instant,
) -> anyhow::Result<Option<GrokUsage>> {
    let output = session
        .take_output()
        .ok_or_else(|| anyhow::anyhow!("Grok usage PTY output unavailable"))?;
    std::thread::sleep(STARTUP_DELAY.min(deadline.saturating_duration_since(Instant::now())));
    if Instant::now() >= deadline {
        return Ok(None);
    }
    write_required_input(session, b"/usage\r")?;

    let mut settle_at = None;
    let mut trusted = false;
    let mut bytes = Vec::new();
    while Instant::now() < deadline {
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
    fn installed_credits_label_is_parsed_without_capturing_auto_topup_money() {
        assert_eq!(
            parse_usage("Credits: $12.34\n"),
            Some(GrokUsage {
                weekly_remaining_percent: None,
                monthly_remaining_percent: None,
                credits_left: Some(GrokCredits {
                    currency: GrokCurrency::Usd,
                    minor_units: 1_234,
                }),
            })
        );
        assert_eq!(
            parse_usage(
                "Credits:\nAuto topup: $50.00\nMonthly limit $15.00 used of $100.00 limit\n",
            ),
            Some(GrokUsage {
                weekly_remaining_percent: None,
                monthly_remaining_percent: Some(85),
                credits_left: None,
            })
        );
        assert_eq!(parse_usage("Credits used: $12.34\n"), None);
        assert_eq!(parse_usage("Weekly limit\nCredits used: 40% used\n"), None);
    }

    #[test]
    fn credit_only_panel_is_detected_without_waiting_for_the_probe_timeout() {
        assert!(usage_panel_rendered("Credits: $12.34\n"));
        assert!(!usage_panel_rendered("Credits used: $12.34\n"));
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
    fn a_context_section_after_a_limit_label_is_not_usage() {
        assert_eq!(parse_usage("Weekly limit\nContext window 41% used\n"), None);
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
    fn failed_probe_spawn_records_attempt_to_throttle_retries() {
        let attempted = Instant::now();
        assert_eq!(last_request_after_spawn(attempted), attempted);
    }

    #[test]
    fn probe_deadline_includes_the_startup_delay_in_the_25_second_budget() {
        let spawned_at = Instant::now();
        assert_eq!(
            probe_deadline(spawned_at).duration_since(spawned_at),
            PROBE_TIMEOUT
        );
    }

    #[test]
    fn inherited_path_fallback_prepends_the_detected_executable_parent() {
        let executable = Path::new("/Users/test/.nvm/versions/node/v24.0.0/bin/grok");
        let inherited =
            std::env::join_paths([Path::new("/usr/bin"), Path::new("/bin")]).expect("test PATH");
        let command = grok_probe_command(
            executable,
            std::path::PathBuf::from("/tmp/deppy-grok-probe"),
            None,
            Some(inherited.as_os_str()),
        )
        .expect("Grok probe command");
        let path = command
            .env
            .iter()
            .find_map(|(key, value)| (key == "PATH").then_some(value))
            .expect("probe PATH");
        let entries = std::env::split_paths(std::ffi::OsStr::new(path)).collect::<Vec<_>>();

        assert_eq!(
            entries.first().map(std::path::PathBuf::as_path),
            executable.parent()
        );
        assert_eq!(
            entries.get(1).map(std::path::PathBuf::as_path),
            Some(Path::new("/usr/bin"))
        );
        assert_eq!(
            entries.get(2).map(std::path::PathBuf::as_path),
            Some(Path::new("/bin"))
        );
    }

    #[test]
    fn detected_launch_path_keeps_node_available_for_a_shim_executable() {
        let executable = Path::new("/Users/test/Library/pnpm/grok");
        let detected = std::env::join_paths([
            Path::new("/Users/test/Library/pnpm"),
            Path::new("/Users/test/.nvm/versions/node/v24.0.0/bin"),
            Path::new("/usr/bin"),
        ])
        .expect("detected launch PATH");
        let inherited =
            std::env::join_paths([Path::new("/usr/bin"), Path::new("/bin")]).expect("Finder PATH");

        let command = grok_probe_command(
            executable,
            std::path::PathBuf::from("/tmp/deppy-grok-probe"),
            Some(detected.as_os_str()),
            Some(inherited.as_os_str()),
        )
        .expect("Grok probe command");
        let path = command
            .env
            .iter()
            .find_map(|(key, value)| (key == "PATH").then_some(value))
            .expect("probe PATH");
        let entries = std::env::split_paths(std::ffi::OsStr::new(path)).collect::<Vec<_>>();

        assert_eq!(
            entries.first().map(std::path::PathBuf::as_path),
            Some(Path::new("/Users/test/Library/pnpm"))
        );
        assert_eq!(
            entries.get(1).map(std::path::PathBuf::as_path),
            Some(Path::new("/Users/test/.nvm/versions/node/v24.0.0/bin"))
        );
        assert_eq!(
            entries.get(2).map(std::path::PathBuf::as_path),
            Some(Path::new("/usr/bin"))
        );
    }

    #[test]
    #[ignore = "실제 Grok CLI를 최대 25초 띄운다"]
    fn grok_실측_프로브는_민감한_원문_없이_끝난다() {
        let Some(path) = std::env::var_os("DEPPY_GROK_EXECUTABLE").map(std::path::PathBuf::from)
        else {
            return;
        };
        let usage = fetch_grok_usage(&path, None).expect("bounded Grok probe");
        assert!(usage.is_none_or(|value| {
            value.weekly_remaining_percent.is_some()
                || value.monthly_remaining_percent.is_some()
                || value.credits_left.is_some()
        }));
    }
}
