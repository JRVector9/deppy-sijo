//! 환경파일 선택의 순수 규칙. DB나 파일 시스템에 접근하지 않는다.
pub fn default_files() -> Vec<String> {
    vec![".env".into(), ".env.local".into()]
}

/// 루트 파일명만 허용해 경로 이탈과 부모 심볼릭 링크를 차단한다.
pub fn valid_files(files: &[String]) -> bool {
    if files.len() > 16 {
        return false;
    }
    let mut seen = std::collections::HashSet::new();
    files.iter().all(|name| {
        !name.is_empty()
            && name.len() <= 255
            && name != "."
            && name != ".."
            && name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
            && seen.insert(name)
    })
}
