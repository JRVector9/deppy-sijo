//! Kimi 플랜 사용량 — 숨긴 PTY에 CLI를 띄우고 `/status`를 쳐서 화면을 읽는다.
//!
//! `claude_usage`와 **같은 방식**이다. 다른 길을 셋 재봤지만 전부 이 방식만 못했다
//! (2026-08-10 조사):
//! - Kimi 로컬 서버(`kimi web`)의 `/api/v1/oauth/usage`는 값이 정확하지만 사용자가
//!   그 서버를 띄우지 않는다. 띄우게 만들면 「사용량 보려고 서버를 켜야 하는」 비대칭이 된다.
//! - 원격 API 직접 호출은 Kimi의 OAuth 자격증명을 우리가 읽어야 하고, Kimi가 토큰을
//!   lazy로만 갱신해서(백그라운드 루프 없음) 만료 상태일 때가 잦다.
//! - transcript 누적은 **한도가 로컬에 없어** %를 만들 수 없다.
//!
//! `/status`는 CLI가 자기 자격증명으로 이미 가져와 그린 값이라 우리가 아무것도 건드리지
//! 않는다. 실측한 화면(0.34.0):
//! ```text
//! Plan usage
//!   Weekly limit  ▓░░░  6% used   resets in 6d 7h 39m
//!   5h limit      ░░░░  0% used   resets in 4h 39m
//! ```
//! 같은 시각 서버 API가 준 값(주간 6%, 5시간 0%)과 일치하는 것을 확인했다.
//!
//! 무료 계정은 이 패널 자리에 「Upgrade to a membership … see plan usage」가 뜬다 —
//! 그 경우 아무것도 못 읽고 조용히 표시하지 않는다. 「쓰는 사람에게만 보인다」가
//! 자연히 성립한다.

use std::path::Path;
use std::sync::{Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use pty::PtyBackend as _;

/// claude 프로브와 같은 주기. 이쪽도 매번 CLI 프로세스를 하나 띄우므로 짧게 잡지 않는다.
const REFRESH_INTERVAL: Duration = Duration::from_secs(60);
const PROBE_TIMEOUT: Duration = Duration::from_secs(25);
const STARTUP_DELAY: Duration = Duration::from_secs(2);
const SETTLE_DELAY: Duration = Duration::from_secs(2);
const MAX_OUTPUT_BYTES: usize = 100_000;

#[derive(Default)]
struct UsageState {
    usage: Option<(Instant, crate::app::ProviderUsage)>,
    pending: Option<mpsc::Receiver<Option<crate::app::ProviderUsage>>>,
    last_request: Option<Instant>,
}

/// 마지막으로 읽어둔 값. 없으면 백그라운드 프로브를 한 번 건다.
///
/// **Kimi가 설치돼 있지 않으면 아무것도 하지 않는다** — 안 쓰는 사용자에게 CLI를
/// 띄우지도, 상태바에 칸을 만들지도 않는다.
///
/// 처음엔 「감지된 Kimi 세션이 있을 때만」으로 막았는데, 그 판정 근거인 `agent_kinds`가
/// **활성 워크스페이스만** 담아서 다른 워크스페이스에서 Kimi를 쓰면 프로브가 영영 돌지
/// 않았다(2026-08-10 실증). 사용량은 계정 단위 값이라 워크스페이스와 무관해야 한다.
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
        let executable = agent.executable().to_path_buf();
        let detected_launch_path = agent.launch_path().map(std::ffi::OsString::from);
        let (sender, receiver) = mpsc::sync_channel(1);
        let repaint = ctx.clone();
        let attempted = Instant::now();
        let spawned = std::thread::Builder::new()
            .name("kimi-usage-probe".to_owned())
            .spawn(move || {
                let usage = fetch_kimi_usage(&executable, detected_launch_path.as_deref())
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
    crate::app::fresh_usage_after(usage, measured_at.elapsed())
}

fn last_request_after_spawn(attempted: Instant) -> Instant {
    attempted
}

fn fetch_kimi_usage(
    executable: &Path,
    detected_launch_path: Option<&std::ffi::OsStr>,
) -> anyhow::Result<Option<crate::app::ProviderUsage>> {
    let backend = pty::PortablePtyBackend;
    // claude 프로브와 같은 전용 디렉터리. 사용자의 실제 프로젝트에서 띄우지 않는다 —
    // 세션 기록이나 신뢰 설정이 그쪽에 섞이면 안 된다.
    let probe_dir = crate::paths::home_dir()
        .map(|home| home.join(".deppy-sijo").join("usage-probe"))
        .unwrap_or_else(std::env::temp_dir);
    std::fs::create_dir_all(&probe_dir)?;

    let command = kimi_probe_command(executable, probe_dir, detected_launch_path)?;

    let mut session = backend.spawn(&command, 120, 40)?;
    let output = session
        .take_output()
        .ok_or_else(|| anyhow::anyhow!("Kimi usage PTY output unavailable"))?;
    std::thread::sleep(STARTUP_DELAY);
    write_required_input(session.as_mut(), b"/status\r")?;

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
                // 폴더 신뢰 확인은 메뉴다(↑↓ 이동, Enter 선택). 기본 선택이 「Trust this
                // folder」라 Enter 한 번이면 지나간다. 우리 전용 프로브 디렉터리라
                // 신뢰해도 사용자 프로젝트에 영향이 없다. 한 번만 보낸다 — 반복하면
                // 다음 화면의 Enter까지 먹어 `/status`가 지워진다.
                if !trusted && lower.contains("trust this folder") {
                    write_required_input(session.as_mut(), b"\r")?;
                    trusted = true;
                    // 신뢰 직후 TUI가 다시 그려지므로 명령을 한 번 더 보낸다.
                    write_required_input(session.as_mut(), b"/status\r")?;
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
    let _ = session.kill();

    let clean = strip_terminal_control_sequences(&String::from_utf8_lossy(&bytes));
    Ok(parse_usage(&clean))
}

fn kimi_probe_command(
    executable: &Path,
    probe_dir: std::path::PathBuf,
    detected_launch_path: Option<&std::ffi::OsStr>,
) -> anyhow::Result<pty::CommandSpec> {
    let program = executable
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Kimi executable path is not UTF-8"))?;
    let search_path = probe_search_path(executable, detected_launch_path, "Kimi")?;
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

fn probe_search_path(
    executable: &Path,
    detected_launch_path: Option<&std::ffi::OsStr>,
    provider: &str,
) -> anyhow::Result<String> {
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
            .map_err(|_| anyhow::anyhow!("{provider} probe PATH could not be constructed"))?
    };
    search_path
        .into_string()
        .map_err(|_| anyhow::anyhow!("{provider} probe PATH is not UTF-8"))
}

fn required_input_result(result: pty::PtyInputEnqueueResult) -> anyhow::Result<()> {
    anyhow::ensure!(
        result.is_accepted(),
        "Kimi usage PTY input was not accepted"
    );
    Ok(())
}

fn write_required_input(session: &mut dyn pty::PtySession, bytes: &[u8]) -> anyhow::Result<()> {
    required_input_result(session.write_input(bytes)?)
}

/// 패널이 다 그려졌는지. 무료 계정 안내와 로드 실패도 «더 기다릴 필요 없음»이다.
fn usage_panel_rendered(lower: &str) -> bool {
    let compact = compact_label(lower);
    [
        "planusage",
        "weeklylimit",
        "nousagedatayet",
        "failedtoload",
        "seeplanusage",
    ]
    .into_iter()
    .any(|needle| compact.contains(needle))
}

/// `Weekly limit … N% used` / `5h limit … N% used`를 읽는다.
///
/// 반환은 `(5시간, 주간)` — 상태바의 다른 provider와 같은 순서다.
fn parse_usage(output: &str) -> Option<crate::app::ProviderUsage> {
    let lines = output.split(['\r', '\n']).collect::<Vec<_>>();
    let short =
        extract_percent_after_label(&lines, |line| short_window_label(&compact_label(line)));
    let weekly =
        extract_percent_after_label(&lines, |line| compact_label(line).contains("weeklylimit"));
    // 한쪽만 그려진 화면에서도 읽어낸 쪽은 살린다(claude 경로와 같은 관례).
    (short.is_some() || weekly.is_some()).then_some((short, weekly))
}

/// 짧은 창 라벨 — 실측은 `5h limit`이지만 CLI 문자열이 `{n}h limit`이라 n은 열어 둔다.
/// 일/분 창(`{n}d limit` / `{n}m limit`)은 5시간 칸에 넣지 않는다 — 다른 의미다.
fn short_window_label(compact: &str) -> bool {
    let bytes = compact.as_bytes();
    for (index, window) in bytes.windows(6).enumerate() {
        if window == b"hlimit" && index > 0 && bytes[index - 1].is_ascii_digit() {
            return true;
        }
    }
    false
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
        regex::Regex::new(r"(?i)(\d{1,3})(?:\.\d+)?\s*%\s*used").expect("static Kimi usage regex")
    });
    // TUI가 같은 패널을 여러 번 다시 그린다. 마지막으로 그려진 값을 쓴다.
    for (index, line) in lines.iter().enumerate().rev() {
        if !matches_label(line) {
            continue;
        }
        for candidate in lines.iter().skip(index).take(4) {
            let Some(captures) = percent.captures(candidate) else {
                continue;
            };
            return captures
                .get(1)?
                .as_str()
                .parse::<u8>()
                .ok()
                .map(|v| v.min(100));
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
    fn probe_command는_런처가_감지한_kimi와_path를_쓴다() {
        let command = kimi_probe_command(
            std::path::Path::new("/custom/bin/kimi"),
            std::path::PathBuf::from("/tmp/deppy-kimi-probe"),
            Some(std::ffi::OsStr::new("/custom/bin:/usr/bin:/bin")),
        )
        .expect("Kimi probe command");

        assert_eq!(command.program, "/custom/bin/kimi");
        assert!(
            command
                .env
                .iter()
                .any(|(key, value)| { key == "PATH" && value.starts_with("/custom/bin:") })
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

    /// 실제 CLI를 앱과 **같은 PTY 백엔드**로 띄워 값을 읽어본다. expect로 재현했을 때는
    /// 입력이 전혀 안 닿았는데, 그건 그쪽 PTY 사정이었을 수 있다 — 앱이 쓰는 경로로
    /// 확인해야 의미가 있다. Kimi가 없는 기기에서는 조용히 건너뛴다.
    ///
    /// `--ignored`로 둔다: CLI 프로세스를 띄우고 최대 25초가 걸려 일반 스위트에 넣을
    /// 성질이 아니다. `cargo test -- --ignored kimi_실측`으로 부른다.
    #[test]
    #[ignore = "실제 kimi CLI를 띄운다(최대 25초)"]
    fn kimi_실측_프로브가_사용량을_읽는다() {
        let excluded = crate::agent_shim::shim_path();
        let snapshot = crate::agent_launcher::detect_installed_agents(excluded.as_deref());
        let Some(agent) = snapshot.find(crate::agent_launcher::AgentKind::Kimi) else {
            return;
        };
        let usage = fetch_kimi_usage(
            agent.executable(),
            agent.launch_path().map(std::ffi::OsStr::new),
        )
        .expect("프로브가 오류 없이 끝나야 한다");
        let usage = usage.expect("Plan usage를 읽지 못했다 — 화면 형식이 바뀌었을 수 있다");
        assert!(
            usage.0.is_some() || usage.1.is_some(),
            "창을 하나도 못 읽었다: {usage:?}"
        );
    }

    #[test]
    #[ignore = "런처 감지와 실제 Kimi CLI를 최대 25초 실행한다"]
    fn kimi_실측_프로브는_런처_감지_경로로_끝난다() {
        let excluded = crate::agent_shim::shim_path();
        let snapshot = crate::agent_launcher::detect_installed_agents(excluded.as_deref());
        let agent = snapshot
            .find(crate::agent_launcher::AgentKind::Kimi)
            .expect("현재 계정에서 설치된 Kimi를 감지해야 한다");
        let usage = fetch_kimi_usage(
            agent.executable(),
            agent.launch_path().map(std::ffi::OsStr::new),
        )
        .expect("launcher-detected bounded Kimi probe");
        assert!(usage.is_some(), "현재 계정에서 Kimi usage 숫자를 기대했다");
    }

    /// 2026-08-10 실측 화면 그대로. 라벨·퍼센트 표기가 바뀌면 여기서 깨져야 한다 —
    /// 조용히 None이 되면 상태바에서 Kimi만 사라지고 이유를 알 수 없다.
    const REAL_PANEL: &str = "\
Status
 >_ Kimi Code (v0.34.0)
   Model        K3 (thinking high)
   Directory    /Users/jr/Desktop/projects/simpleHWP
 Context window
   ▓░░░░░░░   4%  (30.7k / 1M)
 Plan usage
   Weekly limit  ▓░░░░░░   6% used   resets in 6d 7h 39m
   5h limit      ░░░░░░░   0% used   resets in 4h 39m
";

    #[test]
    fn 실측_status_화면에서_두_창을_읽는다() {
        assert_eq!(parse_usage(REAL_PANEL), Some((Some(0), Some(6))));
    }

    /// 컨텍스트 창(4%)은 사용량이 아니다. 라벨을 안 보고 첫 퍼센트만 집으면 그걸 집는다.
    #[test]
    fn 컨텍스트_퍼센트를_사용량으로_읽지_않는다() {
        let (short, weekly) = parse_usage(REAL_PANEL).expect("읽혀야 한다");
        assert_ne!(
            short,
            Some(4),
            "Context window 4%를 5시간 사용량으로 읽었다"
        );
        assert_ne!(
            weekly,
            Some(4),
            "Context window 4%를 주간 사용량으로 읽었다"
        );
    }

    /// 실측 화면에서는 라벨과 값이 같은 줄이라 **라벨 기준 탐색만으로도** 컨텍스트를
    /// 피한다. 하지만 줄바꿈이 달라지면 그 보호가 사라진다 — 그때 `% used` 접미사가
    /// 유일한 방어선이다. 그 조건을 따로 고정한다.
    #[test]
    fn 라벨_뒤에_used없는_퍼센트가_와도_건너뛴다() {
        let wrapped = "\
 Plan usage
   Weekly limit
     4%  (30.7k / 1M)
     6% used   resets in 6d
";
        assert_eq!(
            parse_usage(wrapped).and_then(|usage| usage.1),
            Some(6),
            "'% used'가 아닌 퍼센트를 사용량으로 읽으면 안 된다"
        );
    }

    /// `{n}h limit`의 n은 계정마다 다를 수 있다. 반대로 일/분 창은 5시간 칸이 아니다.
    #[test]
    fn 시간_창만_짧은_창으로_인정한다() {
        assert!(short_window_label(&compact_label("5h limit")));
        assert!(short_window_label(&compact_label("12h limit")));
        assert!(!short_window_label(&compact_label("7d limit")));
        assert!(!short_window_label(&compact_label("30m limit")));
        assert!(!short_window_label(&compact_label("weekly limit")));
    }

    /// 무료 계정은 패널 대신 안내 문구가 뜬다 — 값을 지어내지 말고 아무것도 안 준다.
    #[test]
    fn 사용량이_없는_화면에서는_아무것도_돌려주지_않는다() {
        let free = "Plan usage\n  Upgrade to a membership to use Kimi models and see plan usage\n";
        assert_eq!(parse_usage(free), None);
        assert_eq!(parse_usage(""), None);
        // 다 그려졌다는 판정에는 걸려야 한다 — 아니면 타임아웃까지 기다린다.
        assert!(usage_panel_rendered(&free.to_ascii_lowercase()));
    }
}
