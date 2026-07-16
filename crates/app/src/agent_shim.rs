//! cmux식 PATH shim — deppy 셸에서 `claude`/`codex`를 가로채 hook을 **매 실행 주입**한다.
//! 전역 config(~/.claude/settings.json, ~/.codex/config.toml)를 건드리지 않고, codex는
//! `--dangerously-bypass-hook-trust`를 함께 넘겨 trust 프롬프트가 아예 뜨지 않는다(우리가
//! 방금 주입한 우리 hook만 실행되므로 안전). 상주 프로세스 없음 — shim은 진짜 바이너리를
//! exec하는 몇 줄짜리 스크립트다.
//!
//! 경로: `~/.deppy-sijo/` (공백 없는 경로 — codex TOML/klaude settings의 command 인용 단순화).
//! 앱 시작마다 재생성한다(프록시/db 경로 변경 반영).

use std::path::PathBuf;

use crate::agent_hooks::{hook_command, statusline_command};

/// shim 루트 (`~/.deppy-sijo`).
fn root() -> Option<PathBuf> {
    Some(crate::paths::home_dir()?.join(".deppy-sijo"))
}

/// shim 디렉터리 경로 — 셸 PATH 맨 앞에 주입할 값. 설치 후에만 Some.
pub fn shim_dir() -> Option<PathBuf> {
    let d = root()?.join("shims");
    d.is_dir().then_some(d)
}

#[cfg(unix)]
fn write_executable(path: &std::path::Path, content: &str) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, content)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

/// shim/hook 스크립트/claude 설정을 (재)생성한다. 매 시작 호출 — idempotent.
pub fn install(db_path: &std::path::Path, proxy_bin: &str) -> anyhow::Result<()> {
    let Some(root) = root() else { return Ok(()) };
    // 경로에 작은따옴표가 있으면 shim의 sh/TOML 인용이 조용히 깨진다(codex 리뷰 실험:
    // O'Connor 홈에서 인자 mangling) — 설치 거부(극히 드묾, regex fallback 유지).
    anyhow::ensure!(
        !root.to_string_lossy().contains('\''),
        "shim 경로에 작은따옴표 포함 — hook shim 미설치"
    );
    let shims = root.join("shims");
    let hooks = root.join("hooks");
    std::fs::create_dir_all(&shims)?;
    std::fs::create_dir_all(&hooks)?;

    // ── hook 이벤트 스크립트 (codex TOML command로 공백 없는 경로가 필요) ──
    // (스크립트명, proxy --event 값)
    const EVENTS: &[(&str, &str)] = &[
        ("session-start", "session-start"),
        ("needs-input", "needs-input"),
        ("clear", "clear"),
        ("turn-done", "turn-done"),
    ];
    for (name, ev) in EVENTS {
        let script = format!("#!/bin/sh\nexec {}\n", hook_command(proxy_bin, db_path, ev));
        write_executable(&hooks.join(format!("deppy-hook-{name}.sh")), &script)?;
    }
    let hook = |name: &str| {
        hooks
            .join(format!("deppy-hook-{name}.sh"))
            .display()
            .to_string()
    };

    // ── claude: --settings 오버레이 파일 (사용자 settings.json 무변경) ──
    let claude_settings = serde_json::json!({
        "hooks": {
            "SessionStart":     [ { "hooks": [ { "type": "command", "command": hook("session-start") } ] } ],
            "Notification":     [ { "hooks": [ { "type": "command", "command": hook("needs-input") } ] } ],
            "UserPromptSubmit": [ { "hooks": [ { "type": "command", "command": hook("clear") } ] } ],
            "PreToolUse":       [ { "hooks": [ { "type": "command", "command": hook("clear") } ] } ],
            // Stop = 턴 완료 → 상태 레일 '완료' 트랜지언트 (clear가 아니라 turn-done).
            "Stop":             [ { "hooks": [ { "type": "command", "command": hook("turn-done") } ] } ],
        },
        // statusLine = effort/model/남은 context% 캡처 + 사용자 원래 statusLine 체이닝.
        "statusLine": { "type": "command", "command": statusline_command(proxy_bin, db_path) }
    });
    let settings_path = root.join("claude-hook-settings.json");
    std::fs::write(
        &settings_path,
        serde_json::to_string_pretty(&claude_settings)?,
    )?;

    // ── shim 공통 헤더: 자기 디렉터리를 PATH에서 빼고 진짜 바이너리로 exec ──
    let strip = r#"#!/bin/sh
# deppy shim (자동 생성) — hook 주입 후 진짜 바이너리로 교체 실행. 상주하지 않는다.
SELF_DIR="$(cd "$(dirname "$0")" && pwd)"
PATH="$(printf '%s' "$PATH" | tr ':' '\n' | grep -vxF "$SELF_DIR" | tr '\n' ':' | sed 's/:$//')"
export PATH
# PATH strip 실패(변형 경로 등) 시 자기 자신을 다시 exec하는 무한루프 방지 가드.
if [ -n "${DEPPY_SHIM_GUARD:-}" ]; then echo "deppy shim: real binary not found" >&2; exit 127; fi
export DEPPY_SHIM_GUARD=1
"#;

    // claude shim
    let claude_shim = format!(
        "{strip}exec claude --settings '{}' \"$@\"\n",
        settings_path.display()
    );
    write_executable(&shims.join("claude"), &claude_shim)?;

    // codex shim — cmux와 동일한 per-invocation hook 주입 + bypass(프롬프트 없음).
    let codex_events: &[(&str, &str, u32)] = &[
        ("SessionStart", "session-start", 10_000),
        ("PermissionRequest", "needs-input", 120_000),
        ("UserPromptSubmit", "clear", 10_000),
        ("PreToolUse", "clear", 10_000),
        // Stop = 턴 완료 → '완료' 트랜지언트.
        ("Stop", "turn-done", 10_000),
    ];
    let mut codex_args = String::from("--enable hooks --dangerously-bypass-hook-trust");
    for (event, name, timeout) in codex_events {
        codex_args.push_str(&format!(
            " -c 'hooks.{event}=[{{hooks=[{{type=\"command\",command='\\''{}'\\'',timeout={timeout}}}]}}]'",
            hook(name)
        ));
    }
    let codex_shim = format!("{strip}exec codex {codex_args} \"$@\"\n");
    write_executable(&shims.join("codex"), &codex_shim)?;
    Ok(())
}

/// shim 제거 (설정 토글 OFF) — PATH 주입도 앱이 함께 중단한다.
pub fn remove() -> anyhow::Result<()> {
    if let Some(root) = root()
        && root.exists()
    {
        std::fs::remove_dir_all(root)?;
    }
    Ok(())
}
