//! 프로세스 글로벌 할당자 (mimalloc) — 해제한 페이지를 OS로 **실제 반환**한다.
//!
//! ## 배경
//! scrollback 라인 압축(옵션 D)이 셀 배열 `Vec`를 drop해도, macOS 기본 malloc은
//! 해제 메모리를 zone free-list에 붙잡아 두고 OS에 반환하지 않아 phys_footprint
//! (활성 상태 보기 '메모리' 열)가 떨어지지 않았다. 특히 ~4.8KiB 같은 작은 셀 배열은
//! small zone 매거진에 남아 munmap되지 않는다. `malloc_zone_pressure_relief()`도
//! 0바이트만 반환했다.
//!
//! mimalloc은 purge 시 macOS에서 `MADV_FREE_REUSABLE`로 decommit하며, 이는 **즉시**
//! rss 회계에 반영된다(mimalloc v3 `src/prim/unix/prim.c` `_mi_prim_decommit`,
//! upstream issue #1097). 기본값 `purge_decommits=1`이라 purge=decommit이다.
//!
//! ## 크로스플랫폼 (결정적 제약)
//! 앱은 macOS + Windows/MSVC를 모두 타깃한다. jemalloc(tikv-jemallocator)은
//! Windows/MSVC를 지원하지 않으므로 배제했다. mimalloc은 두 플랫폼 모두 지원한다.
//!
//! ## 설정 정책 — 보수적 + 명시적 purge 필수
//! mimalloc 기본값을 그대로 쓴다(`purge_delay=10ms`, `purge_decommits=1`,
//! `arena_purge_mult=10`). **중요(실측)**: `purge_delay` 기반 자동 백그라운드 반환은
//! idle 프리(더는 alloc/free 활동이 없는 해제)에는 신뢰할 수 없다 — 200MB 해제 후
//! 500ms busy-wait + 소량 alloc churn에도 반환 0MB였다(`scrollback_rss_end_to_end`의
//! `mimalloc은_명시_purge_없이도_자동_반환하는가` 진단). 따라서 스크롤백을 해제하는
//! 순간(hidden/exited 전환·아카이브·압박 격상)마다 [`purge`]로 `mi_collect(true)`를
//! 명시 호출해 지연을 무시하고 강제 decommit한다 — 이게 실제 반환 경로다.
//! 자동 반환을 더 공격적으로 하려면 프로세스 시작 전 환경변수
//! `MIMALLOC_PURGE_DELAY=0`을 설정하면 된다(코드에 하드코딩하지 않는다 —
//! purge_delay의 enum 인덱스가 mimalloc v2/v3 간 다르고 libmimalloc-sys 0.1.49의
//! 바인딩은 해당 상수를 이름으로 노출하지 않아, 매직넘버는 버전 취약하다).
//!
//! ## 되돌리기
//! 이 모듈 제거 + `main.rs`의 `mod alloc;` + `app.rs`의 `crate::alloc::purge()`
//! 호출 + Cargo.toml의 mimalloc/libmimalloc-sys 의존성만 지우면 시스템 할당자로
//! 완전히 복귀한다. 앱 로직은 이 모듈에 의존하지 않는다.

// `bench-alloc` feature는 bench.rs가 counting `GlobalAlloc`(System 위임)을 설치한다.
// `#[global_allocator]`는 프로세스당 하나만 허용되므로 이 둘은 상호배타 —
// 여기서는 `not(bench-alloc)`으로만 mimalloc을 심는다.
#[cfg(not(feature = "bench-alloc"))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// 해제되어 mimalloc이 보유 중인(purge 예약된) 페이지를 지연 없이 OS로 반환한다.
///
/// `mi_collect(true)`는 스레드 로컬 heap의 지연 free를 수거하고 빈 페이지를 arena로
/// 되돌린 뒤, arena의 purge를 **강제**한다(`purge_delay` 만료 여부 무시). macOS에서
/// decommit은 `MADV_FREE_REUSABLE`이라 phys_footprint가 즉시 하락한다.
///
/// 비용: 전체 arena를 훑어 decommit하므로 heap 크기에 비례한다(수십~수백 µs 수준).
/// 매 프레임이 아니라 드문 이벤트(메모리 압박 격상)에서만 호출해야 한다.
///
/// `bench-alloc` 빌드에서는 mimalloc이 설치되지 않으므로 no-op이다.
pub fn purge() {
    #[cfg(not(feature = "bench-alloc"))]
    // SAFETY: `mi_collect`은 스레드 안전한 순수 회수 호출이며, 인자는 강제 여부 bool뿐이다.
    unsafe {
        libmimalloc_sys::mi_collect(true);
    }
}

// 합성 phys_footprint 증명 — macOS 전용, 기본 실행에서 제외(#[ignore]).
// 실행: cargo test -p deppy-sijo --release --ignored --nocapture alloc
//
// bench-alloc 빌드에서는 mimalloc이 아니라 System(counting)이 글로벌 할당자라
// 반환 assert가 성립하지 않으므로 test 전체를 not(bench-alloc)으로 가둔다.
#[cfg(all(test, target_os = "macos", not(feature = "bench-alloc")))]
mod tests {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::hint::black_box;

    const MB: u64 = 1024 * 1024;
    /// 원 이슈의 셀 배열과 유사한 작은 청크(small zone 대상). 과제 명세 ~4832 바이트.
    const CHUNK: usize = 4832;
    const TARGET_BYTES: usize = 200 * 1024 * 1024;
    const COUNT: usize = TARGET_BYTES / CHUNK;

    /// macOS phys_footprint(활성 상태 보기 '메모리'). resource_monitor.rs의
    /// `phys_footprint_for_pid`와 동일한 FFI 패턴(proc_pid_rusage RUSAGE_INFO_V4).
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

    /// mimalloc(글로벌 할당자): 작은 청크로 ~200MB 할당 → 전부 drop → 강제 purge →
    /// phys_footprint가 실질적으로 하락(베이스라인 근처 복귀)함을 증명한다.
    #[test]
    #[ignore = "합성 phys_footprint 측정 — macOS에서 --release --ignored로만 실행"]
    fn mimalloc_purge가_해제_페이지를_os로_반환한다() {
        // 할당자·측정 경로를 먼저 데운다(첫 syscall/TLS 초기화 alloc 제외).
        super::purge();
        let baseline = phys_footprint();

        let mut chunks: Vec<Vec<u8>> = Vec::with_capacity(COUNT);
        for i in 0..COUNT {
            // vec![k; CHUNK]은 memset으로 전 페이지를 touch → 실제 커밋된다.
            chunks.push(vec![(i & 0xff) as u8; CHUNK]);
        }
        // 최적화로 사라지지 않도록 실제로 읽는다.
        let mut acc: u64 = 0;
        for c in &chunks {
            acc = acc
                .wrapping_add(c[0] as u64)
                .wrapping_add(c[CHUNK - 1] as u64);
        }
        black_box(acc);
        let peak = phys_footprint();

        // 전부 해제.
        chunks.clear();
        chunks.shrink_to_fit();
        drop(chunks);

        // 강제 purge(mi_collect(true)) — 지연 없이 decommit.
        super::purge();
        let after = phys_footprint();

        let allocated = (COUNT * CHUNK) as u64;
        let released = peak.saturating_sub(after);
        let retained_over_baseline = after.saturating_sub(baseline);
        eprintln!(
            "[mimalloc] baseline={:.1}MB peak={:.1}MB after={:.1}MB | allocated={:.1}MB \
             released={:.1}MB retained_over_baseline={:.1}MB",
            mb(baseline),
            mb(peak),
            mb(after),
            mb(allocated),
            mb(released),
            mb(retained_over_baseline),
        );

        // peak는 최소한 할당량의 대부분만큼 올라야 한다(측정 위생 점검).
        assert!(
            peak.saturating_sub(baseline) >= 150 * MB,
            "peak가 충분히 오르지 않음 — 측정 무효"
        );
        // 핵심 증명: 해제+purge 후 할당량의 대부분이 OS로 반환되어야 한다.
        assert!(
            released >= 150 * MB,
            "mimalloc이 페이지를 OS로 반환하지 않음: released={:.1}MB",
            mb(released),
        );
    }

    /// 문서화된 베이스라인: **시스템 할당자**로 같은 작은 청크를 할당/해제하면
    /// phys_footprint가 떨어지지 않는다(원 문제 재현). override 미사용이라
    /// `System`은 libc malloc으로 직행 — 글로벌 mimalloc과 독립적이다.
    #[test]
    #[ignore = "합성 baseline — 시스템 할당자는 작은 청크를 OS로 반환하지 않음"]
    fn system_할당자는_작은_청크를_os로_반환하지_않는다_베이스라인() {
        super::purge();
        let baseline = phys_footprint();
        let layout = Layout::from_size_align(CHUNK, 8).unwrap();

        let mut ptrs: Vec<*mut u8> = Vec::with_capacity(COUNT);
        for i in 0..COUNT {
            // SAFETY: layout은 유효(비제로 크기), 반환 포인터는 즉시 touch 후 저장.
            let p = unsafe { System.alloc(layout) };
            assert!(!p.is_null(), "System.alloc 실패");
            unsafe { std::ptr::write_bytes(p, (i & 0xff) as u8, CHUNK) };
            ptrs.push(p);
        }
        let peak = phys_footprint();

        // 전부 해제 — 시스템 malloc은 작은 청크를 zone free-list에 붙잡는다.
        for &p in &ptrs {
            // SAFETY: 위에서 같은 layout으로 할당한 포인터를 정확히 한 번 해제.
            unsafe { System.dealloc(p, layout) };
        }
        let after = phys_footprint();

        let released = peak.saturating_sub(after);
        let retained_over_baseline = after.saturating_sub(baseline);
        eprintln!(
            "[system]   baseline={:.1}MB peak={:.1}MB after={:.1}MB | \
             released={:.1}MB retained_over_baseline={:.1}MB",
            mb(baseline),
            mb(peak),
            mb(after),
            mb(released),
            mb(retained_over_baseline),
        );

        // 측정 위생: peak가 실제로 올랐어야 한다.
        assert!(
            peak.saturating_sub(baseline) >= 150 * MB,
            "peak가 충분히 오르지 않음 — 측정 무효"
        );
        // 베이스라인 성질: 시스템 할당자는 4.8KiB 청크를 zone 매거진에 붙잡아
        // 해제 후에도 수십 MB를 반환하지 않는다(이것이 mimalloc 도입 동기다).
        // 실측(2026-07, macOS): 200MB 중 ~95MB를 계속 보유 — mimalloc의 ~0.5MB와
        // 극명히 대비된다. 정확한 보유량은 OS 버전에 따라 변하므로 임계는 보수적으로
        // 잡되, mimalloc(≈0 보유)과 구조적으로 다름을 증명하는 수준(수십 MB)이면 된다.
        assert!(
            retained_over_baseline >= 40 * MB,
            "예상과 달리 시스템 할당자가 대부분 반환함: retained={:.1}MB (측정/환경 재확인 필요)",
            mb(retained_over_baseline),
        );
    }
}
