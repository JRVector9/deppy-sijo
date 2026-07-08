//! `.env` 파일 파싱 유틸 — app(dotenv_sync)과 runtime 워커(복원 시 pane별 .env 주입,
//! 2026-07-08 A안)가 공유한다. app crate에서 이동해 왔다.

/// `.env` 한 파일을 파싱한다 — `KEY=VALUE`, `export KEY=VALUE`, 주석(#)/빈 줄 무시,
/// 양끝 따옴표('...', "...") 제거. 잘못된 줄은 건너뛴다(엄격 실패 없음 — 사용자 파일).
pub fn parse_dotenv(content: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty()
            || !key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
        {
            continue;
        }
        let value = value.trim();
        // 따옴표 값은 닫는 따옴표까지가 값 — 그 뒤(후행 주석 등)는 버린다.
        // `FOO="bar" # comment`가 `"bar"`로 저장되던 것 수정(codex 리뷰).
        let value = if let Some(rest) = value.strip_prefix('"') {
            rest.split_once('"').map_or(rest, |(v, _)| v)
        } else if let Some(rest) = value.strip_prefix('\'') {
            rest.split_once('\'').map_or(rest, |(v, _)| v)
        } else if let Some((v, _comment)) = value.split_once(" #") {
            v.trim_end()
        } else {
            value
        };
        out.push((key.to_owned(), value.to_owned()));
    }
    out
}

/// 키 이름으로 secret 여부를 판별한다 — 보수적으로 넓게 잡는다(secret이 DB 평문으로
/// 남는 것보다 plain이 keyring에 들어가는 쪽이 안전).
pub fn is_secret_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    // DB 접속 URL은 자격증명을 포함한다 — storage::secret_like_env_key와 판정 정합
    // (복원 경로가 이 함수만 쓰므로 여기서도 secret으로 잡아 redaction 등록, codex High).
    if upper == "DATABASE_URL"
        || upper == "DB_URL"
        || upper.ends_with("_DATABASE_URL")
        || upper.ends_with("_DB_URL")
    {
        return true;
    }
    [
        "SECRET",
        "TOKEN",
        "KEY",
        "PASSWORD",
        "PASSWD",
        "PWD",
        "CREDENTIAL",
        "AUTH",
        "PRIVATE",
    ]
    .iter()
    .any(|marker| upper.contains(marker))
}
