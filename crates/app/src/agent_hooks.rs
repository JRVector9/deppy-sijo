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

use serde_json::{Value, json};

/// 우리 hook 항목을 식별하는 커맨드 마커.
const MARKER: &str = "--deppy-hook";

/// 셸 커맨드 인자로 안전하게 감싼다 — 작은따옴표 안에 넣되 내부 작은따옴표는 '\'' 로 이스케이프.
/// 경로에 공백/작은따옴표(예: /Users/O'Connor/…)가 있어도 hook 커맨드가 안 깨진다(codex 지적).
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// hook 수신 커맨드 문자열 — claude/codex 공통. 경로는 sh_quote로 안전 인용.
fn hook_command(proxy_bin: &str, db_path: &std::path::Path, event: &str) -> String {
    format!(
        "{} hooks --db {} --event {event} {MARKER}",
        sh_quote(proxy_bin),
        sh_quote(&db_path.display().to_string())
    )
}

/// claude 이벤트 → needsInput 상태. Notification=대기 시작, 나머지=해제(재개/완료).
const CLAUDE_EVENTS: &[(&str, &str)] = &[
    ("Notification", "needs-input"),
    ("UserPromptSubmit", "clear"),
    ("PreToolUse", "clear"),
    ("Stop", "clear"),
];

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

/// claude 전역 설정에 deppy 상태 hook을 설치(병합)한다. proxy_bin/db_path는 커맨드에 박는다.
pub fn install_claude(db_path: &std::path::Path, proxy_bin: &str) -> anyhow::Result<()> {
    let Some(path) = claude_settings_path() else {
        return Ok(());
    };
    // 기존 설정 로드 — 파싱 실패(사용자 파일 손상/의도)면 건드리지 않는다.
    let mut root: Value = if path.exists() {
        let text = std::fs::read_to_string(&path)?;
        if text.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&text)
                .map_err(|e| anyhow::anyhow!("claude settings.json 파싱 실패 — 미변경: {e}"))?
        }
    } else {
        json!({})
    };
    if !root.is_object() {
        anyhow::bail!("claude settings.json 최상위가 객체가 아님 — 미변경");
    }
    let hooks = root
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }
    let hooks = hooks.as_object_mut().unwrap();
    for (event, kind) in CLAUDE_EVENTS {
        let cmd = hook_command(proxy_bin, db_path, kind);
        let entry = json!({ "hooks": [{ "type": "command", "command": cmd }] });
        let arr = hooks.entry(*event).or_insert_with(|| json!([]));
        if !arr.is_array() {
            *arr = json!([]);
        }
        let arr = arr.as_array_mut().unwrap();
        arr.retain(|e| !is_ours(e)); // 우리 옛 항목 제거(중복 방지)
        arr.push(entry);
    }
    write_atomic(&path, &root)
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

/// codex 이벤트 → needsInput 상태. PermissionRequest=승인/입력 대기, 나머지=해제.
const CODEX_EVENTS: &[(&str, &str)] = &[
    ("PermissionRequest", "needs-input"),
    ("UserPromptSubmit", "clear"),
    ("PreToolUse", "clear"),
    ("Stop", "clear"),
];

fn codex_config_path() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("HOME")?).join(".codex/config.toml"))
}

/// `{ hooks = [{ type="command", command=<cmd>, timeout=... }] }` 형태의 hook 그룹(인라인 테이블).
/// 이벤트 배열의 한 원소다 — 사용자 그룹과 나란히 넣을 수 있게 그룹 단위로 만든다.
fn codex_group(cmd: &str) -> toml_edit::Value {
    use toml_edit::{Array, InlineTable, Value};
    let mut inner = InlineTable::new();
    inner.insert("type", Value::from("command"));
    inner.insert("command", Value::from(cmd));
    inner.insert("timeout", Value::from(120_000_i64));
    let mut hooks = Array::new();
    hooks.push(Value::InlineTable(inner));
    let mut group = InlineTable::new();
    group.insert("hooks", Value::Array(hooks));
    Value::InlineTable(group)
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

/// codex 전역 설정에 deppy 상태 hook을 설치(포맷 보존 병합)한다.
pub fn install_codex(db_path: &std::path::Path, proxy_bin: &str) -> anyhow::Result<()> {
    let Some(path) = codex_config_path() else {
        return Ok(());
    };
    let text = if path.exists() {
        std::fs::read_to_string(&path)?
    } else {
        String::new()
    };
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e| anyhow::anyhow!("codex config.toml 파싱 실패 — 미변경: {e}"))?;
    codex_apply(&mut doc, db_path, proxy_bin);
    codex_write(&path, &doc.to_string())
}

/// codex config 문서에 hook을 병합한다(파일 I/O 없음 — 테스트 가능). 사용자 hook 보존.
fn codex_apply(doc: &mut toml_edit::DocumentMut, db_path: &std::path::Path, proxy_bin: &str) {
    // hook 기능 활성화(없으면 생성).
    doc["features"]["hooks"] = toml_edit::value(true);
    // [hooks]를 인라인 한 줄이 아닌 정식 테이블 섹션으로(사용자 config 가독성).
    if !doc.get("hooks").is_some_and(|h| h.is_table()) {
        let mut t = toml_edit::Table::new();
        t.set_implicit(false);
        doc.insert("hooks", toml_edit::Item::Table(t));
    }
    for (event, kind) in CODEX_EVENTS {
        let cmd = hook_command(proxy_bin, db_path, kind);
        // 기존 이벤트 배열에서 우리 옛 항목만 제거하고 우리 그룹을 추가한다 — 사용자가 같은
        // 이벤트에 넣은 hook을 덮어쓰지 않는다(codex High). (읽기는 .get — toml_edit의 []는
        // 없는 키에 panic.)
        let mut arr = doc
            .get("hooks")
            .and_then(|h| h.get(event))
            .and_then(|e| e.as_array())
            .cloned()
            .unwrap_or_default();
        arr.retain(|g| !codex_group_is_ours(g));
        arr.push(codex_group(&cmd));
        doc["hooks"][event] = toml_edit::Item::Value(toml_edit::Value::Array(arr));
    }
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

    #[test]
    fn codex_apply_preserves_user_hook_and_is_idempotent() {
        let user = "[hooks]\nPreToolUse = [{ hooks = [{ type = \"command\", command = \"user-tool.sh\" }] }]\n";
        let mut doc: toml_edit::DocumentMut = user.parse().unwrap();
        let db = std::path::Path::new("/db.sqlite3");

        codex_apply(&mut doc, db, "/proxy");
        let s = doc.to_string();
        assert!(s.contains("user-tool.sh"), "사용자 hook 보존"); // 안 지워짐
        assert!(s.contains("--deppy-hook"), "우리 hook 추가");
        assert!(s.contains("features"), "features.hooks 켜짐");

        // 두 번 적용해도 우리 것이 중복되지 않는다(이벤트당 1개).
        codex_apply(&mut doc, db, "/proxy");
        assert_eq!(
            doc.to_string().matches("--deppy-hook").count(),
            CODEX_EVENTS.len(),
            "우리 hook은 이벤트당 정확히 1개"
        );

        // 제거는 우리 것만 — 사용자 hook은 남는다.
        codex_remove(&mut doc);
        let s = doc.to_string();
        assert!(s.contains("user-tool.sh"), "제거 후에도 사용자 hook 유지");
        assert!(!s.contains("--deppy-hook"), "우리 hook 전부 제거");
    }

    #[test]
    fn codex_remove_cleans_up_when_no_user_hooks() {
        let mut doc: toml_edit::DocumentMut = String::new().parse().unwrap();
        codex_apply(&mut doc, std::path::Path::new("/db"), "/proxy");
        codex_remove(&mut doc);
        let s = doc.to_string();
        // 사용자 hook이 없으면 [hooks]/features.hooks까지 정리.
        assert!(!s.contains("--deppy-hook"));
        assert!(!s.contains("[hooks]"));
        assert!(!s.contains("hooks = true"));
    }
}
