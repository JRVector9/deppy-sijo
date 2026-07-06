//! 에이전트 상태 hook 전역 설치 (옵션2 needsInput). claude/codex의 hook 설정에
//! deppy-mcp-proxy 호출을 등록해, 승인·입력 대기(needsInput)를 에이전트가 직접 보고하게
//! 한다. 손타이핑·절대경로·alias 등 실행 방법과 무관하게 잡힌다(전역 설치).
//!
//! - 바인딩: 셸 spawn 시 주입한 `DEPPY_SESSION_ID` env를 hook이 그대로 물려받아 세션 식별.
//! - 보고: hook 커맨드 = `'<proxy>' hooks --db '<db>' --event <needs-input|clear> --deppy-hook`.
//!   `--deppy-hook`은 우리 항목을 식별하는 마커(uninstall 시 이것만 제거).
//! - 안전: 기존 설정을 병합(clobber 안 함), 원자적 쓰기(tmp+rename), 파싱 실패 시 미변경.
//!
//! 현재 claude(`~/.claude/settings.json`, JSON)만. codex(config.toml)는 포맷 보존 편집이
//! 필요해 후속.

use std::path::PathBuf;

use serde_json::{Value, json};

/// 우리 hook 항목을 식별하는 커맨드 마커.
const MARKER: &str = "--deppy-hook";

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
        let cmd = format!(
            "'{}' hooks --db '{}' --event {} {}",
            proxy_bin,
            db_path.display(),
            kind,
            MARKER
        );
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
