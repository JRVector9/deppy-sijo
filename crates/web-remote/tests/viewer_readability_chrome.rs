//! Readable grid defaults and explicit overview are verified in isolated Chromium.
mod chrome_support;
use chrome_support::{StaticFiles, StaticServer};

#[test]
#[ignore = "requires a locally installed Chromium-family browser"]
fn browser_viewer_preserves_readable_cells_and_local_positions() {
    let chrome = chrome_support::require_chrome();
    let profile = chrome_support::BrowserTempDir::new("deppy-viewer-readability");
    let script = format!(
        "{}\n{}",
        include_str!("../../../web/shared/viewer-core.js"),
        include_str!("fixtures/viewer-readability-contract.js")
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
        "readable viewer grid",
    );
    assert!(report.contains("VIEWER_READABILITY_OK:"), "{report}");
}
