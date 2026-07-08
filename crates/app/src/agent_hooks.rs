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

use std::path::PathBuf;

use serde_json::Value;

/// 우리 hook 항목을 식별하는 커맨드 마커.
const MARKER: &str = "--deppy-hook";

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
    Some(PathBuf::from(std::env::var_os("HOME")?).join(".claude/settings.json"))
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
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.deppytmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(root)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// claude 전역 설정에서 deppy hook 항목만 제거한다 (설정 토글 OFF).
pub fn uninstall_claude() -> anyhow::Result<()> {
    let Some(path) = claude_settings_path() else {
        return Ok(());
    };
    if !path.exists() {
        return Ok(());
    }
    let text = std::fs::read_to_string(&path)?;
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
    Some(PathBuf::from(std::env::var_os("HOME")?).join(".codex/config.toml"))
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
fn codex_write(path: &std::path::Path, text: &str) -> anyhow::Result<()> {
    if path.exists() {
        let backup = path.with_extension("toml.pre-deppy");
        if !backup.exists() {
            let _ = std::fs::copy(path, &backup);
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("toml.deppytmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// codex 전역 설정에서 deppy hook 항목만 제거한다(설정 토글 OFF).
pub fn uninstall_codex() -> anyhow::Result<()> {
    let Some(path) = codex_config_path() else {
        return Ok(());
    };
    if !path.exists() {
        return Ok(());
    }
    let Ok(mut doc) = std::fs::read_to_string(&path)?.parse::<toml_edit::DocumentMut>() else {
        return Ok(()); // 파싱 실패면 건드리지 않음
    };
    codex_remove(&mut doc);
    codex_write(&path, &doc.to_string())
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

    #[test]
    fn sh_quote_escapes_single_quotes() {
        assert_eq!(sh_quote("/a/b c"), "'/a/b c'");
        assert_eq!(sh_quote("/O'Connor/x"), "'/O'\\''Connor/x'");
    }
}
