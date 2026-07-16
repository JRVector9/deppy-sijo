//! unix epoch 시각 헬퍼 — 전 crate 공통 관례.
//!
//! 관례: 시계가 UNIX_EPOCH 이전(비정상)이면 0으로 폴백한다. 밀리초는 u64 상한으로
//! 클램프해 캐스팅 잘림이 없다.

use std::time::{SystemTime, UNIX_EPOCH};

/// 현재 unix epoch 초.
pub fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 현재 unix epoch 초 — SQLite TIMESTAMP 등 i64 컬럼용.
pub fn unix_secs_i64() -> i64 {
    unix_secs().min(i64::MAX as u64) as i64
}

/// 현재 unix epoch 밀리초.
pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 시각이_합리적인_범위다() {
        // 2023-11 이후 ~ 2096년 이전 — 시계가 정상일 때의 sanity 범위
        let s = unix_secs();
        assert!(s > 1_700_000_000 && s < 4_000_000_000, "{s}");
        assert!(unix_secs_i64() > 1_700_000_000);
        assert!(unix_ms() > 1_700_000_000_000);
    }
}
