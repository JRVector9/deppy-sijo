//! 고엔트로피 인증 토큰 생성 — remote attach 토큰·web 페어링 토큰 공통 관례.

/// 32바이트 랜덤(uuid v4 ×2 ≈ 244bit 엔트로피) hex 64자 토큰.
/// hex라 URL/QR에 인코딩 없이 실을 수 있다.
pub fn random_hex_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn 토큰은_hex_64자이고_매번_다르다() {
        let a = super::random_hex_token();
        let b = super::random_hex_token();
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
