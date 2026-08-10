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

/// unix 초 → "YYYY-MM-DD". Howard Hinnant의 `civil_from_days` 알고리즘
/// (그레고리력 상시 성립, 윤년 포함) — 순수 함수라 시각과 무관하게 테스트할 수 있다.
/// 시간대는 모른다: `secs`를 그대로 UTC로 읽으므로, 로컬 날짜가 필요하면 호출측이
/// 로컬 오프셋을 더한 값을 넘겨야 한다(status_feed.rs의 `civil_date`와 같은 계산이며,
/// 이 워크스페이스는 chrono를 쓰지 않는 관례라 여기 공용으로 둔다).
pub fn civil_date(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02}")
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

    /// 알려진 기준일들로 civil_date를 고정한다 — epoch 경계, 윤년(2024, 2000),
    /// 100으로 나눠지지만 400으로는 안 나눠지는 평년(1900) 경계까지 확인한다.
    #[test]
    fn civil_date가_알려진_날짜와_일치한다() {
        assert_eq!(civil_date(0), "1970-01-01");
        assert_eq!(civil_date(86_400), "1970-01-02");
        assert_eq!(civil_date(-1), "1969-12-31");
        assert_eq!(civil_date(1_786_320_000), "2026-08-10");
        assert_eq!(civil_date(1_709_208_000), "2024-02-29");
        assert_eq!(civil_date(951_868_800), "2000-03-01");
        assert_eq!(civil_date(-2_203_891_200), "1900-03-01");
    }
}
