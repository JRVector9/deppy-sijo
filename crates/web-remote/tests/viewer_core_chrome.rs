//! 공유 보기 전용 뷰어(`web/shared/viewer-core.js`)를 격리된 실제 브라우저에서 돌리는 게이트.
//!
//! 러너(`fixtures/viewer-core-contract.js`)는 hooks 없는 시청 전용 셸에서 코어가 스스로
//! 초기화되고 문서화된 wire 순서(watch → request_keyframe → unwatch)를 내는지 확인한다.
//! 코어와 러너를 같은 `<script type="module">`에 이어 붙인다 — loopback 셸이 서빙하는 번들과
//! 같은 합성 방식이다.

mod chrome_support;

use chrome_support::{StaticFiles, StaticServer};

const VIEWER_CORE_JS: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../web/shared/viewer-core.js"
));
const CONTRACT_JS: &str = include_str!("fixtures/viewer-core-contract.js");

#[test]
#[ignore = "requires a locally installed Chromium-family browser"]
fn the_shared_viewer_core_runs_its_contract_in_a_real_browser() {
    let chrome = chrome_support::require_chrome();
    let profile = chrome_support::BrowserTempDir::new("deppy-viewer-core");
    let script = format!("{VIEWER_CORE_JS}\n{CONTRACT_JS}").replace("</script", "<\\/script");
    let html = format!(
        "<!doctype html><meta charset=\"utf-8\"><body data-status=\"running\">VIEWER_CORE_RUNNING\
         <script type=\"module\">{script}</script>{}</body>",
        chrome_support::REPORTER_SCRIPT
    );
    let mut files = StaticFiles::new();
    files.insert(
        "/runner.html".to_owned(),
        ("text/html; charset=utf-8", html.into_bytes()),
    );
    let server = StaticServer::start(files);

    let body = chrome_support::run_headless(
        &chrome,
        &profile.path,
        &format!("{}/runner.html", server.origin),
        &server,
        "shared viewer-core contract gate",
    );
    assert!(
        body.contains("VIEWER_CORE_OK:watch,request_keyframe,unwatch"),
        "unexpected viewer-core wire sequence:\n{body}"
    );
}
