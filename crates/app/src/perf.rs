//! PR-21 성능 계측/하네스. 환경변수로만 켜진다 — 평소 실행엔 비용 없음.
//!
//! - `DEPPY_FRAME_STATS=1`: 5초마다 프레임 수와 frame time p95를 로그로 남긴다
//!   (완료 기준 "idle repaint 0회", "frame time p95 16ms 이하" 실측용)
//! - `DEPPY_PERF_HARNESS=1`: 시작 시 hidden 부하 시나리오를 자동 구성한다
//!   (셸 10개 = tab 10개, 그중 hidden 3개가 대량 출력)

use std::time::{Duration, Instant};

pub struct FrameStats {
    enabled: bool,
    window_start: Instant,
    /// 이번 윈도(5s)의 프레임 **소요 시간**(ui 함수 진입~종료) ms —
    /// 완료 기준 "frame time p95"는 렌더 소요지 프레임 간격이 아니다
    frame_ms: Vec<f32>,
    frame_begin: Option<Instant>,
}

impl FrameStats {
    pub fn new() -> Self {
        Self {
            enabled: std::env::var_os("DEPPY_FRAME_STATS").is_some(),
            window_start: Instant::now(),
            frame_ms: Vec::new(),
            frame_begin: None,
        }
    }

    /// 프레임 시작 (ui() 진입).
    pub fn begin(&mut self) {
        if self.enabled {
            self.frame_begin = Some(Instant::now());
        }
    }

    /// 프레임 끝 (ui() 종료 직전). 5초 윈도마다 프레임 수/p95 소요를 로그.
    /// idle이면 프레임 자체가 없어 로그가 나오지 않는다 — 로그 부재 + CPU로
    /// "idle repaint 0회"를 확인한다.
    pub fn end(&mut self) {
        if !self.enabled {
            return;
        }
        let Some(begin) = self.frame_begin.take() else {
            return;
        };
        let now = Instant::now();
        self.frame_ms.push((now - begin).as_secs_f32() * 1000.0);
        if now - self.window_start >= Duration::from_secs(5) {
            let frames = self.frame_ms.len();
            let p95 = percentile95(&mut self.frame_ms);
            tracing::info!(frames, p95_ms = p95, "frame stats (5s window)");
            self.frame_ms.clear();
            self.window_start = now;
        }
    }
}

fn percentile95(samples: &mut [f32]) -> f32 {
    percentile(samples, 0.95)
}

/// q분위(0.0~1.0). `q = 1.0`이면 최댓값. 샘플이 없으면 0.0.
/// 벤치(B1)의 p50/p95/p99/max가 FrameStats와 **같은 정의**를 쓰도록 여기 둔다.
pub fn percentile(samples: &mut [f32], q: f32) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((samples.len() as f32 * q).ceil() as usize).saturating_sub(1);
    samples[idx.min(samples.len() - 1)]
}

/// 부하 하네스 시나리오: 완료 기준 "hidden session 10개 + 그중 3개 대량 출력".
/// 세션 11개 = tab 11개 — 마지막 tab만 active이므로 **hidden이 정확히 10개**,
/// 대량 출력 3개는 hidden에 배치된다 (codex 리뷰: 10개 스폰이면 hidden 9개였다).
pub const HARNESS_SESSIONS: usize = 11;

pub fn harness_enabled() -> bool {
    std::env::var_os("DEPPY_PERF_HARNESS").is_some()
}

/// n번째 하네스 세션이 실행할 명령. 0..3은 대량 출력, 나머지는 idle 셸.
/// 대량 출력 = 1700B 라인 x 100회/초 ≈ 170KB/s ≈ 10MB/min
/// (codex 리뷰: 짧은 라인으로는 ~13KB/s에 그쳤다).
pub fn harness_command(index: usize) -> (String, Vec<String>) {
    if index < 3 {
        (
            "/bin/sh".to_owned(),
            vec![
                "-c".to_owned(),
                "line=$(printf 'x%.0s' $(seq 1 1700)); i=0; \
                 while :; do printf '%06d %s\\n' $i \"$line\"; \
                 i=$((i+1)); [ $((i % 100)) -eq 0 ] && sleep 1; done"
                    .to_owned(),
            ],
        )
    } else {
        ("/bin/sh".to_owned(), vec!["-i".to_owned()])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn p95_계산() {
        let mut empty: Vec<f32> = vec![];
        assert_eq!(percentile95(&mut empty), 0.0);
        let mut one = vec![7.0];
        assert_eq!(percentile95(&mut one), 7.0);
        // 1..=100에서 p95는 95
        let mut hundred: Vec<f32> = (1..=100).map(|v| v as f32).collect();
        assert_eq!(percentile95(&mut hundred), 95.0);
    }

    #[test]
    fn percentile_분위() {
        let mut hundred: Vec<f32> = (1..=100).map(|v| v as f32).collect();
        assert_eq!(percentile(&mut hundred, 0.50), 50.0);
        assert_eq!(percentile(&mut hundred, 0.95), 95.0);
        assert_eq!(percentile(&mut hundred, 0.99), 99.0);
        // q=1.0은 최댓값 (인덱스 오버플로 없이)
        assert_eq!(percentile(&mut hundred, 1.0), 100.0);
        let mut empty: Vec<f32> = vec![];
        assert_eq!(percentile(&mut empty, 0.5), 0.0);
    }

    #[test]
    fn 하네스_명령_구성() {
        let (_, args) = harness_command(0);
        let script = args.join(" ");
        assert!(script.contains("while"));
        assert!(script.contains("seq 1 1700")); // ~170KB/s 페이스의 라인 길이
        assert_eq!(harness_command(5).1, vec!["-i".to_owned()]);
        assert_eq!(HARNESS_SESSIONS, 11); // active 1 + hidden 10
    }
}
