//! BrowserLauncher (설계 §9) — external browser로 authorize URL을 연다 (§1.5).
//! 앱 내 webview 금지 방향이므로 OS 기본 브라우저 커맨드만 쓴다.

use anyhow::Context;

pub fn open_in_browser(url: &str) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(target_os = "windows")]
    let mut command = {
        // cmd /C start는 URL의 &를 셸 구문으로 재해석해 잘라먹는다 —
        // rundll32는 인자를 그대로 URL 핸들러에 넘기므로 안전
        let mut c = std::process::Command::new("rundll32");
        c.arg("url.dll,FileProtocolHandler").arg(url);
        c
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut command = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };
    command
        .spawn()
        .with_context(|| format!("브라우저 열기 실패: {url}"))?;
    Ok(())
}
