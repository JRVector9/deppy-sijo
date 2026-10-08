//! Reading mode retains text selection while the coordinate grid remains exact.
mod chrome_support;
use chrome_support::{StaticFiles, StaticServer};

#[test]
#[ignore = "requires a locally installed Chromium-family browser"]
fn browser_viewer_retains_selection_search_and_local_history() {
    let chrome = chrome_support::require_chrome();
    let profile = chrome_support::BrowserTempDir::new("deppy-viewer-reading");
    let script = format!(
        "{}\n{}",
        include_str!("../../../web/shared/viewer-core.js"),
        include_str!("fixtures/viewer-reading-contract.js")
    )
    .replace("</script", "<\\/script");
    let html = format!(
        "<!doctype html><meta charset=\"utf-8\"><style>{}</style><body data-status=\"running\">\
         <script type=\"module\">{script}</script>{}</body>",
        include_str!("../../../web/shared/viewer-core.css"),
        chrome_support::REPORTER_SCRIPT
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
        "terminal reading mode",
    );
    assert!(report.contains("VIEWER_READING_OK:"), "{report}");
}
