//! `--release` 합성 phys_footprint 증명 (macOS, #[ignore]).
//!
//! 실행:
//! ```sh
//! cargo test -p deppy-sijo --release --test alloc_phys_footprint_release \
//!   -- --ignored --nocapture
//! ```
//!
//! ## 왜 통합 테스트인가
//! deppy-sijo bin의 unittest 타깃(`src/main.rs`)은 `--release`에서 컴파일되지
//! 않는다 — `src/ui/file_tree.rs`의 한 테스트가 egui `Style::debug` 필드를
//! `#[cfg(debug_assertions)]` 가드 없이 접근하는데, 그 필드는 egui 쪽에서
//! `debug_assertions`로 게이트돼 release egui에는 없기 때문이다(이 할당자 작업과
//! 무관한 기존 이슈). alloc 모듈의 unit test는 그래서 debug로만 돌릴 수 있다.
//! 이 통합 테스트는 별도 크레이트라 file_tree.rs를 끌어오지 않아 release에서 그대로
//! 빌드되며, 앱이 설치하는 것과 **동일한** `mimalloc::MiMalloc`를 글로벌 할당자로
//! 선언해 release 조건의 반환 수치를 확보한다.

// 앱(crates/app/src/alloc.rs)이 설치하는 것과 같은 타입. 통합 테스트는 별도 바이너리라
// 자체 #[global_allocator] 선언이 필요하다(앱 main.rs의 선언과 충돌하지 않는다).
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(target_os = "macos")]
mod macos {
    const MB: u64 = 1024 * 1024;
    const CHUNK: usize = 4832;
    const TARGET_BYTES: usize = 200 * 1024 * 1024;
    const COUNT: usize = TARGET_BYTES / CHUNK;

    /// resource_monitor.rs의 `phys_footprint_for_pid`와 동일한 FFI 패턴.
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

    /// release 빌드에서 mimalloc이 해제 페이지를 OS로 반환함을 증명한다.
    #[test]
    #[ignore = "합성 phys_footprint 측정 — macOS에서 --release --ignored로만 실행"]
    fn mimalloc_release_returns_pages_to_os() {
        // SAFETY: 강제 회수 호출, 인자 없음.
        unsafe { libmimalloc_sys::mi_collect(true) };
        let baseline = phys_footprint();

        let mut chunks: Vec<Vec<u8>> = Vec::with_capacity(COUNT);
        for i in 0..COUNT {
            chunks.push(vec![(i & 0xff) as u8; CHUNK]);
        }
        let mut acc: u64 = 0;
        for c in &chunks {
            acc = acc
                .wrapping_add(c[0] as u64)
                .wrapping_add(c[CHUNK - 1] as u64);
        }
        std::hint::black_box(acc);
        let peak = phys_footprint();

        chunks.clear();
        chunks.shrink_to_fit();
        drop(chunks);

        // 강제 purge — 지연 없이 decommit(macOS: MADV_FREE_REUSABLE → 즉시 rss 반영).
        // SAFETY: 위와 동일.
        unsafe { libmimalloc_sys::mi_collect(true) };
        let after = phys_footprint();

        let allocated = (COUNT * CHUNK) as u64;
        let released = peak.saturating_sub(after);
        let retained = after.saturating_sub(baseline);
        eprintln!(
            "[mimalloc/release] baseline={:.1}MB peak={:.1}MB after={:.1}MB | \
             allocated={:.1}MB released={:.1}MB retained_over_baseline={:.1}MB",
            mb(baseline),
            mb(peak),
            mb(after),
            mb(allocated),
            mb(released),
            mb(retained),
        );

        assert!(
            peak.saturating_sub(baseline) >= 150 * MB,
            "peak가 충분히 오르지 않음 — 측정 무효"
        );
        assert!(
            released >= 150 * MB,
            "mimalloc이 페이지를 OS로 반환하지 않음: released={:.1}MB",
            mb(released),
        );
    }
}
