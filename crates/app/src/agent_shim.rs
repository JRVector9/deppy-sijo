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
    let d = shim_path()?;
    d.is_dir().then_some(d)
}

/// 설치 여부와 무관한 shim 경로 — detector가 자기 자신을 실제 CLI로 오인하지 않게 한다.
pub fn shim_path() -> Option<PathBuf> {
    Some(root()?.join("shims"))
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
        "{strip}REAL_AGENT=\"${{DEPPY_AGENT_EXECUTABLE:-claude}}\"\nunset DEPPY_AGENT_EXECUTABLE\nexec \"$REAL_AGENT\" --settings '{}' \"$@\"\n",
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
    let codex_shim = format!(
        "{strip}REAL_AGENT=\"${{DEPPY_AGENT_EXECUTABLE:-codex}}\"\nunset DEPPY_AGENT_EXECUTABLE\nexec \"$REAL_AGENT\" {codex_args} \"$@\"\n"
    );
    write_executable(&shims.join("codex"), &codex_shim)?;

    // ── kimi: 런치별 주입 수단이 없어 전역 config를 고친다(install_kimi_hooks 주석) ──
    // 실패해도 설치 전체를 되돌리지 않는다 — claude/codex shim은 이미 유효하고,
    // Kimi hook이 없으면 Kimi 카드가 상태 신호를 못 받을 뿐 나머지는 멀쩡하다.
    if let Err(error) = install_kimi_hooks(&hooks) {
        tracing::warn!("kimi hook 설치 실패(다른 에이전트는 정상): {error:#}");
    }
    Ok(())
}

/// shim 제거 (설정 토글 OFF) — PATH 주입도 앱이 함께 중단한다.
pub fn remove() -> anyhow::Result<()> {
    // 우리 디렉터리를 지우기 **전에** 남의 config에서 우리 흔적을 걷어낸다.
    if let Err(error) = remove_kimi_hooks() {
        tracing::warn!("kimi hook 제거 실패: {error:#}");
    }
    if let Some(root) = root()
        && root.exists()
    {
        std::fs::remove_dir_all(root)?;
    }
    Ok(())
}

/// Kimi config 경로 — 없으면 Kimi 미사용.
fn kimi_config() -> Option<PathBuf> {
    Some(crate::paths::home_dir()?.join(".kimi-code/config.toml"))
}

/// Kimi 전역 config에 deppy hook을 심는다 — **이 모듈에서 유일하게 전역 파일을 고친다.**
///
/// claude/codex는 런치마다 hook을 주입할 수단이 있어(`--settings`, `-c`) 전역 config를
/// 건드리지 않는다. Kimi CLI(0.34.0)에는 그 수단이 없다 — `--help` 전체를 확인했고
/// `-c`/`--config`/`--settings`가 없다. `KIMI_CODE_HOME`은 홈 **전체**를 옮기는 것이라
/// credentials·sessions까지 딸려가 hook만 갈아끼우는 용도로 쓸 수 없다.
///
/// 그래서 `~/.kimi-code/config.toml`을 고치되 셋을 지킨다:
/// 1. **우리 것만 만진다** — command에 우리 hooks 경로가 든 항목만 걷어내고 다시 넣는다.
/// 2. **없으면 만들지 않는다** — 파일이 없으면 Kimi를 안 쓰는 사용자다.
/// 3. **deppy가 사라져도 무해하다** — command가 스크립트 존재를 먼저 확인한다.
///
/// hook 본체는 `DEPPY_SESSION_ID`가 없으면 즉시 no-op이라, deppy 밖에서 띄운 Kimi
/// 세션에는 아무 영향이 없다.
fn install_kimi_hooks(hooks_dir: &std::path::Path) -> anyhow::Result<()> {
    let Some(config) = kimi_config() else {
        return Ok(());
    };
    let Ok(text) = std::fs::read_to_string(&config) else {
        return Ok(());
    };
    let updated = apply_kimi_hooks(&text, hooks_dir, true)?;
    if updated != text {
        deppy_core::fs::atomic_write(&config, updated.as_bytes())?;
    }
    Ok(())
}

/// Kimi 전역 config에서 deppy hook을 걷어낸다(shim 토글 OFF).
///
/// 스크립트가 사라지면 command가 조용히 no-op이라 남겨둬도 해롭진 않지만, 우리가 넣은
/// 것을 우리가 치우지 않으면 사용자 config에 죽은 항목이 쌓인다.
fn remove_kimi_hooks() -> anyhow::Result<()> {
    let (Some(root), Some(config)) = (root(), kimi_config()) else {
        return Ok(());
    };
    let Ok(text) = std::fs::read_to_string(&config) else {
        return Ok(());
    };
    let updated = apply_kimi_hooks(&text, &root.join("hooks"), false)?;
    if updated != text {
        deppy_core::fs::atomic_write(&config, updated.as_bytes())?;
    }
    Ok(())
}

/// config 본문에서 deppy hook 항목을 걷어내고, `install`이면 다시 넣는다.
///
/// 파일 IO 없이 문자열만 다루는 순수 변환이라 사용자 config를 실제로 건드리지 않고
/// 계약을 테스트할 수 있다. `toml_edit`을 쓰는 이유는 주석·서식·키 순서 보존이다.
fn apply_kimi_hooks(
    text: &str,
    hooks_dir: &std::path::Path,
    install: bool,
) -> anyhow::Result<String> {
    let mut doc = text.parse::<toml_edit::DocumentMut>()?;
    let marker = hooks_dir.display().to_string();
    let table = doc
        .entry("hooks")
        .or_insert(toml_edit::Item::ArrayOfTables(Default::default()));
    let Some(entries) = table.as_array_of_tables_mut() else {
        anyhow::bail!("kimi config.toml의 hooks가 array-of-tables가 아니다 — 건드리지 않는다");
    };
    entries.retain(|t| {
        !t.get("command")
            .and_then(|c| c.as_str())
            .is_some_and(|c| c.contains(&marker))
    });
    if install {
        for (event, script) in kimi_hook_entries(hooks_dir) {
            let mut t = toml_edit::Table::new();
            t["event"] = toml_edit::value(event.as_str());
            t["command"] = toml_edit::value(script.as_str());
            t["timeout"] = toml_edit::value(10_i64);
            entries.push(t);
        }
    }
    // 우리 것만 있던 config는 제거 후 빈 배열만 남는다 — 흔적을 남기지 않는다.
    if entries.is_empty() {
        doc.remove("hooks");
    }
    Ok(doc.to_string())
}

/// Kimi 이벤트 → deppy hook 스크립트. claude/codex 매핑과 같은 의미로 맞춘다.
///
/// `PermissionRequest`가 claude의 `Notification`(= 사람을 막는 순간)에 해당한다.
/// `StopFailure`는 넣지 않는다 — 실패로 끝난 턴을 '완료'로 칠하면 오류가 묻힌다.
fn kimi_hook_entries(hooks_dir: &std::path::Path) -> Vec<(String, String)> {
    const EVENTS: &[(&str, &str)] = &[
        ("UserPromptSubmit", "clear"),
        ("PreToolUse", "clear"),
        ("PermissionRequest", "needs-input"),
        ("Stop", "turn-done"),
    ];
    EVENTS
        .iter()
        .map(|(event, name)| {
            let script = hooks_dir.join(format!("deppy-hook-{name}.sh"));
            let script = script.display();
            (
                (*event).to_owned(),
                format!("if [ -x '{script}' ]; then exec '{script}'; fi"),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// 사용자 config를 통째로 바꾸는 코드라 계약이 셋이다: 우리 것만 건드릴 것,
    /// 두 번 돌려도 늘지 않을 것, 우리가 사라져도 Kimi가 실패하지 않을 것.
    /// 하나라도 깨지면 사용자의 다른 도구나 Kimi 자체가 망가진다.
    #[test]
    fn kimi_hook_설치는_남의_항목을_보존하고_두번_돌려도_늘지_않는다() {
        let hooks = Path::new("/Users/x/.deppy-sijo/hooks");
        let original = r#"default_model = "k3"

# 사용자 주석 — 보존돼야 한다
[thinking]
enabled = true

[[hooks]]
event = "Stop"
command = "/Users/x/.orca/agent-hooks/kimi-hook.sh"
timeout = 10
"#;

        let once = apply_kimi_hooks(original, hooks, true).unwrap();
        assert!(
            once.contains("# 사용자 주석 — 보존돼야 한다") && once.contains("default_model"),
            "toml_edit을 쓰는 이유가 주석·서식 보존이다"
        );
        assert!(
            once.contains("/Users/x/.orca/agent-hooks/kimi-hook.sh"),
            "다른 도구가 심은 hook을 지우면 그 도구가 조용히 망가진다"
        );
        // command 하나가 스크립트를 두 번 언급한다(존재 가드 + exec) — 4항목 × 2.
        assert_eq!(once.matches("deppy-hook-").count(), 8, "넷을 심는다");
        for event in [
            "UserPromptSubmit",
            "PreToolUse",
            "PermissionRequest",
            "Stop",
        ] {
            assert!(once.contains(event), "{event} 항목이 빠졌다");
        }
        assert!(
            once.contains("deppy-hook-needs-input.sh"),
            "사람을 막는 순간(PermissionRequest)이 needs-input으로 가야 히어로 큐에 선다"
        );
        assert!(
            !once.contains("StopFailure"),
            "실패로 끝난 턴을 '완료'로 칠하면 오류가 묻힌다"
        );

        // 앱은 시작마다 install을 부른다 — 두 번째 호출이 항목을 늘리면 안 된다.
        let twice = apply_kimi_hooks(&once, hooks, true).unwrap();
        assert_eq!(
            twice, once,
            "idempotent가 아니면 실행할 때마다 hook이 쌓인다"
        );

        // 토글 OFF: 우리 것만 사라지고 남의 것은 남는다.
        let removed = apply_kimi_hooks(&twice, hooks, false).unwrap();
        assert!(
            !removed.contains("deppy-hook-"),
            "우리가 넣은 것을 우리가 치우지 않으면 죽은 항목이 쌓인다"
        );
        assert!(
            removed.contains("/Users/x/.orca/agent-hooks/kimi-hook.sh"),
            "제거가 남의 hook까지 가져가면 안 된다"
        );
    }

    /// deppy를 지워도 Kimi가 매 이벤트마다 실패하면 안 된다 — command가 스크립트
    /// 존재를 먼저 확인하고, 없으면 조용히 성공해야 한다.
    #[test]
    fn kimi_hook_command는_스크립트가_없으면_조용히_성공한다() {
        let entries = kimi_hook_entries(Path::new("/nonexistent/hooks"));
        assert_eq!(entries.len(), 4);
        // 문자열 모양만 보면 놓친다 — 실제로 sh가 0으로 끝나는지 확인한다.
        for (event, command) in &entries {
            let status = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(command)
                .status()
                .expect("sh 실행");
            assert!(
                status.success(),
                "{event}: 스크립트 부재 시 0으로 끝나야 한다"
            );
        }
    }

    /// 제거 후에는 원본과 완전히 같아야 한다 — 빈 `[[hooks]]` 배열도 남기지 않는다.
    #[test]
    fn hooks가_없던_config에도_흔적을_남기지_않는다() {
        let hooks = Path::new("/Users/x/.deppy-sijo/hooks");
        let plain = "default_model = \"k3\"\n";
        let installed = apply_kimi_hooks(plain, hooks, true).unwrap();
        assert!(installed.contains("deppy-hook-"), "설치는 돼야 한다");
        let removed = apply_kimi_hooks(&installed, hooks, false).unwrap();
        assert_eq!(removed, plain, "제거 후에는 원본과 같아야 한다");
    }
}
