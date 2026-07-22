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

use std::io::Read as _;
use std::path::{Path, PathBuf};

/// Generated zsh wrappers are all below 8 KiB; this leaves headroom without admitting bulk input.
const HOOK_FILE_BYTES_MAX: usize = 16 * 1024;
fn open_hook_file_read_only(path: &Path) -> std::io::Result<std::fs::File> {
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

fn hook_file_matches(path: &Path, expected: &[u8], max_bytes: usize) -> anyhow::Result<bool> {
    let before = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(_) => anyhow::bail!("env_hook_metadata_failed"),
    };
    anyhow::ensure!(
        before.file_type().is_file() && !before.file_type().is_symlink(),
        "env_hook_file_type_invalid"
    );
    if before.len() > max_bytes as u64 {
        return Ok(false);
    }
    let mut file =
        open_hook_file_read_only(path).map_err(|_| anyhow::anyhow!("env_hook_open_failed"))?;
    let opened = file
        .metadata()
        .map_err(|_| anyhow::anyhow!("env_hook_metadata_failed"))?;
    anyhow::ensure!(opened.is_file(), "env_hook_file_type_invalid");
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        anyhow::ensure!(
            before.dev() == opened.dev() && before.ino() == opened.ino(),
            "env_hook_file_changed"
        );
    }
    if opened.len() > max_bytes as u64 {
        return Ok(false);
    }
    let probe = max_bytes
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("env_hook_bytes_exceeded"))?;
    let mut bytes = Vec::with_capacity((opened.len() as usize).min(probe));
    file.by_ref()
        .take(probe as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow::anyhow!("env_hook_read_failed"))?;
    anyhow::ensure!(bytes.len() <= max_bytes, "env_hook_bytes_exceeded");
    let after = file
        .metadata()
        .map_err(|_| anyhow::anyhow!("env_hook_metadata_failed"))?;
    anyhow::ensure!(after.len() == bytes.len() as u64, "env_hook_file_changed");
    Ok(bytes == expected)
}

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

# ── deppy prompt marks (OSC 133 — 셸 통합 1단계. zsh 전용, bash/fish는 후속) ──
# 세션(워커)이 출력 스트림에서 133;A를 스캔해 프롬프트 점프(⌘⇧↑/↓)에 쓴다.
# 이미 다른 셸 통합(iTerm2 등)이 등록한 훅이 OSC 133을 쏘면 중복 마크를 피해
# 설치하지 않는다. env-reload 훅보다 먼저 등록해 D의 $? 오염을 줄인다.
if [[ "$(builtin typeset -f ${precmd_functions[@]:-} ${preexec_functions[@]:-} precmd preexec 2>/dev/null)" != *'133;'* ]]; then
  __deppy_prompt_precmd() {
    # 직전 명령 종료(D;exit code) 후 프롬프트 시작(A). D는 2단계(출력 추출)용.
    builtin printf '\e]133;D;%s\a\e]133;A\a' $?
  }
  __deppy_prompt_preexec() {
    # 명령 실행 — 출력 시작(C).
    builtin printf '\e]133;C\a'
  }
  autoload -Uz add-zsh-hook 2>/dev/null
  if (( $+functions[add-zsh-hook] )); then
    add-zsh-hook precmd __deppy_prompt_precmd
    add-zsh-hook preexec __deppy_prompt_preexec
  else
    typeset -ga precmd_functions preexec_functions
    precmd_functions+=(__deppy_prompt_precmd)
    preexec_functions+=(__deppy_prompt_preexec)
  fi
fi

# ── deppy env live-reload (옵트인 — 설정 › 환경) ──────────────────────────
if [[ "$DEPPY_ENV_LIVE_RELOAD" == "1" && -n "$DEPPY_PROJECT_ROOT" ]]; then
  # 둘 다 builtin 모듈이다. 어느 쪽이든 없으면 외부 stat/cat/dd로 폴백하지 않고 비활성화한다.
  if zmodload -F zsh/stat b:zstat 2>/dev/null &&
     zmodload -F zsh/system b:sysopen b:sysread 2>/dev/null; then
    typeset -g __deppy_env_sig=""
    typeset -g __deppy_env_capture=""
    typeset -gi __deppy_env_capture_bytes=0

    # nofollow로 고정한 regular-file descriptor를 최대 limit+1까지 builtin sysread한다.
    # source path를 다시 열지 않고 이 bounded snapshot만 eval하므로 검사 뒤 파일이 커져도
    # 셸이 1MiB를 넘는 dotenv를 읽거나 실행하지 않는다.
    __deppy_env_capture_bounded() {
      local capture_path="$1" chunk=""
      local -i capture_limit="$2" fd=-1 count=0 read_status=0 total=0
      local -A file_stat
      __deppy_env_capture=""
      __deppy_env_capture_bytes=0
      sysopen -r -o nofollow,cloexec -u fd -- "$capture_path" 2>/dev/null || return 1
      if ! zstat -f "$fd" -H file_stat 2>/dev/null ||
         (( (file_stat[mode] & 61440) != 32768 || file_stat[size] > capture_limit )); then
        exec {fd}<&-
        return 1
      fi
      while true; do
        chunk=""
        count=0
        sysread -i "$fd" -s 65536 -c count chunk 2>/dev/null
        read_status=$?
        (( read_status == 5 )) && break
        if (( read_status != 0 || total + count > capture_limit )); then
          __deppy_env_capture=""
          exec {fd}<&-
          return 1
        fi
        __deppy_env_capture+="$chunk"
        (( total += count ))
      done
      file_stat=()
      if ! zstat -f "$fd" -H file_stat 2>/dev/null || (( file_stat[size] != total )); then
        __deppy_env_capture=""
        exec {fd}<&-
        return 1
      fi
      exec {fd}<&-
      __deppy_env_capture_bytes=$total
      return 0
    }

    __deppy_env_reload() {
      local -a env_files env_present env_contents
      env_files=("$DEPPY_PROJECT_ROOT/.env" "$DEPPY_PROJECT_ROOT/.env.local")
      env_present=()
      env_contents=()
      local f m s content sig=""
      local -A path_stat
      local -i valid=1 total=0 remaining=1048576 index=0

      # unchanged prompt는 metadata builtin 두 번뿐이다. 특수파일/symlink/합산 초과는
      # signature만 기억하고 두 파일 모두 실행하지 않는다(부분 적용 금지).
      for f in "${env_files[@]}"; do
        if [[ ! -e "$f" && ! -L "$f" ]]; then
          env_present+=(0)
          sig+="missing;"
          continue
        fi
        env_present+=(1)
        path_stat=()
        if [[ -L "$f" ]] || ! zstat -H path_stat "$f" 2>/dev/null ||
           (( (path_stat[mode] & 61440) != 32768 )); then
          valid=0
          sig+="invalid;"
          continue
        fi
        s="$path_stat[size]"
        m="$path_stat[mtime]"
        if [[ "$s" != <-> || "$m" != <-> ]] || (( s > 1048576 - total )); then
          valid=0
          sig+="invalid;"
          continue
        fi
        (( total += s ))
        sig+="$f:$m:$s;"
      done
      [[ "$sig" == "$__deppy_env_sig" ]] && return
      # 안정적으로 invalid인 metadata는 다음 prompt마다 같은 검사를 반복하지 않는다.
      # 반면 valid metadata의 descriptor capture/apply 실패는 signature를 commit하지 않아
      # mtime/size가 그대로여도 다음 prompt에서 반드시 재시도한다.
      if (( ! valid )); then
        __deppy_env_sig="$sig"
        return
      fi

      for (( index = 1; index <= ${#env_files}; index++ )); do
        if (( env_present[index] )); then
          if ! __deppy_env_capture_bounded "${env_files[index]}" "$remaining"; then
            __deppy_env_capture=""
            env_contents=()
            return
          fi
          (( remaining -= __deppy_env_capture_bytes ))
          env_contents+=("$__deppy_env_capture")
          __deppy_env_capture=""
          __deppy_env_capture_bytes=0
        else
          env_contents+=("")
        fi
      done

      # 배열 순서가 .env → .env.local 우선순위를 보존한다. eval 대상은 위에서 확보한
      # bounded descriptor snapshot뿐이며 path를 다시 source하지 않는다.
      for content in "${env_contents[@]}"; do
        [[ -z "$content" ]] && continue
        set -a
        if ! builtin eval -- "$content" 2>/dev/null; then
          set +a
          env_contents=()
          return
        fi
        set +a
      done
      env_contents=()
      __deppy_env_sig="$sig"
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
fi
"#;

/// 훅 파일 4개를 (재)생성하고 zdot 디렉터리를 돌려준다. 매 시작 호출 — idempotent.
/// 내용이 같으면 다시 쓰지 않는다(mtime churn 방지).
pub fn ensure_hook_files() -> anyhow::Result<Option<PathBuf>> {
    let Some(root) = zdot_root() else {
        return Ok(None);
    };
    std::fs::create_dir_all(&root).map_err(|_| anyhow::anyhow!("env_hook_root_create_failed"))?;
    let root_metadata = std::fs::symlink_metadata(&root)
        .map_err(|_| anyhow::anyhow!("env_hook_root_metadata_failed"))?;
    anyhow::ensure!(
        root_metadata.file_type().is_dir() && !root_metadata.file_type().is_symlink(),
        "env_hook_root_type_invalid"
    );
    for (name, content) in [
        (".zshenv", ZSHENV),
        (".zprofile", ZPROFILE),
        (".zlogin", ZLOGIN),
        (".zshrc", ZSHRC),
    ] {
        let path = root.join(name);
        if !hook_file_matches(&path, content.as_bytes(), HOOK_FILE_BYTES_MAX)? {
            anyhow::ensure!(
                content.len() <= HOOK_FILE_BYTES_MAX,
                "env_hook_bytes_exceeded"
            );
            deppy_core::fs::atomic_write(&path, content.as_bytes())
                .map_err(|_| anyhow::anyhow!("env_hook_write_failed"))?;
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
        Err(_) => {
            tracing::warn!(
                kind = "env_hook",
                phase = "install",
                error_code = "env_hook_install_failed",
                "env live reload hook unavailable"
            );
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

    /// Must match runtime::dotenv::DOTENV_TOTAL_BYTES_MAX and the generated zsh literal.
    const ENV_LIVE_RELOAD_BYTES_MAX: usize = 1024 * 1024;

    fn temp_path(tag: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "deppy-env-hook-bound-{tag}-{}-{nonce}",
            std::process::id()
        ))
    }

    fn padded_dotenv(assignment: &str, total_bytes: usize) -> Vec<u8> {
        assert!(assignment.len() <= total_bytes);
        let mut bytes = assignment.as_bytes().to_vec();
        bytes.resize(total_bytes, b'#');
        bytes
    }

    fn run_live_reload(zshrc: &Path, user_dir: &Path, project: &Path) -> std::process::Output {
        let script = format!(
            r#"
export DEPPY_ENV_LIVE_RELOAD=1
export DEPPY_PROJECT_ROOT={project}
export DEPPY_USER_ZDOTDIR={user}
export DEPPY_BOUND=sentinel
builtin source {zshrc}
echo "value:$DEPPY_BOUND"
"#,
            project = project.display(),
            user = user_dir.display(),
            zshrc = zshrc.display(),
        );
        std::process::Command::new("/bin/zsh")
            .arg("-c")
            .arg(script)
            .env_remove("ZDOTDIR")
            .output()
            .unwrap()
    }

    #[test]
    fn generated_hook_reader_is_bounded_and_byte_exact() {
        let path = temp_path("bytes");
        std::fs::write(&path, vec![b'x'; 64]).unwrap();
        assert!(hook_file_matches(&path, &[b'x'; 64], 64).unwrap());
        std::fs::write(&path, vec![b'x'; 65]).unwrap();
        assert!(!hook_file_matches(&path, &[b'x'; 64], 64).unwrap());
        std::fs::write(&path, [0xff]).unwrap();
        assert!(!hook_file_matches(&path, b"expected", 64).unwrap());
        let _ = std::fs::remove_file(path);
    }

    #[cfg(unix)]
    #[test]
    fn generated_hook_reader_rejects_symlink_and_special_file() {
        use std::os::unix::fs::symlink;

        let dir = temp_path("types");
        std::fs::create_dir_all(&dir).unwrap();
        let regular = dir.join("regular");
        let link = dir.join("link");
        std::fs::write(&regular, b"ok").unwrap();
        symlink(&regular, &link).unwrap();
        assert_eq!(
            hook_file_matches(&link, b"ok", 64).unwrap_err().to_string(),
            "env_hook_file_type_invalid"
        );
        assert_eq!(
            hook_file_matches(Path::new("/dev/null"), b"", 64)
                .unwrap_err()
                .to_string(),
            "env_hook_file_type_invalid"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn production_hook_install_has_no_unbounded_whole_file_read() {
        let production = include_str!("env_reload.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        assert!(!production.contains("read_to_string"));
        assert!(!production.contains("std::fs::read("));
        assert!(production.contains(".take(probe as u64)"));
        assert!(ZSHRC.contains("sysopen -r -o nofollow,cloexec"));
        assert!(ZSHRC.contains("sysread -i \"$fd\" -s 65536"));
        assert!(ZSHRC.contains(&ENV_LIVE_RELOAD_BYTES_MAX.to_string()));
        assert!(!ZSHRC.contains("command stat"));
        assert!(!ZSHRC.contains("command cat"));
        assert!(!ZSHRC.contains("command dd"));
        assert!(!ZSHRC.contains("$(zstat"));
        assert!(!ZSHRC.contains("builtin source \"$rc_f\""));
        let capture_start = ZSHRC.find("for (( index = 1;").unwrap();
        let apply_start = ZSHRC.find("# 배열 순서가 .env").unwrap();
        assert!(!ZSHRC[capture_start..apply_start].contains("__deppy_env_sig=\"$sig\""));
        let final_commit = ZSHRC.rfind("__deppy_env_sig=\"$sig\"").unwrap();
        let eval = ZSHRC.rfind("builtin eval -- \"$content\"").unwrap();
        assert!(final_commit > eval);
    }

    #[test]
    fn capture_일시실패는_동일metadata_다음_reload에서_재시도한다() {
        if !std::path::Path::new("/bin/zsh").exists() {
            return;
        }
        let dir = temp_path("live-retry");
        let zdot = dir.join("zdot");
        let user = dir.join("user");
        let project = dir.join("project");
        std::fs::create_dir_all(&zdot).unwrap();
        std::fs::create_dir_all(&user).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        let zshrc = zdot.join(".zshrc");
        std::fs::write(&zshrc, ZSHRC).unwrap();

        let script = format!(
            r#"
export DEPPY_ENV_LIVE_RELOAD=1
export DEPPY_PROJECT_ROOT={project}
export DEPPY_USER_ZDOTDIR={user}
export DEPPY_BOUND=sentinel
builtin source {zshrc}
echo "initial:$DEPPY_BOUND"
typeset __deppy_sig_before="$__deppy_env_sig"
builtin printf 'DEPPY_BOUND=updated\n' > {project}/.env
functions[__deppy_env_capture_real]="${{functions[__deppy_env_capture_bounded]}}"
typeset -gi __deppy_fail_once=1
__deppy_env_capture_bounded() {{
  if (( __deppy_fail_once )); then
    (( __deppy_fail_once = 0 ))
    return 1
  fi
  __deppy_env_capture_real "$@"
}}
__deppy_env_reload
echo "first:$DEPPY_BOUND"
if [[ "$__deppy_env_sig" == "$__deppy_sig_before" ]]; then
  echo "pending:yes"
else
  echo "pending:no"
fi
__deppy_env_reload
echo "second:$DEPPY_BOUND"
echo "failures_left:$__deppy_fail_once"
"#,
            project = project.display(),
            user = user.display(),
            zshrc = zshrc.display(),
        );
        let output = std::process::Command::new("/bin/zsh")
            .arg("-c")
            .arg(script)
            .env_remove("ZDOTDIR")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("initial:sentinel"), "{stdout}");
        assert!(stdout.contains("first:sentinel"), "{stdout}");
        assert!(stdout.contains("pending:yes"), "{stdout}");
        assert!(stdout.contains("second:updated"), "{stdout}");
        assert!(stdout.contains("failures_left:0"), "{stdout}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn live_reload_accepts_exact_aggregate_and_rejects_plus_one() {
        if !std::path::Path::new("/bin/zsh").exists() {
            return;
        }
        let dir = temp_path("live-limit");
        let zdot = dir.join("zdot");
        let user = dir.join("user");
        let project = dir.join("project");
        std::fs::create_dir_all(&zdot).unwrap();
        std::fs::create_dir_all(&user).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        let zshrc = zdot.join(".zshrc");
        std::fs::write(&zshrc, ZSHRC).unwrap();

        let half = ENV_LIVE_RELOAD_BYTES_MAX / 2;
        std::fs::write(
            project.join(".env"),
            padded_dotenv("DEPPY_BOUND=base\n", half),
        )
        .unwrap();
        std::fs::write(
            project.join(".env.local"),
            padded_dotenv("DEPPY_BOUND=local\n", half),
        )
        .unwrap();
        let exact = run_live_reload(&zshrc, &user, &project);
        assert!(
            exact.status.success(),
            "{}",
            String::from_utf8_lossy(&exact.stderr)
        );
        assert!(
            String::from_utf8_lossy(&exact.stdout).contains("value:local"),
            "{}",
            String::from_utf8_lossy(&exact.stdout)
        );

        std::fs::write(
            project.join(".env.local"),
            padded_dotenv("DEPPY_BOUND=oversized\n", half + 1),
        )
        .unwrap();
        let oversized = run_live_reload(&zshrc, &user, &project);
        assert!(
            oversized.status.success(),
            "{}",
            String::from_utf8_lossy(&oversized.stderr)
        );
        assert!(
            String::from_utf8_lossy(&oversized.stdout).contains("value:sentinel"),
            "{}",
            String::from_utf8_lossy(&oversized.stdout)
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn live_reload_rejects_symlink_and_special_input_without_partial_apply() {
        use std::os::unix::fs::symlink;

        if !std::path::Path::new("/bin/zsh").exists() {
            return;
        }
        let dir = temp_path("live-types");
        let zdot = dir.join("zdot");
        let user = dir.join("user");
        let project = dir.join("project");
        std::fs::create_dir_all(&zdot).unwrap();
        std::fs::create_dir_all(&user).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        let zshrc = zdot.join(".zshrc");
        std::fs::write(&zshrc, ZSHRC).unwrap();
        let target = dir.join("outside.env");
        std::fs::write(&target, "DEPPY_BOUND=symlink\n").unwrap();
        symlink(&target, project.join(".env")).unwrap();
        std::fs::create_dir(project.join(".env.local")).unwrap();

        let output = run_live_reload(&zshrc, &user, &project);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("value:sentinel"),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn disabled_live_reload_installs_no_reader_function() {
        if !std::path::Path::new("/bin/zsh").exists() {
            return;
        }
        let dir = temp_path("live-disabled");
        let zdot = dir.join("zdot");
        let user = dir.join("user");
        std::fs::create_dir_all(&zdot).unwrap();
        std::fs::create_dir_all(&user).unwrap();
        let zshrc = zdot.join(".zshrc");
        std::fs::write(&zshrc, ZSHRC).unwrap();
        let script = format!(
            "export DEPPY_ENV_LIVE_RELOAD=0\nexport DEPPY_USER_ZDOTDIR={}\n\
             builtin source {}\necho installed:$+functions[__deppy_env_reload]:$+functions[__deppy_env_capture_bounded]\n",
            user.display(),
            zshrc.display()
        );
        let output = std::process::Command::new("/bin/zsh")
            .arg("-c")
            .arg(script)
            .env_remove("ZDOTDIR")
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "installed:0:0"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

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

    /// OSC 133 프롬프트 마크 훅(셸 통합 1단계): precmd가 D(종료 코드)+A, preexec가
    /// C를 찍는다 — 세션 스캐너(session crate)가 A를 마크로 저장하는 계약의 셸 쪽 절반.
    #[test]
    fn 프롬프트_마크_훅은_osc_133을_찍는다() {
        if !std::path::Path::new("/bin/zsh").exists() {
            return;
        }
        let dir = std::env::temp_dir().join(format!(
            "deppy-zdot-osc133-{}-{}",
            std::process::id(),
            line!()
        ));
        let zdot = dir.join("zdot");
        let user = dir.join("user");
        std::fs::create_dir_all(&zdot).unwrap();
        std::fs::create_dir_all(&user).unwrap();
        std::fs::write(zdot.join(".zshrc"), ZSHRC).unwrap();
        // 비대화형이라 훅이 자동으로 안 돈다 — 프롬프트 흐름을 직접 흉내낸다.
        let script = format!(
            r#"
export DEPPY_USER_ZDOTDIR={user}
builtin source {zdot}/.zshrc
echo "installed:$+functions[__deppy_prompt_precmd]"
false
__deppy_prompt_precmd
__deppy_prompt_preexec
"#,
            zdot = zdot.display(),
            user = user.display(),
        );
        let out = std::process::Command::new("/bin/zsh")
            .arg("-c")
            .arg(&script)
            .env_remove("ZDOTDIR")
            .output()
            .unwrap();
        let stdout = out.stdout;
        let text = String::from_utf8_lossy(&stdout);
        assert!(text.contains("installed:1"), "훅 미설치: {text}");
        // false 직후 precmd → D;1 + A, preexec → C
        let expect_da: &[u8] = b"\x1b]133;D;1\x07\x1b]133;A\x07";
        assert!(
            stdout.windows(expect_da.len()).any(|w| w == expect_da),
            "D+A 마크 없음: {text:?}"
        );
        let expect_c: &[u8] = b"\x1b]133;C\x07";
        assert!(
            stdout.windows(expect_c.len()).any(|w| w == expect_c),
            "C 마크 없음: {text:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 사용자 셸 통합(iTerm 등)이 이미 OSC 133을 쏘면 중복 마크를 피해 설치하지 않는다.
    #[test]
    fn 기존_133_훅이_있으면_프롬프트_마크_훅을_설치하지_않는다() {
        if !std::path::Path::new("/bin/zsh").exists() {
            return;
        }
        let dir = std::env::temp_dir().join(format!(
            "deppy-zdot-osc133-guard-{}-{}",
            std::process::id(),
            line!()
        ));
        let zdot = dir.join("zdot");
        let user = dir.join("user");
        std::fs::create_dir_all(&zdot).unwrap();
        std::fs::create_dir_all(&user).unwrap();
        std::fs::write(zdot.join(".zshrc"), ZSHRC).unwrap();
        std::fs::write(
            user.join(".zshrc"),
            "__user_integration_precmd() { printf '\\e]133;A\\a'; }\n\
             typeset -ga precmd_functions\n\
             precmd_functions+=(__user_integration_precmd)\n",
        )
        .unwrap();
        let script = format!(
            r#"
export DEPPY_USER_ZDOTDIR={user}
builtin source {zdot}/.zshrc
echo "installed:$+functions[__deppy_prompt_precmd]"
"#,
            zdot = zdot.display(),
            user = user.display(),
        );
        let out = std::process::Command::new("/bin/zsh")
            .arg("-c")
            .arg(&script)
            .env_remove("ZDOTDIR")
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            text.contains("installed:0"),
            "기존 133 훅에도 설치됨: {text} / {}",
            String::from_utf8_lossy(&out.stderr)
        );
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
        let zdot = dir.join("zdot");
        let user = dir.join("user");
        let project = dir.join("proj");
        std::fs::create_dir_all(&zdot).unwrap();
        std::fs::create_dir_all(&user).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(zdot.join(".zshrc"), ZSHRC).unwrap();
        std::fs::write(project.join(".env"), "KORAIL_ID=first\n").unwrap();
        // 비대화형이라 precmd는 자동으로 안 돈다 — 훅 함수를 직접 두 번 호출해
        // "프롬프트 두 번 사이 변경"을 시뮬레이션한다. mtime 초 단위 변화를 위해
        // 과거 mtime을 강제한다(touch -t).
        let script = format!(
            r#"
export DEPPY_ENV_LIVE_RELOAD=1
export DEPPY_PROJECT_ROOT={project}
export DEPPY_USER_ZDOTDIR={user}
builtin source {zdot}/.zshrc
echo "1:$KORAIL_ID"
command touch -t 202001010000 {project}/.env.stamp 2>/dev/null
printf 'KORAIL_ID=second\n' > {project}/.env
command touch -t 202601010000 {project}/.env
__deppy_env_reload
echo "2:$KORAIL_ID"
"#,
            project = project.display(),
            zdot = zdot.display(),
            user = user.display(),
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
