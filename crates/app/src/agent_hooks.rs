//! 에이전트 상태 hook 전역 설치 (옵션2 needsInput). claude/codex의 hook 설정에
//! deppy-mcp-proxy 호출을 등록해, 승인·입력 대기(needsInput)를 에이전트가 직접 보고하게
//! 한다. 손타이핑·절대경로·alias 등 실행 방법과 무관하게 잡힌다(전역 설치).
//!
//! - 바인딩: 셸 spawn 시 주입한 `DEPPY_SESSION_ID` env를 hook이 그대로 물려받아 세션 식별.
//! - 보고: hook 커맨드 = `'<proxy>' hooks --db '<db>' --event <needs-input|clear> --deppy-hook`.
//!   `--deppy-hook`은 우리 항목을 식별하는 마커(uninstall 시 이것만 제거).
//! - 안전: 기존 설정을 병합(clobber 안 함), 원자적 쓰기(tmp+rename), 파싱 실패 시 미변경.
//!
//! claude(`~/.claude/settings.json`, JSON) + codex(`~/.codex/config.toml`, TOML — toml_edit로
//! 포맷 보존). codex는 features.hooks=true로 켜지고 최초 1회 codex TUI trust 승인이 필요하다
//! (codex 자체 동작, deppy는 프롬프트 안 띄움).

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use serde_json::Value;

/// 우리 hook 항목을 식별하는 커맨드 마커.
const MARKER: &str = "--deppy-hook";
/// Third-party hook settings are control data; refuse bulk files before JSON/TOML allocation.
const HOOK_CONFIG_BYTES_MAX: usize = 1024 * 1024;

fn open_hook_config_read_only(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        #[cfg(target_os = "macos")]
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        #[cfg(any(target_os = "linux", target_os = "android"))]
        options.custom_flags(0x20_000 | 0x800);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options.open(path)
}

fn read_hook_config_bounded(path: &Path, max_bytes: usize) -> anyhow::Result<Option<String>> {
    let before = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => anyhow::bail!("agent_hook_config_read_failed"),
    };
    anyhow::ensure!(
        before.file_type().is_file() && !before.file_type().is_symlink(),
        "agent_hook_config_type_invalid"
    );
    let mut file = open_hook_config_read_only(path)
        .map_err(|_| anyhow::anyhow!("agent_hook_config_open_failed"))?;
    let opened = file
        .metadata()
        .map_err(|_| anyhow::anyhow!("agent_hook_config_metadata_failed"))?;
    anyhow::ensure!(opened.is_file(), "agent_hook_config_type_invalid");
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        anyhow::ensure!(
            before.dev() == opened.dev() && before.ino() == opened.ino(),
            "agent_hook_config_changed"
        );
    }
    anyhow::ensure!(
        opened.len() <= max_bytes as u64,
        "agent_hook_config_bytes_exceeded"
    );
    let probe = max_bytes
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("agent_hook_config_bytes_exceeded"))?;
    let mut bytes = Vec::with_capacity((opened.len() as usize).min(probe));
    std::io::Read::by_ref(&mut file)
        .take(probe as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow::anyhow!("agent_hook_config_read_failed"))?;
    anyhow::ensure!(bytes.len() <= max_bytes, "agent_hook_config_bytes_exceeded");
    let after = file
        .metadata()
        .map_err(|_| anyhow::anyhow!("agent_hook_config_metadata_failed"))?;
    anyhow::ensure!(
        after.len() == bytes.len() as u64,
        "agent_hook_config_changed"
    );
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| anyhow::anyhow!("agent_hook_config_utf8_invalid"))
}

/// 셸 커맨드 인자로 안전하게 감싼다 — 작은따옴표 안에 넣되 내부 작은따옴표는 '\'' 로 이스케이프.
/// 경로에 공백/작은따옴표(예: /Users/O'Connor/…)가 있어도 hook 커맨드가 안 깨진다(codex 지적).
pub(crate) fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// hook 수신 커맨드 문자열 — claude/codex 공통. 경로는 sh_quote로 안전 인용.
pub(crate) fn hook_command(proxy_bin: &str, db_path: &std::path::Path, event: &str) -> String {
    format!(
        "{} hooks --db {} --event {event} {MARKER}",
        sh_quote(proxy_bin),
        sh_quote(&db_path.display().to_string())
    )
}

/// claude statusLine 오버레이 command — proxy가 effort/model/context%를 DB에 기록하고
/// 사용자 원래 statusLine을 체이닝한다(2026-07-08).
pub(crate) fn statusline_command(proxy_bin: &str, db_path: &std::path::Path) -> String {
    format!(
        "{} statusline --db {}",
        sh_quote(proxy_bin),
        sh_quote(&db_path.display().to_string())
    )
}

fn claude_settings_path() -> Option<PathBuf> {
    Some(crate::paths::home_dir()?.join(".claude/settings.json"))
}

/// 우리 hook 항목인가 (커맨드에 마커 포함).
fn is_ours(entry: &Value) -> bool {
    entry
        .pointer("/hooks")
        .and_then(Value::as_array)
        .is_some_and(|hs| {
            hs.iter().any(|h| {
                h.get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|c| c.contains(MARKER))
            })
        })
}

/// 설정 JSON을 원자적으로 쓴다 (tmp+rename).
fn write_atomic(path: &std::path::Path, root: &Value) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|_| anyhow::anyhow!("agent_hook_directory_create_failed"))?;
    }
    let text = serde_json::to_string_pretty(root)
        .map_err(|_| anyhow::anyhow!("agent_hook_config_serialize_failed"))?;
    anyhow::ensure!(
        text.len() <= HOOK_CONFIG_BYTES_MAX,
        "agent_hook_config_bytes_exceeded"
    );
    deppy_core::fs::atomic_write(path, text.as_bytes())
        .map_err(|_| anyhow::anyhow!("agent_hook_config_write_failed"))?;
    Ok(())
}

/// claude 전역 설정에서 deppy hook 항목만 제거한다 (설정 토글 OFF).
pub fn uninstall_claude() -> anyhow::Result<()> {
    let Some(path) = claude_settings_path() else {
        return Ok(());
    };
    let Some(text) = read_hook_config_bounded(&path, HOOK_CONFIG_BYTES_MAX)? else {
        return Ok(());
    };
    let Ok(mut root) = serde_json::from_str::<Value>(&text) else {
        return Ok(()); // 파싱 실패면 건드리지 않음
    };
    let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) else {
        return Ok(());
    };
    for arr in hooks.values_mut() {
        if let Some(a) = arr.as_array_mut() {
            a.retain(|e| !is_ours(e));
        }
    }
    write_atomic(&path, &root)
}

// ── codex (~/.codex/config.toml, TOML) ──
// 포맷 보존 편집(toml_edit) — 사용자 hand-edit(projects/notify/trust)을 지킨다. codex hook은
// features.hooks=true로 켜지지만 실행엔 trust가 필요해, 손타이핑 codex 최초 1회 codex TUI에서
// 신뢰 승인이 뜬다(codex 자체 동작, 그 후 영속·자동). deppy는 프롬프트를 띄우지 않는다.

fn codex_config_path() -> Option<PathBuf> {
    Some(crate::paths::home_dir()?.join(".codex/config.toml"))
}

/// 이 hook 그룹이 우리 것인가(내부 command에 마커 포함).
fn codex_group_is_ours(group: &toml_edit::Value) -> bool {
    group
        .as_inline_table()
        .and_then(|t| t.get("hooks"))
        .and_then(|h| h.as_array())
        .is_some_and(|hooks| {
            hooks.iter().any(|h| {
                h.as_inline_table()
                    .and_then(|t| t.get("command"))
                    .and_then(|c| c.as_str())
                    .is_some_and(|c| c.contains(MARKER))
            })
        })
}

/// TOML 텍스트를 최초 1회 백업 + 원자적으로 쓴다.
fn codex_write(path: &std::path::Path, text: &str, original: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        text.len() <= HOOK_CONFIG_BYTES_MAX,
        "agent_hook_config_bytes_exceeded"
    );
    let backup = path.with_extension("toml.pre-deppy");
    match std::fs::symlink_metadata(&backup) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                options.mode(0o600);
                #[cfg(target_os = "macos")]
                options.custom_flags(libc::O_NOFOLLOW);
                #[cfg(any(target_os = "linux", target_os = "android"))]
                options.custom_flags(0x20_000);
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::OpenOptionsExt as _;
                const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
                options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
            }
            match options.open(&backup) {
                Ok(mut file) => file
                    .write_all(original.as_bytes())
                    .map_err(|_| anyhow::anyhow!("agent_hook_backup_write_failed"))?,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(_) => anyhow::bail!("agent_hook_backup_open_failed"),
            }
        }
        Err(_) => anyhow::bail!("agent_hook_backup_metadata_failed"),
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|_| anyhow::anyhow!("agent_hook_directory_create_failed"))?;
    }
    deppy_core::fs::atomic_write(path, text.as_bytes())
        .map_err(|_| anyhow::anyhow!("agent_hook_config_write_failed"))?;
    Ok(())
}

/// codex 전역 설정에서 deppy hook 항목만 제거한다(설정 토글 OFF).
pub fn uninstall_codex() -> anyhow::Result<()> {
    let Some(path) = codex_config_path() else {
        return Ok(());
    };
    let Some(text) = read_hook_config_bounded(&path, HOOK_CONFIG_BYTES_MAX)? else {
        return Ok(());
    };
    let Ok(mut doc) = text.parse::<toml_edit::DocumentMut>() else {
        return Ok(()); // 파싱 실패면 건드리지 않음
    };
    codex_remove(&mut doc);
    codex_write(&path, &doc.to_string(), &text)
}

/// codex config 문서에서 우리 hook만 제거한다(파일 I/O 없음 — 테스트 가능).
fn codex_remove(doc: &mut toml_edit::DocumentMut) {
    if let Some(hooks) = doc.get_mut("hooks").and_then(|h| h.as_table_mut()) {
        // 각 이벤트 배열에서 우리 그룹만 제거하고, 비면 그 이벤트 키를 지운다(사용자 hook 보존).
        let events: Vec<String> = hooks.iter().map(|(k, _)| k.to_owned()).collect();
        for k in events {
            if let Some(arr) = hooks.get_mut(&k).and_then(|v| v.as_array_mut()) {
                arr.retain(|g| !codex_group_is_ours(g));
                if arr.is_empty() {
                    hooks.remove(&k);
                }
            }
        }
    }
    // hook이 하나도 안 남으면 [hooks]와 features.hooks를 정리한다 — 우리가 켠 기능을 되돌려
    // 무관한 hook이 켜진 채 남지 않게(codex Medium). 사용자 hook이 남아 있으면 그대로 둔다.
    let hooks_empty = doc
        .get("hooks")
        .and_then(|h| h.as_table())
        .is_none_or(|t| t.is_empty());
    if hooks_empty {
        doc.remove("hooks");
        // features.hooks 제거 — [features] 섹션(Table)이든 인라인(features={...})이든 처리.
        let features_empty = match doc.get_mut("features") {
            Some(f) if f.is_table() => {
                let t = f.as_table_mut().unwrap();
                t.remove("hooks");
                t.is_empty()
            }
            Some(f) if f.is_inline_table() => {
                let t = f.as_inline_table_mut().unwrap();
                t.remove("hooks");
                t.is_empty()
            }
            _ => false,
        };
        if features_empty {
            doc.remove("features");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "deppy-hook-bound-{tag}-{}-{nonce}",
            std::process::id()
        ))
    }

    #[test]
    fn hook_config_reader_accepts_exact_and_rejects_plus_one_and_invalid_utf8() {
        let path = temp_path("bytes");
        std::fs::write(&path, vec![b'x'; 64]).unwrap();
        assert_eq!(
            read_hook_config_bounded(&path, 64).unwrap().unwrap().len(),
            64
        );
        std::fs::write(&path, vec![b'x'; 65]).unwrap();
        assert_eq!(
            read_hook_config_bounded(&path, 64).unwrap_err().to_string(),
            "agent_hook_config_bytes_exceeded"
        );
        std::fs::write(&path, [0xff]).unwrap();
        assert_eq!(
            read_hook_config_bounded(&path, 64).unwrap_err().to_string(),
            "agent_hook_config_utf8_invalid"
        );
        let _ = std::fs::remove_file(path);
    }

    #[cfg(unix)]
    #[test]
    fn hook_config_reader_rejects_symlink_and_special_file() {
        use std::os::unix::fs::symlink;

        let dir = temp_path("types");
        std::fs::create_dir_all(&dir).unwrap();
        let regular = dir.join("regular");
        let link = dir.join("link");
        std::fs::write(&regular, b"ok").unwrap();
        symlink(&regular, &link).unwrap();
        assert_eq!(
            read_hook_config_bounded(&link, 64).unwrap_err().to_string(),
            "agent_hook_config_type_invalid"
        );
        assert_eq!(
            read_hook_config_bounded(Path::new("/dev/null"), 64)
                .unwrap_err()
                .to_string(),
            "agent_hook_config_type_invalid"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn production_hook_config_has_no_unbounded_read_or_copy() {
        let production = include_str!("agent_hooks.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        assert!(!production.contains("read_to_string"));
        assert!(!production.contains("std::fs::copy"));
        assert!(production.contains(".take(probe as u64)"));
        assert!(production.contains("HOOK_CONFIG_BYTES_MAX"));
    }

    #[test]
    fn sh_quote_escapes_single_quotes() {
        assert_eq!(sh_quote("/a/b c"), "'/a/b c'");
        assert_eq!(sh_quote("/O'Connor/x"), "'/O'\\''Connor/x'");
    }
}
