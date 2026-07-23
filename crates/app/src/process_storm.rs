//! 세션 자식 프로세스 폭주 판정 (docs/runaway-protection-roadmap.md B1).
//!
//! 정책: 자동 개입 없음 — 판정 결과는 경고 표출(B2)과 사용자 원클릭 대응(B3)의
//! 입력일 뿐이다. 트리거는 프로세스 수 단독 — CPU는 정상 빌드(cargo/webpack)가
//! 수백%를 수 분 유지해 오경보를 양산하므로 제외한다(2026-07-23 검토). 소수
//! 프로세스 메모리 누수형 폭주는 시스템 메모리 압박 감지(C1)가 커버한다.

/// 세션당 자손 프로세스 수 폭주 임계. 실제 사고는 5,417개(18배 초과)였고, 무거운
/// 정상 빌드도 순간 프로세스 수는 대체로 200 미만이라 여유가 있다.
pub const STORM_PROCESS_COUNT_THRESHOLD: usize = 300;

/// 확정/해소 지속 시간 — 순간 스파이크(빌드 초기 fork 버스트)를 걸러내고 배너
/// 깜빡임을 막는 히스테리시스. 2초 샘플 캐던스의 3틱.
pub const STORM_SUSTAIN_MS: u64 = 6_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StormEpisode {
    /// 에피소드 식별자 — 해소 후 재발생이면 새 id (B2 재알림 판단 근거).
    pub id: u64,
    /// 임계 초과를 처음 관측한 시각 (`SessionResourceUsage.sampled_at_ms` 기준 —
    /// 모니터의 시계를 그대로 써서 판정이 이벤트 재생만으로 결정적이다).
    pub over_since_ms: u64,
    /// STORM_SUSTAIN_MS 지속으로 확정됨 — 경고 표출은 확정 상태만 대상으로 한다.
    pub confirmed: bool,
    /// 확정 후 임계 미만을 처음 관측한 시각 — 해소 히스테리시스 진행 지점.
    pub under_since_ms: Option<u64>,
    pub peak_process_count: usize,
}

/// 한 세션의 새 샘플을 반영한 다음 상태.
///
/// 결측(캡처 실패) 샘플은 runtime이 마지막 발행값을 유지하므로 여기까지 오지
/// 않는다 — 상태가 신호 소실로 저절로 해소되는 일은 없고, 해소는 실측 미만
/// 샘플이 STORM_SUSTAIN_MS 이어질 때만 일어난다(2026-07-23 검토: 결측 ≠ 해소).
pub fn observe(
    prev: Option<StormEpisode>,
    process_count: usize,
    sampled_at_ms: u64,
    next_episode_id: &mut u64,
) -> Option<StormEpisode> {
    let over = process_count >= STORM_PROCESS_COUNT_THRESHOLD;
    match prev {
        None => over.then(|| {
            let id = *next_episode_id;
            *next_episode_id = next_episode_id.wrapping_add(1);
            StormEpisode {
                id,
                over_since_ms: sampled_at_ms,
                confirmed: false,
                under_since_ms: None,
                peak_process_count: process_count,
            }
        }),
        Some(mut episode) => {
            if over {
                episode.under_since_ms = None;
                episode.peak_process_count = episode.peak_process_count.max(process_count);
                if sampled_at_ms.saturating_sub(episode.over_since_ms) >= STORM_SUSTAIN_MS {
                    episode.confirmed = true;
                }
                Some(episode)
            } else if !episode.confirmed {
                // 확정 전 해소 = 순간 스파이크 — 에피소드 자체를 취소한다.
                None
            } else {
                let under_since = episode.under_since_ms.unwrap_or(sampled_at_ms);
                if sampled_at_ms.saturating_sub(under_since) >= STORM_SUSTAIN_MS {
                    None
                } else {
                    episode.under_since_ms = Some(under_since);
                    Some(episode)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OVER: usize = STORM_PROCESS_COUNT_THRESHOLD;
    const UNDER: usize = STORM_PROCESS_COUNT_THRESHOLD - 1;

    #[test]
    fn 순간_스파이크는_확정_전_해소되면_취소된다() {
        let mut next_id = 0;
        let ep = observe(None, OVER, 0, &mut next_id);
        assert!(ep.is_some_and(|e| !e.confirmed));
        assert_eq!(observe(ep, UNDER, 2_000, &mut next_id), None);
    }

    #[test]
    fn 지속_초과는_sustain_경과_후_확정된다() {
        let mut next_id = 0;
        let mut ep = observe(None, OVER, 0, &mut next_id);
        for at in [2_000, 4_000] {
            ep = observe(ep, OVER, at, &mut next_id);
            assert!(ep.is_some_and(|e| !e.confirmed), "at={at}");
        }
        ep = observe(ep, OVER, 6_000, &mut next_id);
        assert!(ep.is_some_and(|e| e.confirmed));
    }

    #[test]
    fn 확정_후_해소는_sustain_지속_미만일_때만_유지된다() {
        let mut next_id = 0;
        let mut ep = observe(None, OVER, 0, &mut next_id);
        ep = observe(ep, OVER, 6_000, &mut next_id);
        assert!(ep.is_some_and(|e| e.confirmed));
        // 미만 관측 시작 — 히스테리시스 창 안에서는 유지.
        ep = observe(ep, UNDER, 10_000, &mut next_id);
        assert!(ep.is_some_and(|e| e.confirmed && e.under_since_ms == Some(10_000)));
        ep = observe(ep, UNDER, 14_000, &mut next_id);
        assert!(ep.is_some());
        // 창 안에서 재초과하면 해소 진행이 리셋된다.
        ep = observe(ep, OVER, 15_000, &mut next_id);
        assert!(ep.is_some_and(|e| e.under_since_ms.is_none()));
        // 다시 미만이 sustain만큼 이어져야 해소.
        ep = observe(ep, UNDER, 16_000, &mut next_id);
        assert_eq!(observe(ep, UNDER, 22_000, &mut next_id), None);
    }

    #[test]
    fn 재발생은_새_에피소드_id를_받는다() {
        let mut next_id = 0;
        let first = observe(None, OVER, 0, &mut next_id).unwrap();
        // 스파이크 취소 후 재발생.
        assert_eq!(observe(Some(first), UNDER, 2_000, &mut next_id), None);
        let second = observe(None, OVER, 4_000, &mut next_id).unwrap();
        assert_ne!(first.id, second.id);
    }

    #[test]
    fn peak_process_count는_최대값을_추적한다() {
        let mut next_id = 0;
        let mut ep = observe(None, OVER, 0, &mut next_id);
        ep = observe(ep, 5_417, 2_000, &mut next_id);
        ep = observe(ep, OVER, 4_000, &mut next_id);
        assert!(ep.is_some_and(|e| e.peak_process_count == 5_417));
    }
}
