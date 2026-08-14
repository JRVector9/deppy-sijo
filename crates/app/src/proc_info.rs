//! 네이티브 프로세스 정보 조회 — `ps`/`lsof` exec 대신 커널 syscall(`proc_pidinfo`)을
//! in-process로 직접 호출한다.
//!
//! ## 배경 (2026-08-14)
//! `agent_detect`가 에이전트 감지를 위해 `ps -axo pid=,ppid=,command=`와 `lsof`를 분당 72회
//! exec한다. macOS는 exec마다 코드서명을 검증하므로 syspolicyd 부하로 이어진다. 이 모듈은 그
//! 중 두 가지 조회(생존+동일성 확인, cwd 조회)를 서브프로세스 없이 커널에서 직접 읽어
//! exec 비용을 없앤다. `port_inventory.rs`의 `query_process_birth_with_operation`,
//! `bench.rs`의 `own_task_info`가 같은 `proc_pidinfo` 계열 FFI를 이미 쓰고 있어 그 스타일을
//! 따른다.

/// 프로세스 시작 시각(초 단위 epoch 등 단조 비교 가능한 값).
/// pid만으로는 재사용을 구분할 수 없으므로 (pid, start_time) 쌍으로 동일성을 판정한다.
/// 프로세스가 없거나 조회 실패면 None.
#[allow(dead_code)] // 생존/동일성 확인 소비자가 병렬 작업으로 붙는다.
#[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
pub(crate) fn pid_start_time(pid: u32) -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        pid_start_time_macos(pid)
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// 프로세스의 현재 작업 디렉터리. 없거나 조회 실패면 None.
#[allow(dead_code)] // cwd 조회 소비자가 병렬 작업으로 붙는다.
#[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
pub(crate) fn pid_cwd(pid: u32) -> Option<std::path::PathBuf> {
    #[cfg(target_os = "macos")]
    {
        pid_cwd_macos(pid)
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

#[cfg(target_os = "macos")]
fn pid_start_time_macos(pid: u32) -> Option<u64> {
    let pid_i32 = i32::try_from(pid).ok()?;
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let size = i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;
    // SAFETY: `info`는 `size` 바이트짜리 쓰기 가능한 `proc_bsdinfo` 버퍼다. pid와 flavor는
    // 고정 상수/입력 검증을 거쳤고, 반환값이 정확히 `size`일 때만 아래에서 읽는다.
    let written = unsafe {
        libc::proc_pidinfo(
            pid_i32,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    // 부분 기록(예: 시그니처 불일치·권한 부족으로 커널이 일부만 채운 경우)을 그대로
    // 읽으면 UB이므로, 요청한 크기와 정확히 같을 때만 초기화된 것으로 취급한다.
    if written != size {
        return None;
    }
    // SAFETY: 위에서 `written == size`를 확인했으므로 구조체 전체가 커널에 의해 채워졌다.
    let info = unsafe { info.assume_init() };
    // pid 재확인 + 시작 시각이 0(비정상 값)이 아님을 검증한다 — port_inventory.rs의
    // query_process_birth_with_operation과 같은 방어적 점검.
    if info.pbi_pid != pid || info.pbi_start_tvsec == 0 {
        return None;
    }
    Some(info.pbi_start_tvsec)
}

#[cfg(target_os = "macos")]
fn pid_cwd_macos(pid: u32) -> Option<std::path::PathBuf> {
    use std::os::unix::ffi::OsStrExt;

    let pid_i32 = i32::try_from(pid).ok()?;
    let mut info = std::mem::MaybeUninit::<libc::proc_vnodepathinfo>::uninit();
    let size = i32::try_from(std::mem::size_of::<libc::proc_vnodepathinfo>()).ok()?;
    // SAFETY: `info`는 `size` 바이트짜리 쓰기 가능한 `proc_vnodepathinfo` 버퍼다. pid와
    // flavor는 고정 상수/입력 검증을 거쳤고, 반환값이 정확히 `size`일 때만 아래에서 읽는다.
    let written = unsafe {
        libc::proc_pidinfo(
            pid_i32,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    // 부분 기록을 그대로 읽으면 UB이므로, 요청한 크기와 정확히 같을 때만 읽는다.
    if written != size {
        return None;
    }
    // SAFETY: 위에서 `written == size`를 확인했으므로 구조체 전체가 커널에 의해 채워졌다.
    let info = unsafe { info.assume_init() };

    // `vip_path`는 rustc 구버전의 배열 크기 제약 때문에 `[c_char; MAXPATHLEN]`이 아니라
    // `[[c_char; 32]; 32]`(32*32 = MAXPATHLEN = 1024)로 평탄화되어 노출된다(libc crate
    // 소스 주석 참조). NUL 종료를 신뢰하지 않고, NUL을 만나면 그 지점에서 안전하게 자른다.
    let mut path_bytes: Vec<u8> = Vec::with_capacity(1024);
    'outer: for row in info.pvi_cdir.vip_path.iter() {
        for &ch in row.iter() {
            if ch == 0 {
                break 'outer;
            }
            path_bytes.push(ch as u8);
        }
    }
    if path_bytes.is_empty() {
        return None;
    }
    let os_str = std::ffi::OsStr::from_bytes(&path_bytes);
    Some(std::path::PathBuf::from(os_str))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "macos")]
    fn pid_start_time는_자기_자신에_대해_같은_값을_안정적으로_돌려준다() {
        let me = std::process::id();
        let first = pid_start_time(me);
        let second = pid_start_time(me);
        assert!(first.is_some(), "자기 자신의 시작 시각을 못 얻음");
        assert_eq!(first, second, "같은 프로세스에 대해 두 번 호출한 값이 다름");
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn pid_cwd는_자기_자신에_대해_존재하는_절대경로_디렉터리를_돌려준다() {
        let me = std::process::id();
        let cwd = pid_cwd(me).expect("자기 자신의 cwd를 못 얻음");
        assert!(cwd.is_absolute(), "cwd가 절대경로가 아님: {cwd:?}");
        assert!(cwd.is_dir(), "cwd가 디렉터리가 아님: {cwd:?}");

        // 심볼릭 링크(예: macOS의 /tmp -> /private/tmp) 차이로 바이트 단위 비교가 깨질 수
        // 있어 canonicalize 후 비교한다. 그래도 두 경로 중 하나가 존재하지 않거나
        // canonicalize가 실패하는 극히 드문 CI 환경 차이가 있을 수 있으므로, 실패 시
        // "존재+절대경로+디렉터리" 판정(위 두 assert)까지만 남기고 통과시킨다.
        if let (Ok(expected), Ok(actual)) = (
            std::env::current_dir().and_then(|p| p.canonicalize()),
            cwd.canonicalize(),
        ) {
            assert_eq!(actual, expected, "cwd가 std::env::current_dir()와 다름");
        }
    }

    #[test]
    fn 존재하지_않는_pid는_start_time과_cwd_모두_none이고_패닉하지_않는다() {
        // libc::pid_t::MAX는 존재할 가능성이 극히 낮은 pid다(macOS pid_t는 i32, 실제
        // 커널이 배정하는 pid 상한은 훨씬 낮다). CI/로컬 어느 쪽에서도 이 pid가 살아있을
        // 확률은 사실상 0이라 flaky하지 않다.
        let improbable_pid = libc::pid_t::MAX as u32;
        assert_eq!(pid_start_time(improbable_pid), None);
        assert_eq!(pid_cwd(improbable_pid), None);
    }

    #[test]
    fn pid_0에_대해_패닉하지_않는다() {
        // pid=0은 macOS에서 kernel_task를 가리킬 수 있어 반환값을 특정 짓지 않는다 —
        // 여기서는 오직 "패닉하지 않음"만 검증한다.
        let _ = pid_start_time(0);
        let _ = pid_cwd(0);
    }
}
