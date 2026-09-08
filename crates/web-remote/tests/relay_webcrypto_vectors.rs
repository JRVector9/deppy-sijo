//! Task 1 고정 벡터를 실제 브라우저 WebCrypto로 재현하는 게이트.
//!
//! 러너(`fixtures/relay-webcrypto-v1.js`)는 배포되는 `web/relay-shell/relay-crypto.js`를
//! **복사 없이** import한다(계획 Task 6 Step 5). ES 모듈은 `file://`에서 import할 수 없으므로
//! `relay_shell_chrome`과 같은 loopback HTTP로 서빙한다 — 정적 파일뿐이고 바깥 네트워크는 없다.

mod chrome_support;

use p256::ecdsa::{Signature, VerifyingKey, signature::Verifier as _};

use chrome_support::{StaticFiles, StaticServer};

const FIXTURE_JSON: &str = include_str!("fixtures/relay-webcrypto-v1.json");
const BROWSER_RUNNER: &str = include_str!("fixtures/relay-webcrypto-v1.js");
const CRYPTO_MODULE: &str = include_str!("../../../web/relay-shell/relay-crypto.js");

#[test]
#[ignore = "requires a locally installed Chromium-family browser"]
fn fixed_relay_vectors_match_real_browser_webcrypto() {
    let chrome = chrome_support::require_chrome();
    let profile = chrome_support::BrowserTempDir::new("deppy-relay-webcrypto");
    let server = StaticServer::start(runner_files());
    let body = chrome_support::run_headless(
        &chrome,
        &profile.path,
        &format!("{}/runner.html", server.origin),
        &server,
        "real-browser WebCrypto vector gate",
    );
    verify_browser_signature(&body).unwrap_or_else(|error| {
        panic!("browser-produced Relay signature failed Rust verification: {error}\n{body}")
    });
}

/// 러너 문서와 자산. 크립토 모듈은 **작업 트리의 그 파일 그대로**를 서빙한다 — 테스트용
/// 사본을 만들면 게이트가 배포물이 아니라 사본을 검증하게 된다.
fn runner_files() -> StaticFiles {
    let fixture = FIXTURE_JSON.replace("</script", "<\\/script");
    let runner = format!(
        "<!doctype html><meta charset=\"utf-8\"><body data-status=\"running\">\
         <script type=\"application/json\" id=\"relay-fixture\">{fixture}</script>\
         <script type=\"module\" src=\"./runner.js\"></script>{}</body>",
        chrome_support::REPORTER_SCRIPT
    );
    let mut files = StaticFiles::new();
    files.insert(
        "/runner.html".to_owned(),
        ("text/html; charset=utf-8", runner.into_bytes()),
    );
    files.insert(
        "/runner.js".to_owned(),
        ("text/javascript", BROWSER_RUNNER.as_bytes().to_vec()),
    );
    files.insert(
        "/relay-crypto.js".to_owned(),
        ("text/javascript", CRYPTO_MODULE.as_bytes().to_vec()),
    );
    files
}

fn browser_signature_hex(body: &str) -> Option<&str> {
    const PREFIX: &str = "RELAY_WEBCRYPTO_OK:";
    let payload = body.split_once(PREFIX)?.1.trim();
    (payload.len() == 128 && payload.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then_some(payload)
}

fn verify_browser_signature(body: &str) -> Result<(), String> {
    let fixture: serde_json::Value =
        serde_json::from_str(FIXTURE_JSON).map_err(|error| error.to_string())?;
    let device = fixture
        .get("device")
        .ok_or_else(|| "fixture device is missing".to_owned())?;
    let public_key = fixture_hex(device, "identity_public_sec1_hex")?;
    let transcript = fixture_hex(&fixture, "transcript_hex")?;
    let signature = chrome_support::decode_hex(
        browser_signature_hex(body)
            .ok_or_else(|| "browser signature payload is missing".to_owned())?,
    )?;
    let verifying_key =
        VerifyingKey::from_sec1_bytes(&public_key).map_err(|error| error.to_string())?;
    let signature = Signature::from_slice(&signature).map_err(|error| error.to_string())?;
    verifying_key
        .verify(&transcript, &signature)
        .map_err(|error| error.to_string())
}

fn fixture_hex(value: &serde_json::Value, field: &str) -> Result<Vec<u8>, String> {
    chrome_support::decode_hex(
        value
            .get(field)
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("fixture {field} is missing"))?,
    )
}

/// 성공 표식만으로는 통과할 수 없다 — 러너가 **실제로 서명한** 64바이트가 있어야 하고,
/// 그 서명이 Rust 검증을 통과해야 한다. (`ok`/`error` 상태 판정은 `chrome_support`가 한다.)
#[test]
fn browser_completion_marker_requires_an_executed_signature() {
    assert!(browser_signature_hex("RELAY_WEBCRYPTO_OK").is_none());
    assert!(browser_signature_hex("RELAY_WEBCRYPTO_ERROR: failed").is_none());
    assert!(browser_signature_hex(&format!("RELAY_WEBCRYPTO_OK:{}", "zz".repeat(64))).is_none());

    let fixture: serde_json::Value = serde_json::from_str(FIXTURE_JSON).unwrap();
    let signature = fixture["device"]["signature_raw_hex"].as_str().unwrap();
    let completed = format!("RELAY_WEBCRYPTO_OK:{signature}");
    assert_eq!(browser_signature_hex(&completed), Some(signature));
    assert!(verify_browser_signature(&completed).is_ok());

    let tampered = format!("RELAY_WEBCRYPTO_OK:{}", "00".repeat(64));
    assert!(verify_browser_signature(&tampered).is_err());
}

#[test]
fn fixture_separates_identity_signing_and_ephemeral_derivation_keys() {
    let fixture: serde_json::Value = serde_json::from_str(FIXTURE_JSON).unwrap();
    for role in ["desktop", "device"] {
        assert_eq!(
            fixture[role]["identity_private_jwk"]["key_ops"],
            serde_json::json!(["sign"]),
            "{role} identity key must only sign"
        );
        assert_eq!(
            fixture[role]["ephemeral_private_jwk"]["key_ops"],
            serde_json::json!(["deriveBits"]),
            "{role} ephemeral key must only derive ECDH bits"
        );
        assert_ne!(
            fixture[role]["identity_private_jwk"]["d"], fixture[role]["ephemeral_private_jwk"]["d"],
            "{role} identity and ephemeral private keys must stay distinct"
        );
    }
}

#[test]
fn envelope_fixtures_carry_the_sequence_the_browser_serializes() {
    let fixture: serde_json::Value = serde_json::from_str(FIXTURE_JSON).unwrap();
    for direction in ["desktop_to_device", "device_to_desktop"] {
        assert_eq!(
            fixture[direction]["sequence"],
            serde_json::json!(0),
            "{direction} must declare its initial sequence"
        );
    }
    assert!(BROWSER_RUNNER.contains("BigInt(vector.sequence)"));
    assert!(!BROWSER_RUNNER.contains("INITIAL_SEQUENCE"));
}

/// 계획 Task 6 Step 5의 **브라우저 벡터 권위** 규정. 러너는 배포되는
/// `web/relay-shell/relay-crypto.js`를 그대로 import해야 하고, 계약을 정의하는 연산을
/// 스스로 다시 구현해서는 안 된다. 복사본을 두면 셸이 틀려도 벡터 게이트가 초록으로 남는다.
#[test]
fn the_vector_runner_imports_the_shipped_crypto_module_instead_of_copying_it() {
    assert!(
        BROWSER_RUNNER.contains("from \"./relay-crypto.js\""),
        "러너는 프로덕션 크립토 모듈을 import해야 한다"
    );
    // 계약을 정의하는 값은 프로덕션 모듈에만 있어야 한다. 러너에 한 번이라도 복제되면
    // 두 구현이 갈라져도 벡터가 통과한다.
    for domain in [
        "deppy-relay-handshake-v1",
        "deppy-relay-hkdf-salt-v1",
        "deppy-relay-desktop-to-device-v1",
        "deppy-relay-device-to-desktop-v1",
        "deppy-relay-sas-v1",
        "deppy-relay-envelope-aad-v1",
    ] {
        assert!(
            !BROWSER_RUNNER.contains(domain),
            "러너가 도메인 분리 문자열을 다시 정의했다: {domain}"
        );
    }
    // 픽스처 JWK를 CryptoKey로 바꾸는 `importKey`만 러너의 몫이다(실제 기기 키는 추출
    // 불가능해서 프로덕션 모듈이 JWK를 볼 일이 없다). 나머지 SubtleCrypto 연산은 전부
    // 프로덕션 모듈을 거쳐야 한다.
    for operation in [
        "subtle.encrypt",
        "subtle.decrypt",
        "subtle.deriveBits",
        "subtle.digest",
        "subtle.sign",
        "subtle.verify",
    ] {
        assert!(
            !BROWSER_RUNNER.contains(operation),
            "러너가 계약 연산을 직접 호출했다: {operation}"
        );
    }
    // 실제로 프로덕션 구현을 거치는지 — 이름을 하나씩 고정한다.
    for symbol in [
        "buildTranscript",
        "transcriptSalt",
        "deriveSessionMaterial",
        "envelopeNonce",
        "envelopeAad",
        "RelaySecureChannel",
        "signTranscript",
        "verifyPeerSignature",
        "deriveSharedSecret",
    ] {
        assert!(
            BROWSER_RUNNER.contains(symbol),
            "러너가 프로덕션 {symbol}을 쓰지 않는다"
        );
    }
}
