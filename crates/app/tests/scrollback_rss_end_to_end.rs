//! 전체 스택 phys_footprint 증명 (macOS, `--release`, `#[ignore]`).
//!
//! 옵션 D(스크롤백 라인 압축)와 mimalloc 글로벌 할당자를 **함께** 검증한다:
//! 무거운 스크롤백 세션들을 만든 뒤 Hidden으로 전이(히스토리 전체 압축)하고 mimalloc을
//! 강제 purge하면, 압축이 해제한 셀 배열 페이지가 실제로 OS에 반환돼 phys_footprint
//! (활성 상태 보기 '메모리')가 떨어짐을 실측한다. 오케스트레이터가 두 서브에이전트
//! 작업(옵션 2 할당자 + 옵션 3 예산 위에 놓인 옵션 D 압축)의 결합 효과를 확인하는
//! 최종 검증이다.
//!
//! 실행:
//! ```sh
//! cargo test -p deppy-sijo --release --test scrollback_rss_end_to_end \
//!   -- --ignored --nocapture
//! ```
//!
//! 별도 통합 테스트 크레이트라 앱과 **동일한** mimalloc을 글로벌 할당자로 선언한다
//! (bin의 file_tree.rs release 컴파일 이슈를 우회 — alloc_phys_footprint_release.rs 참고).

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(target_os = "macos")]
mod macos {
    use terminal::{AlacrittyBackend, TerminalBackend, TerminalCacheClass};

    const MB: u64 = 1024 * 1024;

    fn phys_footprint() -> u64 {
        let mut info = std::mem::MaybeUninit::<libc::rusage_info_v4>::uninit();
        let rc = unsafe {
            libc::proc_pid_rusage(
                std::process::id() as libc::c_int,
                libc::RUSAGE_INFO_V4,
                info.as_mut_ptr().cast(),
            )
        };
        assert_eq!(rc, 0, "proc_pid_rusage 실패");
        // SAFETY: rc == 0이면 커널이 요청한 flavor 구조체 전체를 채웠다.
        unsafe { info.assume_init() }.ri_phys_footprint
    }

    fn mb(bytes: u64) -> f64 {
        bytes as f64 / MB as f64
    }

    /// 현실적 로그류 한 줄(줄마다 다른 내용 + 가끔 SGR 색).
    fn log_line(i: u32) -> String {
        format!(
            "\x1b[32m2026-07-24T12:00:{:02}\x1b[0m INFO worker[{}] req={} status=200 \
             path=/api/v1/items/{} latency={}ms bytes={}\r\n",
            i % 60,
            i % 8,
            i,
            (i * 7) % 100_000,
            i % 500,
            (i * 131) % 65_536,
        )
    }

    /// 무거운 스크롤백 세션 여러 개 → Hidden 전이(전체 압축) → mimalloc purge →
    /// phys_footprint가 실질적으로 하락함을 증명한다.
    #[test]
    #[ignore = "전체 스택 phys_footprint 측정 — macOS에서 --release --ignored로만 실행"]
    fn 압축된_스크롤백은_purge후_os로_반환된다() {
        // 할당자·측정 경로 워밍업.
        unsafe { libmimalloc_sys::mi_collect(true) };
        let baseline = phys_footprint();

        // 200열 세션 8개에 각 20,000줄을 먹인다(옵션 3 예산으로 세션당 최대 10,000줄
        // 유지 가능). feed가 HOT(256) 밖을 점진 압축하므로, 이 시점 상주 메모리에는
        // 피딩 churn이 해제했지만 mimalloc이 아직 보유 중인 페이지가 섞여 있다.
        const SESSIONS: usize = 8;
        const LINES: u32 = 20_000;
        let mut backends: Vec<AlacrittyBackend> = Vec::with_capacity(SESSIONS);
        for _ in 0..SESSIONS {
            let mut b = AlacrittyBackend::new(200, 40, 10_000);
            for i in 0..LINES {
                let _ = b.feed(log_line(i).as_bytes());
            }
            backends.push(b);
        }
        let peak = phys_footprint();

        // 모든 세션을 Hidden으로 → HOT 창까지 히스토리 전체 압축(셀 배열 해제).
        for b in &mut backends {
            b.set_cache_class(TerminalCacheClass::Hidden);
        }
        let after_hidden = phys_footprint();

        // mimalloc 강제 purge — 해제 페이지를 지연 없이 OS로 반환.
        unsafe { libmimalloc_sys::mi_collect(true) };
        let after_purge = phys_footprint();

        // 세션은 여전히 살아있다(스크롤백을 압축된 채 보유) — drop 아님.
        std::hint::black_box(&backends);

        let freed_by_purge = after_hidden.saturating_sub(after_purge);
        let total_drop = peak.saturating_sub(after_purge);
        eprintln!(
            "[e2e] baseline={:.1}MB  peak(8세션×2만줄)={:.1}MB  \
             after_hidden(전체압축)={:.1}MB  after_purge={:.1}MB\n\
             → purge가 반환={:.1}MB, 총 하락(peak−after_purge)={:.1}MB, \
             세션 유지분(after_purge−baseline)={:.1}MB",
            mb(baseline),
            mb(peak),
            mb(after_hidden),
            mb(after_purge),
            mb(freed_by_purge),
            mb(total_drop),
            mb(after_purge.saturating_sub(baseline)),
        );

        // 측정 위생: 8세션 피딩으로 상주가 충분히 올랐어야 한다.
        assert!(
            peak.saturating_sub(baseline) >= 30 * MB,
            "peak가 충분히 오르지 않음 — 측정 무효 (peak-baseline={:.1}MB)",
            mb(peak.saturating_sub(baseline)),
        );
        // 핵심 증명: 압축 + purge 후 상주가 peak 대비 크게 내려간다(해제 페이지가 OS로
        // 반환됨). 압축된 스크롤백은 원시 대비 수십 배 작으므로, 유지분은 peak의 일부에
        // 그쳐야 한다.
        assert!(
            after_purge < peak.saturating_sub(20 * MB),
            "압축+purge 후에도 상주가 안 떨어짐: peak={:.1}MB after_purge={:.1}MB",
            mb(peak),
            mb(after_purge),
        );
    }
}
