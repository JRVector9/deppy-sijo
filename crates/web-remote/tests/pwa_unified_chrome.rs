//! Approved mobile design contracts against isolated actual Chromium and production assets.
mod chrome_support;

use chrome_support::{StaticFiles, StaticServer};

const INDEX: &str = include_str!("../assets/index.html");
const CORE_JS: &str = include_str!("../../../web/shared/viewer-core.js");
const CORE_CSS: &str = include_str!("../../../web/shared/viewer-core.css");
const THEME_CSS: &str = include_str!("../../../web/shared/mobile-theme.css");
const APP_JS: &str = concat!(
    include_str!("../../../web/shared/cloud-results.js"),
    "\n",
    include_str!("../assets/app.js")
);
const APP_CSS: &str = concat!(
    include_str!("../../../web/shared/cloud-results.css"),
    "\n",
    include_str!("../assets/app.css")
);

#[test]
#[ignore = "requires a locally installed Chromium-family browser"]
fn approved_mobile_design_unified_input_and_workspace_rows() {
    let chrome = chrome_support::require_chrome();
    let profile = chrome_support::BrowserTempDir::new("deppy-pwa-unified-mobile-ui");
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
            format!("{CORE_JS}\n{APP_JS}").into_bytes(),
        ),
    );
    files.insert(
        "/app.css".into(),
        (
            "text/css; charset=utf-8",
            format!("{CORE_CSS}\n{THEME_CSS}\n{APP_CSS}").into_bytes(),
        ),
    );
    files.insert(
        "/fixture.js".into(),
        (
            "text/javascript; charset=utf-8",
            include_str!("fixtures/pwa-unified-mobile-ui.js")
                .as_bytes()
                .to_vec(),
        ),
    );
    let server = StaticServer::start(files);
    let report = chrome_support::run_headless(
        &chrome,
        &profile.path,
        &format!("{}/runner.html", server.origin),
        &server,
        "approved PWA unified mobile design",
    );
    assert!(report.contains("PWA_UNIFIED_MOBILE_UI_OK:"), "{report}");
}

#[test]
#[ignore = "requires a locally installed Chromium-family browser"]
fn approved_mobile_design_continuous_bounded_history() {
    let chrome = chrome_support::require_chrome();
    let profile = chrome_support::BrowserTempDir::new("deppy-pwa-unified-renderer");
    let script = format!(
        "{CORE_JS}\n{}",
        include_str!("fixtures/pwa-unified-renderer.js"),
    )
    .replace("</script", "<\\/script");
    let html = format!(
        "<!doctype html><meta charset=\"utf-8\"><style>{CORE_CSS}\n{THEME_CSS}</style>\
         <body data-status=\"running\"><script type=\"module\">{script}</script>{}</body>",
        chrome_support::REPORTER_SCRIPT,
    );
    let mut files = StaticFiles::new();
    files.insert(
        "/runner.html".into(),
        ("text/html; charset=utf-8", html.into_bytes()),
    );
    let server = StaticServer::start(files);
    let report = chrome_support::run_headless(
        &chrome,
        &profile.path,
        &format!("{}/runner.html", server.origin),
        &server,
        "approved PWA continuous bounded history",
    );
    assert!(report.contains("PWA_UNIFIED_RENDERER_OK:"), "{report}");
}
