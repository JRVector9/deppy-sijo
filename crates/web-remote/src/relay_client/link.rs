//! 페어링 링크 — 폰에 건네는 유일한 1회용 재료.
//!
//! 링크는 고정 셸 오리진 뒤의 **URL 조각(fragment)** 하나다. 조각은 서버로 전송되지 않으므로
//! 셸 오리진의 접근 로그에도 남지 않으며, 셸은 읽자마자 주소를 세탁한다
//! (`web/relay-shell/relay-shell.js`의 `readPairingLinkFromLocation`).
//!
//! ```text
//! https://<shell-origin>/#base64url( admission_handle 32 || pairing_id 16 || pairing_secret 32 )
//! ```
//!
//! 셋 다 5분짜리 1회용이다. 재사용 가능한 자격증명(라우트 핸들·Mac 승인 자격증명·기기 신원)은
//! 절대 링크에 오르지 않는다.

use relay_protocol::AdmissionCredential;

use crate::relay::contract::{PairingId, RELAY_ID_BYTES};
use crate::relay::pairing::{PAIRING_SECRET_BYTES, PairingSecret};

/// 기기 입장 핸들(서버가 1회 소비하는 32바이트).
pub const ADMISSION_HANDLE_BYTES: usize = relay_protocol::ADMISSION_CREDENTIAL_BYTES;
/// 조각의 원시 바이트 수. 셸의 `PAIRING_LINK_BYTES`와 같아야 한다.
pub const PAIRING_LINK_BYTES: usize =
    ADMISSION_HANDLE_BYTES + RELAY_ID_BYTES + PAIRING_SECRET_BYTES;
/// 셸이 받아 주는 조각 최대 길이. 80바이트의 base64url은 107자다.
pub const MAX_PAIRING_FRAGMENT_CHARS: usize = 128;

/// 배포된 모바일 셸의 고정 오리진. 아직 `None`이다 — 셸 아티팩트의 DNS·TLS·레지스트리 소유자가
/// 정해지지 않았다(`deploy/relay/README.md`). 엔드포인트와 같은 이유로 비워 둔다.
pub const PRODUCTION_RELAY_SHELL_ORIGIN: Option<&str> = None;

/// 링크를 만들 셸 오리진. 프로덕션 상수가 없으면 디버그 빌드에서만 `DEPPY_RELAY_DEV_SHELL_ORIGIN`
/// 을 본다. `https://`(또는 로컬 개발용 `http://localhost`/`http://127.0.0.1`)만 받는다.
pub fn shell_origin() -> Option<String> {
    let raw = match PRODUCTION_RELAY_SHELL_ORIGIN {
        Some(origin) => origin.to_owned(),
        None => super::lifecycle::dev_override("DEPPY_RELAY_DEV_SHELL_ORIGIN")?,
    };
    let origin = raw.trim_end_matches('/');
    // 접두사 검사로는 `http://localhost.attacker.example`과 `https://good.test@attacker.example`이
    // 통과한다. 이 값은 **페어링 비밀 32바이트가 실려 나가는 URL의 목적지**이므로, 옆의
    // `RelayEndpoint::parse`처럼 authority를 실제로 분해해 판정한다.
    let (scheme, rest) = origin.split_once("://")?;
    if rest.contains(['#', '?', ' ', '@', '/']) || rest.is_empty() {
        return None;
    }
    let (host, port) = match rest.rsplit_once(':') {
        Some((host, port)) => (host, Some(port.parse::<u16>().ok()?)),
        None => (rest, None),
    };
    if port == Some(0) || host.is_empty() {
        return None;
    }
    let host = host.to_ascii_lowercase();
    let accepted = match scheme {
        "https" => {
            // 진짜 DNS 이름만. 라벨 규칙은 엔드포인트 정책과 같다.
            host.contains('.')
                && host.split('.').all(|label| {
                    !label.is_empty()
                        && label.len() <= 63
                        && !label.starts_with('-')
                        && !label.ends_with('-')
                        && label
                            .chars()
                            .all(|character| character.is_ascii_alphanumeric() || character == '-')
                })
        }
        // 로컬 개발용 예외는 정확히 이 두 이름뿐이다.
        "http" => host == "localhost" || host == "127.0.0.1",
        _ => false,
    };
    accepted.then(|| origin.to_owned())
}

/// 링크 하나를 만든다. 비밀은 여기서 한 번 읽히고 곧바로 인코딩된다 — 호출자는 이 문자열을
/// QR·복사에만 쓰고 로그에 남기지 않는다.
pub fn encode_pairing_link(
    shell_origin: &str,
    admission_handle: &AdmissionCredential,
    pairing_id: PairingId,
    secret: &PairingSecret,
) -> String {
    let mut bytes = [0u8; PAIRING_LINK_BYTES];
    let mut at = 0;
    bytes[at..at + ADMISSION_HANDLE_BYTES].copy_from_slice(admission_handle.as_bytes());
    at += ADMISSION_HANDLE_BYTES;
    bytes[at..at + RELAY_ID_BYTES].copy_from_slice(pairing_id.as_bytes());
    at += RELAY_ID_BYTES;
    bytes[at..].copy_from_slice(secret.expose());
    let fragment = base64url(&bytes);
    debug_assert!(fragment.len() <= MAX_PAIRING_FRAGMENT_CHARS);
    format!("{}/#{fragment}", shell_origin.trim_end_matches('/'))
}

/// 조각을 되읽는다(셸과 같은 판정). 링크 왕복 검증과 테스트가 쓴다.
pub fn decode_pairing_fragment(
    fragment: &str,
) -> Option<(AdmissionCredential, PairingId, [u8; PAIRING_SECRET_BYTES])> {
    let text = fragment.strip_prefix('#').unwrap_or(fragment);
    if text.is_empty()
        || text.len() > MAX_PAIRING_FRAGMENT_CHARS
        || !text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return None;
    }
    let bytes = base64url_decode(text)?;
    if bytes.len() != PAIRING_LINK_BYTES {
        return None;
    }
    let mut handle = [0u8; ADMISSION_HANDLE_BYTES];
    handle.copy_from_slice(&bytes[..ADMISSION_HANDLE_BYTES]);
    let mut id = [0u8; RELAY_ID_BYTES];
    id.copy_from_slice(&bytes[ADMISSION_HANDLE_BYTES..ADMISSION_HANDLE_BYTES + RELAY_ID_BYTES]);
    let mut secret = [0u8; PAIRING_SECRET_BYTES];
    secret.copy_from_slice(&bytes[ADMISSION_HANDLE_BYTES + RELAY_ID_BYTES..]);
    Some((
        AdmissionCredential::from_bytes(handle),
        PairingId::from_bytes(id),
        secret,
    ))
}

fn base64url(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn base64url_decode(text: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(text)
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret(byte: u8) -> PairingSecret {
        let mut bytes = [byte; PAIRING_SECRET_BYTES];
        PairingSecret::take_from_bytes(&mut bytes)
    }

    #[test]
    fn a_link_round_trips_and_stays_inside_the_shell_fragment_limit() {
        let handle = AdmissionCredential::from_bytes([0xa1; 32]);
        let id = PairingId::from_bytes([0x10; 16]);
        let link = encode_pairing_link("https://shell.example.test/", &handle, id, &secret(0x5c));
        let (origin, fragment) = link.split_once("/#").unwrap();
        assert_eq!(origin, "https://shell.example.test");
        assert_eq!(fragment.len(), 107);
        assert!(fragment.len() <= MAX_PAIRING_FRAGMENT_CHARS);
        assert!(!fragment.contains(['=', '+', '/']), "base64url, 패딩 없음");
        let (decoded_handle, decoded_id, decoded_secret) =
            decode_pairing_fragment(&format!("#{fragment}")).unwrap();
        assert!(decoded_handle.matches(&handle));
        assert_eq!(decoded_id, id);
        assert_eq!(decoded_secret, [0x5c; 32]);
    }

    /// 셸의 `parsePairingLink`가 거절하는 것을 여기서도 거절한다.
    #[test]
    fn malformed_fragments_are_rejected() {
        assert!(decode_pairing_fragment("").is_none());
        assert!(decode_pairing_fragment("#").is_none());
        assert!(decode_pairing_fragment("#not base64url!").is_none());
        assert!(decode_pairing_fragment(&"A".repeat(MAX_PAIRING_FRAGMENT_CHARS + 1)).is_none());
        // 79바이트·81바이트는 길이로 떨어진다.
        assert!(decode_pairing_fragment(&base64url(&[0u8; 79])).is_none());
        assert!(decode_pairing_fragment(&base64url(&[0u8; 81])).is_none());
        assert!(decode_pairing_fragment(&base64url(&[0u8; 80])).is_some());
    }

    /// 재사용 가능한 자격증명은 링크에 없다 — 링크의 바이트는 정확히 세 재료뿐이다.
    #[test]
    fn the_fragment_carries_exactly_the_three_one_shot_materials() {
        let handle = AdmissionCredential::from_bytes([1; 32]);
        let id = PairingId::from_bytes([2; 16]);
        let link = encode_pairing_link("https://s.example", &handle, id, &secret(3));
        let (_, fragment) = link.split_once("/#").unwrap();
        let bytes = base64url_decode(fragment).unwrap();
        let mut expected = vec![1u8; 32];
        expected.extend([2u8; 16]);
        expected.extend([3u8; 32]);
        assert_eq!(bytes, expected);
    }

    #[test]
    fn the_shell_origin_is_policy_checked() {
        assert!(
            PRODUCTION_RELAY_SHELL_ORIGIN.is_none(),
            "BLOCKED — 배정되면 이 단언을 지운다"
        );
        // 개발 override는 디버그 빌드에서만, 그리고 https(또는 로컬 http)만.
        for (value, accepted) in [
            ("https://shell.example.test/", cfg!(debug_assertions)),
            ("http://localhost:8080", cfg!(debug_assertions)),
            ("http://127.0.0.1:10000", cfg!(debug_assertions)),
            ("http://shell.example.test", false),
            ("https://shell.example.test/#x", false),
            ("ftp://x", false),
            // 접두사 검사였다면 통과했을 것들.
            ("http://localhost.attacker.example", false),
            ("http://127.0.0.1.attacker.example", false),
            ("https://shell.example.test@attacker.example", false),
            ("https://shell.example.test/path", false),
            ("https://nodot", false),
            ("https://shell.example.test:0", false),
            ("https://-bad.example", false),
        ] {
            // SAFETY: 테스트 프로세스 안에서 순차 실행되며 다른 스레드가 이 변수를 읽지 않는다.
            unsafe { std::env::set_var("DEPPY_RELAY_DEV_SHELL_ORIGIN", value) };
            assert_eq!(shell_origin().is_some(), accepted, "{value}");
        }
        unsafe { std::env::remove_var("DEPPY_RELAY_DEV_SHELL_ORIGIN") };
        assert!(shell_origin().is_none());
    }
}
