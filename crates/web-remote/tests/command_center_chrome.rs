//! Production bundle + isolated transport; never launches Deppy or an agent.
mod chrome_support;
use chrome_support::{StaticFiles, StaticServer};

#[test]
#[ignore = "requires a locally installed Chromium-family browser"]
fn command_center_production_shell_preserves_drafts_and_terminal_roundtrip() {
    let chrome = chrome_support::require_chrome();
    let profile = chrome_support::BrowserTempDir::new("deppy-command-center-shell");
    let html = include_str!("../assets/index.html")
        .replace("/app.__SHELL_VERSION__.css", "/app.css")
        .replace(
            "  <script type=\"module\" src=\"/app.__SHELL_VERSION__.js\"></script>",
            &format!("<script src=\"/fixture.js\"></script><script type=\"module\" src=\"/app.js\"></script>{}", chrome_support::REPORTER_SCRIPT),
        );
    let mut files = StaticFiles::new();
    files.insert(
        "/runner.html".into(),
        ("text/html; charset=utf-8", html.into_bytes()),
    );
    for (path, mime) in [("/app.js", "text/javascript"), ("/app.css", "text/css")] {
        let response = web_remote::static_srv::respond(path, "", "fixture-only");
        assert_eq!(response.status, 200);
        files.insert(path.into(), (mime, response.body.into_owned()));
    }
    files.insert(
        "/fixture.js".into(),
        (
            "text/javascript",
            include_str!("fixtures/command-center-shell.js")
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
        "production Command Center shell",
    );
    assert!(report.contains("COMMAND_CENTER_SHELL_OK:"), "{report}");
}
