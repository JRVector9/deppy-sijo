//! Actual production cloud PWA, isolated synthetic transport, no Deppy process.
mod chrome_support;
use chrome_support::{StaticFiles, StaticServer};

#[test]
#[ignore = "requires a locally installed Chromium-family browser"]
fn mobile_cloud_results_keep_source_identity_and_owner_controls() {
    let chrome = chrome_support::require_chrome();
    let profile = chrome_support::BrowserTempDir::new("deppy-cloud-results-mobile");
    let html = include_str!("../assets/index.html").replace("/app.__SHELL_VERSION__.css", "/app.css")
        .replace("  <script type=\"module\" src=\"/app.__SHELL_VERSION__.js\"></script>",
            &format!("<script src=\"/fixture.js\"></script><script type=\"module\" src=\"/app.js\"></script>{}", chrome_support::REPORTER_SCRIPT));
    let mut files = StaticFiles::new();
    files.insert(
        "/runner.html".into(),
        ("text/html; charset=utf-8", html.into_bytes()),
    );
    files.insert(
        "/app.js".into(),
        (
            "text/javascript",
            concat!(
                include_str!("../../../web/shared/viewer-core.js"),
                "\n",
                include_str!("../../../web/shared/cloud-results.js"),
                "\n",
                include_str!("../assets/app.js")
            )
            .as_bytes()
            .to_vec(),
        ),
    );
    files.insert(
        "/app.css".into(),
        (
            "text/css",
            concat!(
                include_str!("../../../web/shared/viewer-core.css"),
                "\n",
                include_str!("../../../web/shared/mobile-theme.css"),
                "\n",
                include_str!("../../../web/shared/cloud-results.css"),
                "\n",
                include_str!("../assets/app.css")
            )
            .as_bytes()
            .to_vec(),
        ),
    );
    files.insert(
        "/fixture.js".into(),
        (
            "text/javascript",
            include_str!("fixtures/cloud-results-mobile.js")
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
        "mobile cloud results",
    );
    assert!(report.contains("CLOUD_RESULTS_MOBILE_OK:"), "{report}");
}
