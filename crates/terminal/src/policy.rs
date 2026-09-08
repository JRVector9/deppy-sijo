//! 앱 설정과 runtime이 공유하는 터미널 보관 정책. OS 조회와 렌더러 상태를 소유하지 않는다.

/// 설정 화면·파일의 최소값. 기존 wire spawn의 0(보관 안 함)은 별도로 허용한다.
pub const SCROLLBACK_SETTING_MIN: u32 = 100;
pub const SCROLLBACK_LINES_MAX: usize = 100_000;
pub const SCROLLBACK_DEFAULT: u32 = 10_000;
pub const CACHE_BUDGET_MANUAL_MIN_MIB: u32 = 32;
pub const CACHE_BUDGET_MANUAL_MAX_MIB: u32 = 2_048;
pub const CACHE_BUDGET_FALLBACK_MIB: u32 = 128;

/// 누락된 모드는 기존 수동 예산을 보존한다. 신규 설정은 앱에서 Auto를 명시한다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheBudgetMode {
    Auto,
    #[default]
    Manual,
}

/// 물리 RAM의 1/128을 사용한다. 상한 적용 후 변환하여 큰 입력에서도 잘리지 않는다.
/// RAM 조회 실패 시 기존 기본 예산을 유지하고, 가용 메모리의 순간 변화는 사용하지 않는다.
pub fn auto_cache_budget_mib(ram_bytes: Option<u64>) -> u32 {
    ram_bytes.map_or(CACHE_BUDGET_FALLBACK_MIB, |bytes| {
        (bytes / 128 / (1024 * 1024)).clamp(128, 512) as u32
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 자동_예산은_ram비율과_상하한을_지킨다() {
        const GIB: u64 = 1024 * 1024 * 1024;
        assert_eq!(auto_cache_budget_mib(None), 128);
        assert_eq!(auto_cache_budget_mib(Some(0)), 128);
        assert_eq!(auto_cache_budget_mib(Some(8 * GIB)), 128);
        assert_eq!(auto_cache_budget_mib(Some(16 * GIB)), 128);
        assert_eq!(auto_cache_budget_mib(Some(32 * GIB)), 256);
        assert_eq!(auto_cache_budget_mib(Some(64 * GIB)), 512);
        assert_eq!(auto_cache_budget_mib(Some(u64::MAX)), 512);
    }
}
