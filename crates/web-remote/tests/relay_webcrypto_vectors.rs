use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use p256::ecdsa::{Signature, VerifyingKey, signature::Verifier as _};

const FIXTURE_JSON: &str = include_str!("fixtures/relay-webcrypto-v1.json");
const BROWSER_RUNNER: &str = include_str!("fixtures/relay-webcrypto-v1.js");

#[test]
#[ignore = "requires a locally installed Chromium-family browser"]
fn fixed_relay_vectors_match_real_browser_webcrypto() {
    let chrome = chrome_path().unwrap_or_else(|| {
        panic!("set CHROME_PATH/CHROME_BIN or install Google Chrome/Chromium to run this gate")
    });
    let temp = BrowserTempDir::new();
    let html_path = temp.path.join("relay-webcrypto-v1.html");
    let stdout_path = temp.path.join("chrome.stdout");
    let stderr_path = temp.path.join("chrome.stderr");
    let fixture = FIXTURE_JSON.replace("</script", "<\\/script");
    let html = format!(
        "<!doctype html><meta charset=\"utf-8\"><body data-status=\"running\">RELAY_WEBCRYPTO_RUNNING<script type=\"application/json\" id=\"relay-fixture\">{fixture}</script><script>{BROWSER_RUNNER}</script>"
    );
    std::fs::write(&html_path, html).expect("write isolated WebCrypto fixture runner");

    let mut child = Command::new(&chrome)
        .arg("--headless=new")
        .arg("--disable-gpu")
        .arg("--disable-background-networking")
        .arg("--disable-component-update")
        .arg("--disable-sync")
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--virtual-time-budget=10000")
        .arg(format!("--user-data-dir={}", temp.path.display()))
        .arg("--dump-dom")
        .arg(file_url(&html_path))
        .stdout(Stdio::from(
            std::fs::File::create(&stdout_path).expect("create Chrome stdout capture"),
        ))
        .stderr(Stdio::from(
            std::fs::File::create(&stderr_path).expect("create Chrome stderr capture"),
        ))
        .spawn()
        .unwrap_or_else(|error| panic!("launch {}: {error}", chrome.display()));
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        let current_stdout = read_capture(&stdout_path);
        if current_stdout.contains("<body data-status=\"error\">") {
            child
                .kill()
                .expect("stop isolated Chrome after failed DOM dump");
            let _ = child.wait();
            panic!(
                "real-browser WebCrypto vector gate failed\nstdout:\n{}\nstderr:\n{}",
                current_stdout,
                read_capture(&stderr_path)
            );
        }
        if browser_completed_successfully(&current_stdout) {
            child
                .kill()
                .expect("stop isolated Chrome after completed DOM dump");
            let _ = child.wait();
            break None;
        }
        if let Some(status) = child.try_wait().expect("poll isolated Chrome runner") {
            break Some(status);
        }
        if Instant::now() >= deadline {
            child
                .kill()
                .expect("terminate timed-out isolated Chrome runner");
            let _ = child.wait();
            panic!(
                "real-browser WebCrypto vector gate timed out\nstdout:\n{}\nstderr:\n{}",
                read_capture(&stdout_path),
                read_capture(&stderr_path)
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let stdout = read_capture(&stdout_path);
    let stderr = read_capture(&stderr_path);
    assert!(
        status.is_none_or(|status| status.success()) && browser_completed_successfully(&stdout),
        "real-browser WebCrypto vector gate failed\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        status.map_or_else(
            || "completed marker".to_owned(),
            |status| status.to_string()
        ),
        stdout,
        stderr
    );
    verify_browser_signature(&stdout).unwrap_or_else(|error| {
        panic!("browser-produced Relay signature failed Rust verification: {error}")
    });
}

fn browser_completed_successfully(dom: &str) -> bool {
    browser_signature_hex(dom).is_some()
}

fn browser_signature_hex(dom: &str) -> Option<&str> {
    const PREFIX: &str = "<body data-status=\"ok\">RELAY_WEBCRYPTO_OK:";
    let payload = dom.split_once(PREFIX)?.1.split_once("</body>")?.0;
    (payload.len() == 128 && payload.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then_some(payload)
}

fn verify_browser_signature(dom: &str) -> Result<(), String> {
    let fixture: serde_json::Value =
        serde_json::from_str(FIXTURE_JSON).map_err(|error| error.to_string())?;
    let device = fixture
        .get("device")
        .ok_or_else(|| "fixture device is missing".to_owned())?;
    let public_key = fixture_hex(device, "identity_public_sec1_hex")?;
    let transcript = fixture_hex(&fixture, "transcript_hex")?;
    let signature = decode_hex(
        browser_signature_hex(dom)
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
    decode_hex(
        value
            .get(field)
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("fixture {field} is missing"))?,
    )
}

fn decode_hex(value: &str) -> Result<Vec<u8>, String> {
    if !value.len().is_multiple_of(2) {
        return Err("hex value has odd length".to_owned());
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let pair = std::str::from_utf8(pair).map_err(|error| error.to_string())?;
            u8::from_str_radix(pair, 16).map_err(|error| error.to_string())
        })
        .collect()
}

fn read_capture(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_else(|error| format!("<failed to read {}: {error}>", path.display()))
}

fn chrome_path() -> Option<PathBuf> {
    for variable in ["CHROME_PATH", "CHROME_BIN"] {
        if let Some(path) = std::env::var_os(variable).map(PathBuf::from) {
            return path.is_file().then_some(path);
        }
    }
    [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/usr/bin/google-chrome",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
    ]
    .into_iter()
    .map(PathBuf::from)
    .find(|path| path.is_file())
}

fn file_url(path: &Path) -> String {
    let raw = path
        .canonicalize()
        .expect("canonicalize WebCrypto runner path")
        .to_string_lossy()
        .replace('%', "%25")
        .replace(' ', "%20")
        .replace('#', "%23");
    format!("file://{raw}")
}

struct BrowserTempDir {
    path: PathBuf,
}

impl BrowserTempDir {
    fn new() -> Self {
        let unique = format!(
            "deppy-relay-webcrypto-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time after Unix epoch")
                .as_nanos()
        );
        let path = std::env::temp_dir().join(unique);
        std::fs::create_dir(&path).expect("create isolated Chrome profile directory");
        Self { path }
    }
}

impl Drop for BrowserTempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[test]
fn browser_completion_marker_requires_executed_success_state_and_signature() {
    assert!(!browser_completed_successfully(
        r#"<body data-status="running"><script>document.body.textContent = "RELAY_WEBCRYPTO_OK"</script></body>"#
    ));
    assert!(!browser_completed_successfully(
        r#"<body data-status="error">RELAY_WEBCRYPTO_ERROR: failed; RELAY_WEBCRYPTO_OK</body>"#
    ));
    assert!(!browser_completed_successfully(
        r#"<body data-status="ok">RELAY_WEBCRYPTO_OK</body>"#
    ));

    let fixture: serde_json::Value = serde_json::from_str(FIXTURE_JSON).unwrap();
    let signature = fixture["device"]["signature_raw_hex"].as_str().unwrap();
    let completed = format!(r#"<body data-status="ok">RELAY_WEBCRYPTO_OK:{signature}</body>"#);
    assert!(browser_completed_successfully(&completed));
    assert!(verify_browser_signature(&completed).is_ok());

    let tampered = "00".repeat(64);
    let tampered = format!(r#"<body data-status="ok">RELAY_WEBCRYPTO_OK:{tampered}</body>"#);
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
