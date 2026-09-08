//! 프로덕션 Relay 셸을 **그대로** 격리된 실제 브라우저에서 돌리는 게이트.
//!
//! `web/relay-shell/{relay-crypto,relay-shell}.js`를 복사 없이 import한 러너
//! (`fixtures/relay-shell-v1.js`)가 Rust 고정 벡터(`fixtures/relay-hello-v1.json`)와
//! 대조하고, 가짜 소켓 위에서 페어링 전 과정을 끝까지 돌린다. ES 모듈은 `file://`에서
//! import할 수 없으므로 loopback HTTP로 서빙한다 — 정적 파일뿐이며 바깥 네트워크는 없다.

mod chrome_support;

use std::path::Path;

use p256::ecdsa::{Signature, VerifyingKey, signature::Verifier as _};
use relay_protocol::AdmissionCredential;
use web_remote::relay::contract::PairingId;
use web_remote::relay::pairing::PairingSecret;
use web_remote::relay_client::encode_pairing_link;

use chrome_support::{StaticFiles, StaticServer};

const FIXTURE_JSON: &str = include_str!("fixtures/relay-hello-v1.json");
const RUNNER_JS: &str = include_str!("fixtures/relay-shell-v1.js");
const SHELL_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../web/relay-shell");
const SHARED_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../web/shared");
const LINK_HANDLE: [u8; 32] = [0xa1; 32];

#[test]
#[ignore = "requires a locally installed Chromium-family browser"]
fn the_production_shell_pairs_end_to_end_against_the_rust_vectors() {
    let chrome = chrome_support::require_chrome();
    let profile = chrome_support::BrowserTempDir::new("deppy-relay-shell");
    let fixture = fixture_with_link();
    let server = StaticServer::start(shell_files(&fixture));
    let url = format!("{}/runner.html", server.origin);

    let body = chrome_support::run_headless(
        &chrome,
        &profile.path,
        &url,
        &server,
        "production Relay shell gate",
    );
    verify_device_signature(&body, &fixture).unwrap_or_else(|error| {
        panic!("browser-produced device signature failed Rust verification: {error}\n{body}")
    });
}

/// 러너 문서와 셸 자산. 러너 문서는 프로덕션 `index.html`에서 자동 부팅만 뗀 것이다 — 마크업은
/// 그대로라서 셸이 실제 화면 요소(`#verify-code`, `#session-mount`)를 찾는다.
fn shell_files(fixture: &serde_json::Value) -> StaticFiles {
    let shell = Path::new(SHELL_DIR);
    let shared = Path::new(SHARED_DIR);
    let mut files = StaticFiles::new();
    for (name, mime) in [
        ("relay-shell.js", "text/javascript"),
        ("relay-terminal.js", "text/javascript"),
        ("relay-crypto.js", "text/javascript"),
        ("relay-shell.css", "text/css"),
        ("sw.js", "text/javascript"),
        ("manifest.webmanifest", "application/manifest+json"),
    ] {
        files.insert(format!("/{name}"), (mime, read(&shell.join(name))));
    }
    files.insert(
        "/mobile-theme.css".to_owned(),
        ("text/css", read(&shared.join("mobile-theme.css"))),
    );
    let index = String::from_utf8(read(&shell.join("index.html"))).expect("index.html utf-8");
    let boot = "<script type=\"module\" src=\"./relay-shell.js\"></script>";
    assert!(index.contains(boot), "index.html must boot relay-shell.js");
    assert!(
        index.contains("id=\"relay-shell-root\""),
        "index.html must mark the production root"
    );
    assert!(index.contains("</body>"), "index.html must close its body");
    let fixture_json = fixture.to_string().replace("</script", "<\\/script");
    let runner = index
        .replace("__RELAY_ORIGIN__", "wss://relay.example.test")
        .replace("__SHELL_ORIGIN__", "http://127.0.0.1")
        // 프로덕션 루트 id를 바꿔 모듈의 자동 부팅을 막는다. 러너가 셸을 직접 만든다.
        .replace(
            "id=\"relay-shell-root\"",
            "id=\"relay-shell-root-under-test\"",
        )
        .replace(
            boot,
            &format!(
                "<script type=\"application/json\" id=\"relay-fixture\">{fixture_json}</script>\
                 <script type=\"module\" src=\"./runner.js\"></script>"
            ),
        )
        .replace(
            "</body>",
            &format!("{}</body>", chrome_support::REPORTER_SCRIPT),
        );
    files.insert(
        "/runner.html".to_owned(),
        ("text/html; charset=utf-8", runner.into_bytes()),
    );
    files.insert(
        "/runner.js".to_owned(),
        ("text/javascript", RUNNER_JS.as_bytes().to_vec()),
    );
    files
}

/// Rust가 만든 페어링 링크를 fixture에 얹는다 — 셸의 `parsePairingLink`가 이걸 되읽는다.
fn fixture_with_link() -> serde_json::Value {
    let mut fixture: serde_json::Value = serde_json::from_str(FIXTURE_JSON).expect("fixture json");
    let proof = &fixture["pairing_proof"];
    let pairing_id: [u8; 16] = fixture_hex(proof, "pairing_id_hex").try_into().unwrap();
    let mut secret: [u8; 32] = fixture_hex(proof, "secret_hex").try_into().unwrap();
    let link = encode_pairing_link(
        "https://shell.example.test",
        &AdmissionCredential::from_bytes(LINK_HANDLE),
        PairingId::from_bytes(pairing_id),
        &PairingSecret::take_from_bytes(&mut secret),
    );
    let (_, fragment) = link.split_once("/#").expect("link fragment");
    fixture["pairing_link"] = serde_json::json!({
        "fragment": fragment,
        "admission_handle_hex": hex(&LINK_HANDLE),
    });
    fixture
}

fn signature_hex(body: &str) -> Option<&str> {
    let payload = body.split_once("RELAY_SHELL_OK:")?.1;
    let end = payload
        .find(|c: char| !c.is_ascii_hexdigit())
        .unwrap_or(payload.len());
    let payload = &payload[..end];
    (payload.len() == 128).then_some(payload)
}

fn verify_device_signature(body: &str, fixture: &serde_json::Value) -> Result<(), String> {
    let public_key = fixture_hex(&fixture["device"], "identity_public_sec1_hex");
    let transcript = fixture_hex(fixture, "transcript_hex");
    let signature = chrome_support::decode_hex(
        signature_hex(body).ok_or_else(|| "browser signature payload is missing".to_owned())?,
    )?;
    let verifying_key =
        VerifyingKey::from_sec1_bytes(&public_key).map_err(|error| error.to_string())?;
    let signature = Signature::from_slice(&signature).map_err(|error| error.to_string())?;
    verifying_key
        .verify(&transcript, &signature)
        .map_err(|error| error.to_string())
}

fn fixture_hex(value: &serde_json::Value, field: &str) -> Vec<u8> {
    chrome_support::decode_hex(
        value[field]
            .as_str()
            .unwrap_or_else(|| panic!("fixture {field} is missing")),
    )
    .unwrap_or_else(|error| panic!("fixture {field}: {error}"))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn read(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}
