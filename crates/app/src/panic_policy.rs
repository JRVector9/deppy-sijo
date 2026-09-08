//! Process-wide sanitized panic diagnostics.

fn sanitized_source_basename(file: &str) -> &str {
    std::path::Path::new(file)
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or("unknown")
}

/// Installs the production panic policy. Panic payloads may contain raw paths, tool arguments,
/// OAuth material, or secrets, so the hook deliberately ignores the payload. 컴파일된 소스의
/// 파일명·줄만 남겨 사용자 경로를 노출하지 않으면서 원인 위치는 찾을 수 있게 한다.
pub(crate) fn install_sanitized_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let (source_file, source_line) = info.location().map_or(("unknown", 0), |location| {
            (sanitized_source_basename(location.file()), location.line())
        });
        tracing::error!(
            kind = "panic",
            phase = "unwind",
            error_code = "panic",
            source_file,
            source_line,
            "application component panicked"
        );
    }));
}

#[cfg(test)]
mod tests {
    #[test]
    fn panic_위치는_사용자_경로를_버리고_소스파일명만_남긴다() {
        assert_eq!(
            super::sanitized_source_basename("/Users/person/private/project/src/workspace.rs"),
            "workspace.rs"
        );
        assert_eq!(super::sanitized_source_basename("app.rs"), "app.rs");
    }
}
