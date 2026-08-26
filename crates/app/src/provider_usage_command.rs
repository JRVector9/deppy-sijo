//! Claude/Kimi 사용량 프로브의 실행 명령 구성.
//!
//! 런처가 감지한 실행 파일은 절대 경로지만 Windows의 `.cmd`/`.bat`는 직접 실행할 수
//! 없다. 이때 probe PATH에서 `cmd.exe`를 다시 찾으면 사용자 쓰기 가능한 디렉터리의
//! 동명 파일에 가로채질 수 있으므로 Win32가 알려 주는 시스템 디렉터리를 사용한다.

use std::path::{Path, PathBuf};

pub(crate) fn probe_program_and_args(
    executable: &Path,
    windows: bool,
    provider: &str,
) -> anyhow::Result<(String, Vec<String>)> {
    let executable = executable
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("{provider} executable path is not UTF-8"))?;
    let is_batch = Path::new(executable)
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat")
        });
    if !windows || !is_batch {
        return Ok((executable.to_owned(), Vec::new()));
    }

    let command_processor = trusted_windows_command_processor()?;
    let command_processor = command_processor
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Windows system command processor path is not UTF-8"))?;
    // portable-pty는 Windows 인자를 C 런타임 규칙으로 묶지만 cmd.exe는 자기 문법으로
    // 다시 읽는다. `/s /c`가 바깥따옴표를 벗긴 뒤에도 경로가 한 명령 토큰으로 남도록
    // cmd 메타문자를 caret으로 이스케이프한다.
    Ok((
        command_processor.to_owned(),
        vec![
            "/d".to_owned(),
            // `%` 보호에 쓰는 `%cd:~,%` substring 치환은 command extensions가
            // 필요하다. 레지스트리 기본값에 기대지 않고 이 프로세스에서만 켠다.
            "/e:on".to_owned(),
            "/v:off".to_owned(),
            "/s".to_owned(),
            "/c".to_owned(),
            escape_cmd_command(executable),
        ],
    ))
}

fn escape_cmd_command(command: &str) -> String {
    let mut escaped = String::with_capacity(command.len());
    for character in command.chars() {
        if character == '%' {
            // caret은 `%VAR%` 치환보다 늦게 처리돼 `%`를 보호하지 못한다. Rust 표준
            // 라이브러리의 batch 인자 처리와 같은 zero-length `%cd:~,%` 치환을 끼워
            // 원래 퍼센트가 변수 이름의 경계로 짝지어지지 않게 한다.
            escaped.push_str("%%cd:~,%%");
            continue;
        }
        if matches!(
            character,
            '(' | ')'
                | '['
                | ']'
                | '!'
                | '^'
                | '"'
                | '`'
                | '<'
                | '>'
                | '&'
                | '|'
                | ';'
                | ','
                | '='
                | ' '
                | '*'
                | '?'
        ) {
            escaped.push('^');
        }
        escaped.push(character);
    }
    escaped
}

#[cfg(windows)]
fn trusted_windows_command_processor() -> anyhow::Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt as _;

    use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;

    let mut buffer = vec![0_u16; 260];
    loop {
        let length = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) };
        if length == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if (length as usize) < buffer.len() {
            buffer.truncate(length as usize);
            let directory = std::ffi::OsString::from_wide(&buffer);
            return Ok(PathBuf::from(directory).join("cmd.exe"));
        }
        buffer.resize(length as usize + 1, 0);
    }
}

// 비-Windows 빌드에서 `windows = true` 테스트가 production 분기를 검증하기 위한 대역.
// 실제 Windows 빌드는 위 Win32 API 구현만 컴파일한다.
#[cfg(not(windows))]
fn trusted_windows_command_processor() -> anyhow::Result<PathBuf> {
    Ok(PathBuf::from(r"C:\Windows\System32\cmd.exe"))
}

// 기대값이 production resolver를 재사용하면 resolver가 PATH 탐색으로 퇴행해도 테스트가
// 함께 바뀌어 통과한다. 테스트 전용 구현은 의도적으로 Win32 호출/대역을 독립 복제한다.
#[cfg(all(test, windows))]
pub(crate) fn trusted_windows_command_processor_for_test() -> anyhow::Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt as _;

    use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;

    let mut buffer = vec![0_u16; 260];
    loop {
        let length = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) };
        if length == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if (length as usize) < buffer.len() {
            buffer.truncate(length as usize);
            let directory = std::ffi::OsString::from_wide(&buffer);
            return Ok(PathBuf::from(directory).join("cmd.exe"));
        }
        buffer.resize(length as usize + 1, 0);
    }
}

#[cfg(all(test, not(windows)))]
pub(crate) fn trusted_windows_command_processor_for_test() -> anyhow::Result<PathBuf> {
    Ok(PathBuf::from(r"C:\Windows\System32\cmd.exe"))
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn cmd와_bat은_유효한_cmd특수문자_경로에서_실제로_실행된다() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "deppy-usage-command-{}-{nonce}",
            std::process::id()
        ));
        let _cleanup = TempDir(root.clone());
        let special_directory = root.join("space ()[]!^`&;,= percent%");
        std::fs::create_dir_all(&special_directory).expect("special-character directory");

        for extension in ["cmd", "bat"] {
            let shim = special_directory.join(format!("usage.{extension}"));
            std::fs::write(&shim, b"@echo DEPPY_USAGE_SHIM_OK\r\n").expect("usage shim");
            let (program, args) =
                probe_program_and_args(&shim, true, "test").expect("wrapped Windows usage command");

            let output = std::process::Command::new(program)
                .args(args)
                // 검색 경로와 COMSPEC가 공격자 제어여도 절대 System32 cmd.exe를 쓴다.
                .env("PATH", &root)
                .env("COMSPEC", root.join("attacker.exe"))
                .output()
                .expect("execute wrapped usage shim");

            assert!(output.status.success(), "{extension} failed: {output:?}");
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("DEPPY_USAGE_SHIM_OK"),
                "{extension} output missing marker: {output:?}"
            );
        }
    }
}
