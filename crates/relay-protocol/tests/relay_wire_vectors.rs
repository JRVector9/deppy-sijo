//! 체크인된 골든 와이어 벡터가 구현과 바이트 단위로 일치하는지 확인한다.
//!
//! 벡터는 이 구현과 **독립적으로 작성된** 인코더가 만들었다. 그래서 이 테스트가 고정하는
//! 것은 코드의 현재 동작이 아니라 명세다. 브라우저 셸도 같은 파일을 읽는다 — 그래서
//! 파서를 쓰지 않고 손으로 훑는다(이 크레이트는 의존성이 없어야 한다).

use relay_protocol::{
    ADMISSION_CREDENTIAL_BYTES, CONNECTION_ID_BYTES, ConnectionId, DecodeError, FrameType,
    HEADER_BYTES, MAGIC, MAX_CIPHERTEXT_BYTES, MAX_HELLO_BYTES, PROTOCOL_VERSION, ROUTE_ID_BYTES,
    RelayFrame, RouteId,
};

const FIXTURE: &str = include_str!("fixtures/relay-wire-v1.json");

#[test]
fn fixture_constants_match_the_compiled_contract() {
    assert_eq!(field(FIXTURE, "magic"), "DRLY");
    assert_eq!(MAGIC, *b"DRLY");
    assert_eq!(number(FIXTURE, "version"), PROTOCOL_VERSION as usize);
    assert_eq!(number(FIXTURE, "header_bytes"), HEADER_BYTES);
    assert_eq!(number(FIXTURE, "max_hello_bytes"), MAX_HELLO_BYTES);
    assert_eq!(
        number(FIXTURE, "max_ciphertext_bytes"),
        MAX_CIPHERTEXT_BYTES
    );
    assert_eq!(
        number(FIXTURE, "admission_credential_bytes"),
        ADMISSION_CREDENTIAL_BYTES
    );
}

#[test]
fn every_accepted_vector_encodes_and_decodes_byte_for_byte() {
    let vectors = objects(section(FIXTURE, "\"accept\""));
    assert_eq!(vectors.len(), 9, "the fixture must not silently shrink");

    for vector in vectors {
        let name = field(&vector, "name");
        let frame_type = frame_type(number(&vector, "frame_type") as u8);
        let route = RouteId::from_bytes(fixed::<ROUTE_ID_BYTES>(&field(&vector, "route_id")));
        let connection = ConnectionId::from_bytes(fixed::<CONNECTION_ID_BYTES>(&field(
            &vector,
            "connection_id",
        )));
        let sequence = sequence(&vector);
        let payload = unhex(&field(&vector, "payload"));
        let expected = unhex(&field(&vector, "frame"));

        let built = RelayFrame::new(frame_type, route, connection, sequence, &payload)
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(built.to_vec(), expected, "{name}: encoding drifted");

        let (decoded, consumed) =
            RelayFrame::decode(&expected).unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(consumed, expected.len(), "{name}");
        assert_eq!(decoded.frame_type(), frame_type, "{name}");
        assert_eq!(decoded.route_id(), route, "{name}");
        assert_eq!(decoded.connection_id(), connection, "{name}");
        assert_eq!(decoded.sequence(), sequence, "{name}");
        assert_eq!(decoded.payload(), payload.as_slice(), "{name}");
    }
}

#[test]
fn every_rejected_vector_fails_with_the_recorded_reason() {
    let vectors = objects(section(FIXTURE, "\"reject\""));
    assert_eq!(vectors.len(), 11, "the fixture must not silently shrink");

    for vector in vectors {
        let name = field(&vector, "name");
        let expected = field(&vector, "error");
        let bytes = unhex(&field(&vector, "frame"));
        let error = RelayFrame::decode(&bytes)
            .err()
            .unwrap_or_else(|| panic!("{name}: must not decode"));
        let actual = match error {
            DecodeError::BadMagic => "BadMagic",
            DecodeError::UnsupportedVersion(_) => "UnsupportedVersion",
            DecodeError::ReservedFlagsSet => "ReservedFlagsSet",
            DecodeError::UnknownFrameType(_) => "UnknownFrameType",
            DecodeError::PayloadTooLarge { .. } => "PayloadTooLarge",
            DecodeError::PayloadTooSmall { .. } => "PayloadTooSmall",
            DecodeError::Incomplete { .. } => "Incomplete",
        };
        assert_eq!(actual, expected, "{name}");
    }
}

/// 거절 벡터는 모두 헤더만 있는 52바이트다. 그런데도 `Incomplete`가 아니라 즉시 거절이
/// 나온다는 것이, 상한 검사가 버퍼링보다 먼저라는 증거다.
#[test]
fn oversized_declarations_are_rejected_from_the_header_alone() {
    for vector in objects(section(FIXTURE, "\"reject\"")) {
        let bytes = unhex(&field(&vector, "frame"));
        assert_eq!(bytes.len(), HEADER_BYTES, "{}", field(&vector, "name"));
    }
}

/// 픽스처에 어떤 키 재료도 들어 있지 않다는 것을 문서가 아니라 테스트로 고정한다.
#[test]
fn the_fixture_carries_no_key_material_vocabulary() {
    let lowercase = FIXTURE.to_lowercase();
    for forbidden in [
        "private",
        "secret",
        "scalar",
        "signature",
        "pkcs",
        "jwk",
        "\"d\":",
        "token",
    ] {
        assert!(!lowercase.contains(forbidden), "{forbidden}");
    }
}

fn frame_type(byte: u8) -> FrameType {
    for candidate in [
        FrameType::Hello,
        FrameType::Ciphertext,
        FrameType::Heartbeat,
        FrameType::Close,
        FrameType::DesktopAdmission,
        FrameType::DeviceAdmission,
        FrameType::TicketPublish,
        FrameType::TicketRevoke,
        FrameType::Admitted,
        FrameType::Rejected,
        FrameType::PeerJoined,
        FrameType::PeerLeft,
    ] {
        if candidate as u8 == byte {
            return candidate;
        }
    }
    panic!("fixture names an unknown frame type {byte:#04x}");
}

fn section<'a>(source: &'a str, key: &str) -> &'a str {
    let start = source.find(key).expect("fixture section missing");
    &source[start..]
}

/// `{ ... }` 블록들을 하나씩 떼어 낸다. 픽스처는 중첩 객체가 없는 평평한 구조라
/// 중괄호 깊이만 세면 충분하다.
fn objects(source: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut depth = 0usize;
    let mut current = String::new();
    for character in source.chars() {
        match character {
            '{' => {
                depth += 1;
                current.push(character);
            }
            '}' => {
                current.push(character);
                depth -= 1;
                if depth == 0 {
                    found.push(std::mem::take(&mut current));
                }
            }
            ']' if depth == 0 => break,
            _ if depth > 0 => current.push(character),
            _ => {}
        }
    }
    found
}

fn field(source: &str, key: &str) -> String {
    let needle = format!("\"{key}\":");
    let rest = &source[source.find(&needle).unwrap_or_else(|| panic!("{key}")) + needle.len()..];
    let start = rest.find('"').expect("string value") + 1;
    let end = rest[start..].find('"').expect("string end") + start;
    rest[start..end].to_owned()
}

fn number(source: &str, key: &str) -> usize {
    let needle = format!("\"{key}\":");
    let rest = &source[source.find(&needle).unwrap_or_else(|| panic!("{key}")) + needle.len()..];
    let digits: String = rest
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().expect("numeric value")
}

fn sequence(source: &str) -> u64 {
    let needle = "\"sequence\":";
    let rest = &source[source.find(needle).expect("sequence") + needle.len()..];
    let digits: String = rest
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().expect("numeric sequence")
}

fn unhex(value: &str) -> Vec<u8> {
    assert!(value.len().is_multiple_of(2), "hex must be byte aligned");
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = nibble(pair[0]);
            let low = nibble(pair[1]);
            (high << 4) | low
        })
        .collect()
}

fn fixed<const N: usize>(value: &str) -> [u8; N] {
    unhex(value).try_into().expect("fixed-width hex value")
}

fn nibble(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        b'A'..=b'F' => byte - b'A' + 10,
        _ => panic!("fixture is not hexadecimal"),
    }
}
