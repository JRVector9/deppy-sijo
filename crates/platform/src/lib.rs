//! Platform crate (설계문서 9장). PR-13: OS notification.
//! Clipboard/OpenExternalBrowser/PackagingHelpers는 후속 PR.

const APP_NAME: &str = "Deppy Sijo";

/// OS 알림을 띄운다. 실패해도 앱 흐름을 막지 않는다 (로그만) —
/// Linux는 D-Bus 데몬 부재, headless 환경 등에서 실패할 수 있다.
pub fn notify(summary: &str, body: &str) {
    let result = notify_rust::Notification::new()
        .appname(APP_NAME)
        .summary(summary)
        .body(body)
        .show();
    if let Err(e) = result {
        tracing::warn!("OS 알림 실패: {e}");
    }
}
