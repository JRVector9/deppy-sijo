//! BrowserLauncher (설계 §9) — external browser로 authorize URL을 연다 (§1.5).
//! 앱 내 webview 금지 방향이므로 OS 기본 브라우저 커맨드만 쓴다.

use anyhow::Context;

pub fn open_in_browser(url: &str) -> anyhow::Result<()> {
    let mut command = browser_command(url);
    command
        .spawn()
        .with_context(|| format!("브라우저 열기 실패: {url}"))?;
    Ok(())
}

/// Opens a URL with the platform browser launcher and synchronously reaps that launcher process.
///
/// This Connector-host entry point deliberately does not include the URL in either error path.
/// The caller receives success only after the launcher exits successfully; the browser process
/// itself may remain independently owned by the operating system.
pub fn open_in_browser_reaped(url: &str) -> anyhow::Result<()> {
    wait_for_browser_launcher(browser_command(url))
}

fn browser_command(url: &str) -> std::process::Command {
    #[cfg(target_os = "macos")]
    {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    }
    #[cfg(target_os = "windows")]
    {
        // cmd /C start는 URL의 &를 셸 구문으로 재해석해 잘라먹는다 —
        // rundll32는 인자를 그대로 URL 핸들러에 넘기므로 안전
        let mut c = std::process::Command::new("rundll32");
        c.arg("url.dll,FileProtocolHandler").arg(url);
        c
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    }
}

fn wait_for_browser_launcher(mut command: std::process::Command) -> anyhow::Result<()> {
    let status = command
        .status()
        .context("browser launcher could not be started or reaped")?;
    anyhow::ensure!(status.success(), "browser launcher exited unsuccessfully");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SENSITIVE_URL: &str = "https://authorization.invalid/callback?code=must-not-appear";

    #[cfg(unix)]
    fn command_with_exit_code(exit_code: i32) -> std::process::Command {
        let mut command = std::process::Command::new("sh");
        command
            .arg("-c")
            .arg("exit \"$1\"")
            .arg("browser-launcher-test")
            .arg(exit_code.to_string());
        command
    }

    #[cfg(windows)]
    fn command_with_exit_code(exit_code: i32) -> std::process::Command {
        let mut command = std::process::Command::new("cmd");
        command
            .arg("/D")
            .arg("/C")
            .arg(format!("exit /B {exit_code}"));
        command
    }

    #[test]
    fn reaped_launcher_maps_success_and_nonzero_status() {
        wait_for_browser_launcher(command_with_exit_code(0)).unwrap();

        let error = wait_for_browser_launcher(command_with_exit_code(7)).unwrap_err();
        assert_eq!(error.to_string(), "browser launcher exited unsuccessfully");
    }

    #[cfg(unix)]
    #[test]
    fn reaped_launcher_error_never_contains_url_argument() {
        let mut command = command_with_exit_code(9);
        command.arg(SENSITIVE_URL);

        let error = wait_for_browser_launcher(command).unwrap_err();
        assert!(!error.to_string().contains(SENSITIVE_URL));
        assert!(!format!("{error:?}").contains(SENSITIVE_URL));
        assert!(!format!("{error:#}").contains("must-not-appear"));
    }

    #[test]
    fn reaped_launcher_start_error_never_contains_url_argument() {
        let mut command = std::process::Command::new(
            "deppy-browser-launcher-test-intentionally-missing-executable-7adf87ac",
        );
        command.arg(SENSITIVE_URL);

        let error = wait_for_browser_launcher(command).unwrap_err();
        assert!(!error.to_string().contains(SENSITIVE_URL));
        assert!(!format!("{error:?}").contains(SENSITIVE_URL));
        assert!(!format!("{error:#}").contains("must-not-appear"));
    }
}
