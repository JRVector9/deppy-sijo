//! .env 라이브 반영 zsh 훅 (E5 ⑨, 옵트인 — 2026-07-13).
//!
//! 프로세스 환경변수는 spawn 시점에 고정되므로, .env를 나중에 고쳐도 이미 떠 있는
//! 셸에는 반영되지 않는다. 이 모듈은 VS Code shell-integration과 같은 **ZDOTDIR
//! 주입** 방식으로 deppy 셸(zsh)에 precmd 훅을 심어, 다음 프롬프트마다 프로젝트
//! `.env`/`.env.local`의 mtime을 확인하고 바뀌었으면 `set -a; source`로 현재 셸에
//! 다시 export한다 — 세션 재시작 없이 다음 명령부터 최신 env가 적용된다.
//!
//! 안전 장치:
//! - 훅은 `DEPPY_ENV_LIVE_RELOAD=1` + `DEPPY_PROJECT_ROOT` 가 있을 때만 활성 —
//!   두 값은 세션 기본 env(SetSessionDefaultEnv)로 주입되므로 설정 토글이 꺼지면
//!   새 셸부터 훅이 잠든다(파일은 남지만 no-op).
//! - 사용자 zsh 설정(.zshenv/.zprofile/.zshrc/.zlogin)은 전부 그대로 통과(source)
//!   시키고, .zshrc 끝에서 ZDOTDIR를 사용자 값으로 되돌려 중첩 zsh는 재주입되지
//!   않는다.
//! - deppy가 띄우는 셸에만 적용된다(ZDOTDIR는 spawn env로만 주입) — 사용자의
//!   일반 터미널은 건드리지 않는다.

use std::path::PathBuf;

/// 훅 루트 (`~/.deppy-sijo/zdot`).
fn zdot_root() -> Option<PathBuf> {
    Some(crate::paths::home_dir()?.join(".deppy-sijo/zdot"))
}

const ZSHENV: &str = r#"# deppy-sijo env-live-reload bootstrap — 사용자 zsh 설정을 그대로 통과시킨다.
if [[ -z "$DEPPY_ZDOTDIR" ]]; then
  export DEPPY_ZDOTDIR="$ZDOTDIR"
fi
ZDOTDIR="${DEPPY_USER_ZDOTDIR:-$HOME}"
if [[ -f "$ZDOTDIR/.zshenv" ]]; then
  builtin source "$ZDOTDIR/.zshenv"
fi
# 사용자 .zshenv가 ZDOTDIR를 바꿨으면 이후 단계의 사용자 디렉터리로 존중한다.
export DEPPY_USER_ZDOTDIR="$ZDOTDIR"
ZDOTDIR="$DEPPY_ZDOTDIR"
"#;

const ZPROFILE: &str = r#"# deppy-sijo passthrough — 사용자 .zprofile 실행.
__deppy_saved_zdotdir="$ZDOTDIR"
ZDOTDIR="${DEPPY_USER_ZDOTDIR:-$HOME}"
if [[ -f "$ZDOTDIR/.zprofile" ]]; then
  builtin source "$ZDOTDIR/.zprofile"
fi
ZDOTDIR="$__deppy_saved_zdotdir"
unset __deppy_saved_zdotdir
"#;

const ZLOGIN: &str = r#"# deppy-sijo passthrough — 사용자 .zlogin 실행.
__deppy_saved_zdotdir="$ZDOTDIR"
ZDOTDIR="${DEPPY_USER_ZDOTDIR:-$HOME}"
if [[ -f "$ZDOTDIR/.zlogin" ]]; then
  builtin source "$ZDOTDIR/.zlogin"
fi
ZDOTDIR="$__deppy_saved_zdotdir"
unset __deppy_saved_zdotdir
"#;

const ZSHRC: &str = r#"# deppy-sijo — 사용자 .zshrc 통과 후 env 라이브 반영 훅 등록.
ZDOTDIR="${DEPPY_USER_ZDOTDIR:-$HOME}"
if [[ -f "$ZDOTDIR/.zshrc" ]]; then
  builtin source "$ZDOTDIR/.zshrc"
fi
# 여기서부터 ZDOTDIR는 사용자 값 — 중첩 zsh는 재주입되지 않는다.

# ── deppy env live-reload (옵트인 — 설정 › 환경) ──────────────────────────
if [[ "$DEPPY_ENV_LIVE_RELOAD" == "1" && -n "$DEPPY_PROJECT_ROOT" ]]; then
  zmodload -F zsh/stat b:zstat 2>/dev/null
  typeset -g __deppy_env_sig=""
  __deppy_env_reload() {
    local f m sig=""
    for f in "$DEPPY_PROJECT_ROOT/.env" "$DEPPY_PROJECT_ROOT/.env.local"; do
      if [[ -f "$f" ]]; then
        m="$(zstat +mtime "$f" 2>/dev/null)" || m="$(command stat -f %m "$f" 2>/dev/null)" || m=""
        sig+="$f:$m;"
      fi
    done
    [[ "$sig" == "$__deppy_env_sig" ]] && return
    __deppy_env_sig="$sig"
    local rc_f
    for rc_f in "$DEPPY_PROJECT_ROOT/.env" "$DEPPY_PROJECT_ROOT/.env.local"; do
      if [[ -f "$rc_f" ]]; then
        set -a
        builtin source "$rc_f" 2>/dev/null
        set +a
      fi
    done
  }
  autoload -Uz add-zsh-hook 2>/dev/null
  if (( $+functions[add-zsh-hook] )); then
    add-zsh-hook precmd __deppy_env_reload
  else
    typeset -ga precmd_functions
    precmd_functions+=(__deppy_env_reload)
  fi
  __deppy_env_reload
fi
"#;

/// 훅 파일 4개를 (재)생성하고 zdot 디렉터리를 돌려준다. 매 시작 호출 — idempotent.
/// 내용이 같으면 다시 쓰지 않는다(mtime churn 방지).
pub fn ensure_hook_files() -> anyhow::Result<Option<PathBuf>> {
    let Some(root) = zdot_root() else {
        return Ok(None);
    };
    std::fs::create_dir_all(&root)?;
    for (name, content) in [
        (".zshenv", ZSHENV),
        (".zprofile", ZPROFILE),
        (".zlogin", ZLOGIN),
        (".zshrc", ZSHRC),
    ] {
        let path = root.join(name);
        if std::fs::read_to_string(&path).ok().as_deref() != Some(content) {
            std::fs::write(&path, content)?;
        }
    }
    Ok(Some(root))
}

/// 기본 셸이 zsh면 spawn env에 넣을 (key, value)들. 래퍼는 **항상** 주입한다 —
/// 순수 passthrough라 기능 OFF면 no-op이고(훅은 DEPPY_ENV_LIVE_RELOAD=1일 때만
/// 활성), 이렇게 해야 설정 토글이 워커 재생성 없이 다음 새 셸부터 동작한다.
/// 활성 조건(DEPPY_ENV_LIVE_RELOAD/DEPPY_PROJECT_ROOT)은 세션 기본 env가 나른다.
pub fn shell_env() -> Vec<(String, String)> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_owned());
    if !shell.trim_end_matches('/').ends_with("zsh") {
        return Vec::new(); // zsh 전용 — bash 등은 후속
    }
    let root = match ensure_hook_files() {
        Ok(Some(root)) => root,
        Ok(None) => return Vec::new(),
        Err(e) => {
            tracing::warn!("env 라이브 반영 훅 설치 실패 — 비활성: {e:#}");
            return Vec::new();
        }
    };
    let mut env = vec![("ZDOTDIR".to_owned(), root.display().to_string())];
    // 앱 자신이 ZDOTDIR 환경에서 떴다면 사용자 원본을 넘겨 통과 경로를 보존한다.
    if let Ok(user) = std::env::var("ZDOTDIR") {
        env.push(("DEPPY_USER_ZDOTDIR".to_owned(), user));
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 훅 스크립트 4개가 zsh 문법 검사(zsh -n)를 통과한다 — 셸 기동 실패로
    /// 사용자 터미널이 깨지는 최악을 CI에서 막는다. zsh 없는 환경은 skip.
    #[test]
    fn 훅_스크립트는_zsh_문법_검사를_통과한다() {
        if !std::path::Path::new("/bin/zsh").exists() {
            return;
        }
        let dir = std::env::temp_dir().join(format!(
            "deppy-zdot-syntax-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, content) in [
            ("zshenv", ZSHENV),
            ("zprofile", ZPROFILE),
            ("zlogin", ZLOGIN),
            ("zshrc", ZSHRC),
        ] {
            let path = dir.join(name);
            std::fs::write(&path, content).unwrap();
            let out = std::process::Command::new("/bin/zsh")
                .arg("-n")
                .arg(&path)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{name} 문법 오류: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 훅이 실제로 .env 변경을 현재 셸에 반영한다 — 대화형 zsh을 흉내내
    /// .zshrc 로드 → 프롬프트(precmd) 2회 사이에 .env를 바꿔 값 변화를 관찰한다.
    #[test]
    fn 훅은_env_변경을_다음_프롬프트에_반영한다() {
        if !std::path::Path::new("/bin/zsh").exists() {
            return;
        }
        let dir = std::env::temp_dir().join(format!(
            "deppy-zdot-live-{}-{}",
            std::process::id(),
            line!()
        ));
        let project = dir.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(dir.join(".zshrc"), ZSHRC).unwrap();
        std::fs::write(project.join(".env"), "KORAIL_ID=first\n").unwrap();
        // 비대화형이라 precmd는 자동으로 안 돈다 — 훅 함수를 직접 두 번 호출해
        // "프롬프트 두 번 사이 변경"을 시뮬레이션한다. mtime 초 단위 변화를 위해
        // 과거 mtime을 강제한다(touch -t).
        let script = format!(
            r#"
export DEPPY_ENV_LIVE_RELOAD=1
export DEPPY_PROJECT_ROOT={project}
export DEPPY_USER_ZDOTDIR={dir}
builtin source {dir}/.zshrc
echo "1:$KORAIL_ID"
command touch -t 202001010000 {project}/.env.stamp 2>/dev/null
printf 'KORAIL_ID=second\n' > {project}/.env
command touch -t 202601010000 {project}/.env
__deppy_env_reload
echo "2:$KORAIL_ID"
"#,
            project = project.display(),
            dir = dir.display(),
        );
        let out = std::process::Command::new("/bin/zsh")
            .arg("-c")
            .arg(&script)
            .env_remove("ZDOTDIR")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("1:first"), "초기 로드 실패: {stdout}");
        assert!(
            stdout.contains("2:second"),
            "변경 미반영: {stdout} / {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
