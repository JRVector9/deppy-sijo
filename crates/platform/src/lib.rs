//! Platform crate (설계문서 9장). PR-13: OS notification.
//! Clipboard/OpenExternalBrowser/PackagingHelpers는 후속 PR.

const APP_NAME: &str = "Deppy Sijo";

/// OS 알림을 띄운다. 실패해도 앱 흐름을 막지 않는다 (로그만).
///
/// macOS는 osascript `display notification`을 쓴다 — notify-rust(mac-notification-sys)는
/// 앱 이름으로 번들을 검색하다 실패하면 Finder "Choose Application" 다이얼로그를 띄우고,
/// 거기서 취소하면 FFI 블록 안 panic → `panic_cannot_unwind` → abort로 **앱 전체가
/// 죽는다** (2026-07-05 크래시 리포트 실증: pane exit 알림 → 다이얼로그 Cancel → SIGABRT).
#[cfg(target_os = "macos")]
pub fn notify(summary: &str, body: &str) {
    let script = format!(
        "display notification {} with title {} subtitle {}",
        applescript_quote(body),
        applescript_quote(APP_NAME),
        applescript_quote(summary),
    );
    // 자식 프로세스 wait를 별도 스레드에서 — UI 스레드 블로킹과 좀비 프로세스 방지.
    std::thread::spawn(move || {
        match std::process::Command::new("osascript")
            .arg("-e")
            .arg(&script)
            .status()
        {
            Ok(status) if !status.success() => {
                tracing::warn!("OS 알림 실패: osascript {status}");
            }
            Err(e) => tracing::warn!("OS 알림 실패: {e}"),
            Ok(_) => {}
        }
    });
}

/// AppleScript 문자열 리터럴 이스케이프 — 백슬래시·큰따옴표만 특수문자다.
#[cfg(target_os = "macos")]
fn applescript_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(not(target_os = "macos"))]
pub fn notify(summary: &str, body: &str) {
    // Linux는 D-Bus 데몬 부재, headless 환경 등에서 실패할 수 있다.
    let result = notify_rust::Notification::new()
        .appname(APP_NAME)
        .summary(summary)
        .body(body)
        .show();
    if let Err(e) = result {
        tracing::warn!("OS 알림 실패: {e}");
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn applescript_quote는_따옴표와_백슬래시를_이스케이프() {
        assert_eq!(applescript_quote("plain"), "\"plain\"");
        assert_eq!(applescript_quote("a\"b"), "\"a\\\"b\"");
        assert_eq!(applescript_quote("a\\b"), "\"a\\\\b\"");
        assert_eq!(
            applescript_quote("셸 종료 \"zsh\" (exit 0)"),
            "\"셸 종료 \\\"zsh\\\" (exit 0)\""
        );
    }
}
