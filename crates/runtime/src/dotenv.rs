//! `.env` 파일 파싱 유틸 — app(dotenv_sync)과 runtime 워커(복원 시 pane별 .env 주입,
//! 2026-07-08 A안)가 공유한다. app crate에서 이동해 왔다.

use std::io::Read as _;
use std::path::Path;

/// All dotenv files participating in one merge share this byte ceiling.
pub const DOTENV_TOTAL_BYTES_MAX: usize = 1024 * 1024;
/// Both parsed occurrences and unique merged entries are capped at this value.
pub const DOTENV_ENTRIES_MAX: usize = 4_096;
pub const DOTENV_KEY_BYTES_MAX: usize = 1024;
pub const DOTENV_VALUE_BYTES_MAX: usize = 64 * 1024;
pub const DOTENV_FILE_NAMES: [&str; 2] = [".env", ".env.local"];

/// 앱에서 선택한 프로젝트 루트와 순서. 값은 포함하지 않는다.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DotenvSourceSelection {
    pub root: Option<std::path::PathBuf>,
    pub files: Vec<String>,
}

const ERROR_INPUT_BYTES: &str = "dotenv_input_bytes_exceeded";
const ERROR_TOTAL_BYTES: &str = "dotenv_total_bytes_exceeded";
const ERROR_ENTRY_BUDGET: &str = "dotenv_entry_budget_invalid";
const ERROR_ENTRIES: &str = "dotenv_entries_exceeded";
const ERROR_KEY_BYTES: &str = "dotenv_key_bytes_exceeded";
const ERROR_VALUE_BYTES: &str = "dotenv_value_bytes_exceeded";
const ERROR_READ: &str = "dotenv_read_failed";
const ERROR_UTF8: &str = "dotenv_utf8_invalid";
const ERROR_UNIQUE_ENTRIES: &str = "dotenv_unique_entries_exceeded";

fn parse_line(line: &str) -> Option<(&str, &str)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
    let (key, value) = line.split_once('=')?;
    let key = key.trim();
    if key.is_empty()
        || !key.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '_' || character == '.'
        })
    {
        return None;
    }
    let value = value.trim();
    // Quoted values stop at the matching quote. Unquoted trailing comments keep the historical
    // `space + #` rule; this helper is shared by the compatible and bounded APIs.
    let value = if let Some(rest) = value.strip_prefix('"') {
        rest.split_once('"').map_or(rest, |(value, _)| value)
    } else if let Some(rest) = value.strip_prefix('\'') {
        rest.split_once('\'').map_or(rest, |(value, _)| value)
    } else if let Some((value, _comment)) = value.split_once(" #") {
        value.trim_end()
    } else {
        value
    };
    Some((key, value))
}

/// `.env` 한 파일을 파싱한다 — `KEY=VALUE`, `export KEY=VALUE`, 주석(#)/빈 줄 무시,
/// 양끝 따옴표('...', "...") 제거. 잘못된 줄은 건너뛴다(엄격 실패 없음 — 사용자 파일).
pub fn parse_dotenv(content: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in content.lines() {
        if let Some((key, value)) = parse_line(line) {
            out.push((key.to_owned(), value.to_owned()));
        }
    }
    out
}

/// Fail-closed parser for persistence/import paths. `remaining_entries` is the caller's remaining
/// aggregate occurrence budget across every file in the merge. The check happens before `push`,
/// so the temporary Vec never contains more than the configured maximum.
pub fn parse_dotenv_bounded(
    content: &str,
    remaining_entries: usize,
) -> anyhow::Result<Vec<(String, String)>> {
    anyhow::ensure!(content.len() <= DOTENV_TOTAL_BYTES_MAX, ERROR_INPUT_BYTES);
    anyhow::ensure!(remaining_entries <= DOTENV_ENTRIES_MAX, ERROR_ENTRY_BUDGET);

    let mut out = Vec::with_capacity(remaining_entries.min(64));
    for line in content.lines() {
        let Some((key, value)) = parse_line(line) else {
            continue;
        };
        anyhow::ensure!(key.len() <= DOTENV_KEY_BYTES_MAX, ERROR_KEY_BYTES);
        anyhow::ensure!(value.len() <= DOTENV_VALUE_BYTES_MAX, ERROR_VALUE_BYTES);
        anyhow::ensure!(out.len() < remaining_entries, ERROR_ENTRIES);
        out.push((key.to_owned(), value.to_owned()));
    }
    Ok(out)
}

/// Read one dotenv file using only the caller's remaining aggregate budget plus one proof byte.
/// Metadata length is intentionally ignored so growth between open/read cannot bypass the cap.
pub fn read_dotenv_file_bounded(
    path: &Path,
    remaining_bytes: &mut usize,
) -> anyhow::Result<Option<String>> {
    anyhow::ensure!(
        *remaining_bytes <= DOTENV_TOTAL_BYTES_MAX,
        ERROR_TOTAL_BYTES
    );
    let before = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => anyhow::bail!(ERROR_READ),
    };
    anyhow::ensure!(
        before.is_file() && !before.file_type().is_symlink(),
        "dotenv_file_type_invalid"
    );
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        options.custom_flags(0x0020_0000);
    }
    let file = options
        .open(path)
        .map_err(|_| anyhow::anyhow!(ERROR_READ))?;
    let opened = file.metadata().map_err(|_| anyhow::anyhow!(ERROR_READ))?;
    anyhow::ensure!(opened.is_file(), ERROR_READ);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        anyhow::ensure!(
            before.dev() == opened.dev() && before.ino() == opened.ino(),
            ERROR_READ
        );
    }
    let probe = remaining_bytes
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!(ERROR_TOTAL_BYTES))?;
    let probe_u64 = u64::try_from(probe).map_err(|_| anyhow::anyhow!(ERROR_TOTAL_BYTES))?;
    let mut bytes = Vec::with_capacity(probe);
    file.take(probe_u64)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow::anyhow!(ERROR_READ))?;
    anyhow::ensure!(bytes.len() <= *remaining_bytes, ERROR_TOTAL_BYTES);
    *remaining_bytes -= bytes.len();
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| anyhow::anyhow!(ERROR_UTF8))
}

/// Read and merge `.env` then `.env.local` under one byte, occurrence, and unique-entry budget.
/// Later files replace values in-place without changing first-seen order.
pub fn read_dotenv_merged_bounded(root: &Path) -> anyhow::Result<Option<Vec<(String, String)>>> {
    read_dotenv_files_bounded(root, &deppy_core::env_sources::default_files())
}

/// 명시한 순서대로 루트 파일을 읽는다. 빈 목록은 파일 사용 중지다.
pub fn read_dotenv_files_bounded(
    root: &Path,
    files: &[String],
) -> anyhow::Result<Option<Vec<(String, String)>>> {
    anyhow::ensure!(
        deppy_core::env_sources::valid_files(files),
        "env_source_name_invalid"
    );
    let mut merged: Vec<(String, String)> = Vec::new();
    let mut positions: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut remaining_bytes = DOTENV_TOTAL_BYTES_MAX;
    let mut remaining_entries = DOTENV_ENTRIES_MAX;
    let mut found = false;

    for name in files {
        let Some(content) = read_dotenv_file_bounded(&root.join(name), &mut remaining_bytes)?
        else {
            continue;
        };
        found = true;
        let parsed = parse_dotenv_bounded(&content, remaining_entries)?;
        remaining_entries -= parsed.len();
        for (key, value) in parsed {
            if let Some(index) = positions.get(&key).copied() {
                merged[index].1 = value;
            } else {
                anyhow::ensure!(merged.len() < DOTENV_ENTRIES_MAX, ERROR_UNIQUE_ENTRIES);
                positions.insert(key.clone(), merged.len());
                merged.push((key, value));
            }
        }
    }
    Ok(found.then_some(merged))
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
        // `PRIVATE` 단독이 아니라 `PRIVATE_KEY`다(2026-08-21). 단독 부분문자열은
        // `ALLOW_PRIVATE_URLS` 같은 평범한 플래그까지 비밀로 잡는데, 그 값이 redaction
        // 최소 길이 미만이면 dotenv 동기화 전체가 fail-closed로 죽어 워크스페이스가
        // 통째로 막혔다(사용자 보고). 저장소의 `secret_like_env_key`도 이 개념을
        // `PRIVATE_KEY`/`_PRIVATE_KEY`로만 잡으므로, 좁히는 쪽이 두 판정의 간극을
        // **줄인다** — 넓히는 게 아니다.
        "PRIVATE_KEY",
    ]
    .iter()
    .any(|marker| upper.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_parser_behavior_remains_tolerant_and_compatible() {
        let content = "# comment\nexport A=1\nA=2 # tail\nB=\"quoted value\" # tail\nBAD LINE\n";
        assert_eq!(
            parse_dotenv(content),
            vec![
                ("A".to_owned(), "1".to_owned()),
                ("A".to_owned(), "2".to_owned()),
                ("B".to_owned(), "quoted value".to_owned()),
            ]
        );
    }

    #[test]
    fn bounded_parser_enforces_occurrence_budget_before_push() {
        let content = "A=1\nA=2\nA=3\n";
        let error = parse_dotenv_bounded(content, 2).unwrap_err();
        assert_eq!(error.to_string(), ERROR_ENTRIES);
        assert_eq!(parse_dotenv_bounded("A=1\nA=2\n", 2).unwrap().len(), 2);
        assert!(parse_dotenv_bounded("# ignored\n", 0).unwrap().is_empty());
    }

    #[test]
    fn bounded_parser_enforces_input_key_and_value_bytes() {
        let oversized_input = "#".repeat(DOTENV_TOTAL_BYTES_MAX + 1);
        assert_eq!(
            parse_dotenv_bounded(&oversized_input, DOTENV_ENTRIES_MAX)
                .unwrap_err()
                .to_string(),
            ERROR_INPUT_BYTES
        );

        let oversized_key = format!("{}=v", "K".repeat(DOTENV_KEY_BYTES_MAX + 1));
        assert_eq!(
            parse_dotenv_bounded(&oversized_key, DOTENV_ENTRIES_MAX)
                .unwrap_err()
                .to_string(),
            ERROR_KEY_BYTES
        );
        let oversized_value = format!("K={}", "v".repeat(DOTENV_VALUE_BYTES_MAX + 1));
        assert_eq!(
            parse_dotenv_bounded(&oversized_value, DOTENV_ENTRIES_MAX)
                .unwrap_err()
                .to_string(),
            ERROR_VALUE_BYTES
        );

        let maximum = format!(
            "{}={}",
            "K".repeat(DOTENV_KEY_BYTES_MAX),
            "v".repeat(DOTENV_VALUE_BYTES_MAX)
        );
        assert_eq!(parse_dotenv_bounded(&maximum, 1).unwrap().len(), 1);
    }

    #[test]
    fn bounded_parser_rejects_invalid_caller_budget() {
        assert_eq!(
            parse_dotenv_bounded("A=1", DOTENV_ENTRIES_MAX + 1)
                .unwrap_err()
                .to_string(),
            ERROR_ENTRY_BUDGET
        );
    }

    #[test]
    fn bounded_reader_accepts_exact_aggregate_and_rejects_one_byte_over() {
        let dir = std::env::temp_dir().join(format!(
            "deppy-runtime-dotenv-bytes-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let half = DOTENV_TOTAL_BYTES_MAX / 2;
        std::fs::write(dir.join(".env"), vec![b'#'; half]).unwrap();
        std::fs::write(dir.join(".env.local"), vec![b'#'; half]).unwrap();
        assert!(
            read_dotenv_merged_bounded(&dir)
                .unwrap()
                .unwrap()
                .is_empty()
        );

        std::fs::write(dir.join(".env.local"), vec![b'#'; half + 1]).unwrap();
        assert_eq!(
            read_dotenv_merged_bounded(&dir).unwrap_err().to_string(),
            ERROR_TOTAL_BYTES
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bounded_reader_shares_occurrence_budget_across_both_files() {
        let dir = std::env::temp_dir().join(format!(
            "deppy-runtime-dotenv-items-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".env"), "A=1\n".repeat(DOTENV_ENTRIES_MAX / 2)).unwrap();
        std::fs::write(
            dir.join(".env.local"),
            "A=2\n".repeat(DOTENV_ENTRIES_MAX / 2),
        )
        .unwrap();
        let merged = read_dotenv_merged_bounded(&dir).unwrap().unwrap();
        assert_eq!(merged, vec![("A".to_owned(), "2".to_owned())]);

        std::fs::write(
            dir.join(".env.local"),
            "A=2\n".repeat(DOTENV_ENTRIES_MAX / 2 + 1),
        )
        .unwrap();
        assert_eq!(
            read_dotenv_merged_bounded(&dir).unwrap_err().to_string(),
            ERROR_ENTRIES
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
