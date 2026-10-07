//! 루프백 PWA의 연결 화면을 실제 브라우저에서 검사한다. Deppy 앱 프로세스는 실행하지 않는다.
mod chrome_support;

use chrome_support::{StaticFiles, StaticServer};

const INDEX: &str = include_str!("../assets/index.html");
const APP_JS: &str = include_str!("../assets/app.js");
const APP_CSS: &str = include_str!("../assets/app.css");
const VIEWER_JS: &str = include_str!("../../../web/shared/viewer-core.js");
const VIEWER_CSS: &str = include_str!("../../../web/shared/viewer-core.css");
const THEME_CSS: &str = include_str!("../../../web/shared/mobile-theme.css");
const FIXTURE: &str = include_str!("fixtures/mobile-shell-parity.js");

#[test]
#[ignore = "requires a locally installed Chromium-family browser"]
fn connected_mobile_shell_matches_workspace_and_terminal_flow() {
    let chrome = chrome_support::require_chrome();
    let profile = chrome_support::BrowserTempDir::new("deppy-mobile-shell");
    let html = INDEX.replace("/app.__SHELL_VERSION__.css", "/app.css").replace(
        "  <script type=\"module\" src=\"/app.__SHELL_VERSION__.js\"></script>",
        &format!(
            "<script src=\"/fixture.js\"></script><script type=\"module\" src=\"/app.js\"></script>{}",
            chrome_support::REPORTER_SCRIPT,
        ),
    );
    let mut files = StaticFiles::new();
    files.insert(
        "/runner.html".into(),
        ("text/html; charset=utf-8", html.into_bytes()),
    );
    files.insert(
        "/app.js".into(),
        (
            "text/javascript; charset=utf-8",
            format!("{VIEWER_JS}\n{APP_JS}").into_bytes(),
        ),
    );
    files.insert(
        "/app.css".into(),
        (
            "text/css; charset=utf-8",
            format!("{VIEWER_CSS}\n{THEME_CSS}\n{APP_CSS}").into_bytes(),
        ),
    );
    files.insert(
        "/fixture.js".into(),
        (
            "text/javascript; charset=utf-8",
            FIXTURE.as_bytes().to_vec(),
        ),
    );
    let server = StaticServer::start(files);
    let body = chrome_support::run_headless(
        &chrome,
        &profile.path,
        &format!("{}/runner.html", server.origin),
        &server,
        "mobile connected shell parity gate",
    );
    assert!(body.contains("MOBILE_SHELL_PARITY_OK"), "{body}");
}
