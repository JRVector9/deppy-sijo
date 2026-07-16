//! 비밀 재료(keyring 저장 키·토큰)의 hex 인코딩 — runtime/web-remote/audit 공통.

use anyhow::Context;

/// 소문자 hex 인코딩.
pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// hex 디코딩. 바이트 기반 파싱 — keyring 값이 손상돼 비ASCII가 섞여 있어도
/// str 슬라이스 경계 패닉 없이 Err로 보고한다.
pub fn from_hex(s: &str) -> anyhow::Result<Vec<u8>> {
    let bytes = s.as_bytes();
    anyhow::ensure!(bytes.len().is_multiple_of(2), "hex 길이가 홀수");
    bytes
        .chunks_exact(2)
        .map(|pair| {
            let hi = hex_digit(pair[0])?;
            let lo = hex_digit(pair[1])?;
            Ok(hi << 4 | lo)
        })
        .collect()
}

fn hex_digit(byte: u8) -> anyhow::Result<u8> {
    (byte as char)
        .to_digit(16)
        .map(|d| d as u8)
        .with_context(|| format!("hex가 아닌 바이트: 0x{byte:02x}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 왕복_인코딩이_원본을_보존한다() {
        let data: Vec<u8> = (0..=255).collect();
        assert_eq!(from_hex(&to_hex(&data)).unwrap(), data);
    }

    #[test]
    fn 홀수_길이는_에러다() {
        assert!(from_hex("abc").is_err());
    }

    #[test]
    fn 비hex_문자는_에러다() {
        assert!(from_hex("zz").is_err());
    }

    #[test]
    fn 비ascii_손상_값도_패닉_없이_에러다() {
        assert!(from_hex("한글").is_err());
    }
}
