//! Actual canvas calls must advance by terminal owner cells, not scalar count.
mod chrome_support;
use chrome_support::{StaticFiles, StaticServer};

#[test]
#[ignore = "requires a locally installed Chromium-family browser"]
fn browser_canvas_preserves_graphemes_and_cell_advances() {
    let chrome = chrome_support::require_chrome();
    let profile = chrome_support::BrowserTempDir::new("deppy-viewer-grapheme");
    let runner = r#"
const painted = [];
const originalFillText = CanvasRenderingContext2D.prototype.fillText;
CanvasRenderingContext2D.prototype.fillText = function(text, x, y, maxWidth) {
  painted.push({text, x, y, maxWidth});
  return originalFillText.call(this, text, x, y, maxWidth);
};
const viewer = createViewer({send: () => true});
viewer.wrap.style.width = '400px';
viewer.wrap.style.height = '240px';
viewer.setViewerConnection('connected');
viewer.openViewer('unicode', 'unicode');
viewer.handleViewport({session:'unicode', keyframe:true, cols:10, rows:3,
  cursor:{visible:false}, lines:[{row:0, runs:[
    {s:0,t:'가ᇹ한',g:['가ᇹ','한'],w:true},
    {s:4,t:'a\u0301\u0308Z',g:['a\u0301\u0308','Z']}
  ]}]});
requestAnimationFrame(() => requestAnimationFrame(() => {
  try {
    const first = painted.findIndex(call => call.text === '가ᇹ');
    if (first < 0) throw new Error('old Hangul cluster was split into scalar canvas calls');
    const calls = painted.slice(first, first+4);
    if (calls.map(call=>call.text).join('|') !== '가ᇹ|한|a\u0301\u0308|Z') throw new Error('cluster text lost');
    if (Math.abs(calls[1].x-calls[0].x-calls[0].maxWidth)>0.001) throw new Error('wide owner advance incorrect');
    if (Math.abs(calls[3].x-calls[2].x-calls[2].maxWidth)>0.001) throw new Error('narrow owner advance incorrect');
    document.body.dataset.status='ok'; document.body.textContent='VIEWER_GRAPHEME_OK';
  } catch(error) { document.body.dataset.status='error'; document.body.textContent=String(error); }
}));
"#;
    let script = format!(
        "{}\n{runner}",
        include_str!("../../../web/shared/viewer-core.js")
    );
    let html = format!(
        "<!doctype html><body data-status=running><script type=module>{script}</script>{}</body>",
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
        "grapheme canvas",
    );
    assert!(report.contains("VIEWER_GRAPHEME_OK"));
}
