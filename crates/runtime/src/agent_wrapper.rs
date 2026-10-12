//! 에이전트를 실행하고 끝나면 셸로 넘어가는 래퍼(`/bin/sh -c <script> deppy-agent-session
//! <agent> <args…>`). 앱은 새 실행을 이 래퍼로 감싸고, runtime은 저장된 래퍼로 다시 실행할
//! 때 스크립트를 지금 판으로 바꾼다 — 예전 실행 인자가 DB에 남아 있어도 같은 동작을 한다.

/// 래퍼의 `$0` 자리 표식. 저장된 실행 인자에서 래퍼를 알아보는 기준이다.
pub const AGENT_SESSION_WRAPPER_NAME: &str = "deppy-agent-session";

/// 에이전트가 끝난 뒤 열리는 셸에만 적용할 셸 통합 env의 전달 이름. 에이전트 프로세스는 이
/// 이름을 읽지 않으므로 에이전트가 띄우는 하위 셸에는 Deppy zsh 통합이 끼지 않는다.
const POST_AGENT_SHELL_ENV: [(&str, &str); 2] = [
    ("ZDOTDIR", "DEPPY_SHELL_ZDOTDIR"),
    ("DEPPY_USER_ZDOTDIR", "DEPPY_SHELL_USER_ZDOTDIR"),
];

/// 일반 셸 pane과 같은 shim 우선 순위를 쓰도록 폴백 셸 PATH 앞에 둘 shim 디렉터리.
const SHIM_DIR_ENV: &str = "DEPPY_SHIM_DIR";

/// runtime이 에이전트 env에 넣는 폴백 셸 전달 값. 외부 env 예산(256개) 밖의 runtime 몫이고,
/// 부모 Deppy에서 상속된 값은 PTY spawn 때 지운다(`pty::INHERITED_AGENT_SESSION_VARS`).
pub(crate) const POST_AGENT_SHELL_ENV_NAMES: [&str; 3] = [
    "DEPPY_SHELL_ZDOTDIR",
    "DEPPY_SHELL_USER_ZDOTDIR",
    SHIM_DIR_ENV,
];

/// 에이전트가 끝나면 그 자리에 평범한 셸을 얹는 래퍼.
///
/// `stty sane` 뒤의 드레인 한 줄이 핵심이다. 에이전트 TUI는 터미널에 커서 위치(CPR
/// `ESC[2;1R`), 전경·배경색(OSC 10/11), 장치 속성(DA `ESC[?6c`)을 **질의**하고, 우리
/// 터미널이 그 **응답을 PTY 입력 쪽으로 되돌려 쓴다**. 에이전트가 응답을 읽기 전에
/// 끝나면 그 바이트가 입력 버퍼에 남아, 뒤이어 뜬 셸이 사용자가 타이핑한 것으로
/// 읽는다 — 실제로 `zsh: command not found: 1R10`처럼 깨진 명령이 실행됐다
/// (2026-08-19 사용자 보고).
///
/// `stty sane`은 **모드만 되돌릴 뿐 남은 입력을 버리지 않는다**. 그래서 비정규 모드로
/// 잠깐 두고(`min 0 time 1` = 최대 0.1초, 없으면 즉시 반환) 남은 바이트를 `dd`로 읽어
/// 버린다. 그동안 `-echo`라 그 바이트가 화면에 얼룩으로 찍히지도 않는다. 실측으로
/// 두 효과를 모두 확인했다(실제 CPR/OSC/DA 시퀀스를 PTY에 주입 → 잔여 입력 없음,
/// 화면 출력 없음). `stty`가 없거나 실패하면 `&&`로 건너뛰어 기존 동작 그대로다.
/// 남은 터미널 질의 응답을 버리는 조각. 스크립트와 테스트가 **같은 문자열**을 쓰도록
/// 상수로 둔다 — 한쪽만 고치면 회귀를 못 잡는다.
///
/// `time 0`(기다리지 않음)인 이유: 이 드레인은 **이미 버퍼에 있는 것만** 버려야 한다.
/// `time 1`(0.1초 대기)로 두면 그 사이에 사용자가 친 글자까지 함께 삼킨다 — 실측으로
/// 확인했다(에이전트 종료 50ms 뒤 타이핑: `time 1`은 삼킴, `time 0`은 살아남음).
/// 질의 응답은 우리 터미널이 질의를 파싱하는 즉시(= 에이전트가 살아 있을 때) 써 넣으므로
/// 이 시점엔 이미 버퍼에 있고, `time 0`으로도 그대로 걸린다(실측: 잔여 입력 없음).
#[cfg(unix)]
pub const DRAIN_PENDING_TTY_INPUT: &str =
    "stty -icanon -echo min 0 time 0 2>/dev/null && dd of=/dev/null bs=4096 count=1 2>/dev/null";

/// `"$@"` 직후 `$?`(에이전트의 진짜 종료 코드 — 뒤이어 붙는 폴백 셸의 것이 아니다)를
/// 잡아 sentinel 파일에 남긴다. 파일명은 이 래퍼 셸 자신의 PID(`$$`)로 결정되는데, 그
/// PID는 runtime이 `process_identity()`로 이미 알고 있는 값과 정확히 같은 프로세스를
/// 가리킨다(PTY가 직접 스폰하는 게 바로 이 `/bin/sh`) — 그래서 새 IPC 없이 파일 하나로
/// 만난다. 경로 계산은 `session::agent_exit_sentinel_path`와 이 접두사를 공유한다
/// (`DRAIN_PENDING_TTY_INPUT`과 같은 관례 — 한쪽만 고치면 서로 못 찾는다).
///
/// **화면에는 아무것도 안 남는다** — `printf`는 파일로만 쓰고, 실패해도(temp dir 없음
/// 등) `|| true`로 조용히 넘어가 기존 동작(폴백 셸 진입)을 막지 않는다. 세션 로그
/// redaction과도 무관하다 — PTY로 나가는 바이트가 아니라 파일 시스템 쓰기라 로그에
/// 찍힐 게 없다.
///
/// 폴백 셸은 에이전트 env를 물려받으므로 셸 통합(`ZDOTDIR`)을 `DEPPY_SHELL_*`에서 되살리고
/// shim을 PATH 앞에 둔 뒤 연다. 안 하면 그 셸에서 직접 친 `claude`/`codex`가 shim을 거치지
/// 않아 hook이 없다. zsh 통합은 사용자 rc 뒤에 shim을 다시 맨 앞으로 옮기고 중복을 지운다.
#[cfg(unix)]
pub fn agent_then_shell_script() -> String {
    let prefix = session::AGENT_EXIT_SENTINEL_PREFIX;
    format!(
        r#""$@"; __deppy_exit=$?; printf '%s' "$__deppy_exit" > "${{TMPDIR:-/tmp}}/{prefix}$$" 2>/dev/null || true; stty sane 2>/dev/null || true; {DRAIN_PENDING_TTY_INPUT}; stty sane 2>/dev/null || true; unset DEPPY_AGENT_EXECUTABLE DEPPY_SHIM_GUARD; if [ -n "${{DEPPY_SHELL_ZDOTDIR:-}}" ]; then ZDOTDIR="$DEPPY_SHELL_ZDOTDIR"; export ZDOTDIR; fi; if [ -n "${{DEPPY_SHELL_USER_ZDOTDIR:-}}" ]; then DEPPY_USER_ZDOTDIR="$DEPPY_SHELL_USER_ZDOTDIR"; export DEPPY_USER_ZDOTDIR; fi; unset DEPPY_SHELL_ZDOTDIR DEPPY_SHELL_USER_ZDOTDIR; if [ -n "${{DEPPY_SHIM_DIR:-}}" ] && [ -d "$DEPPY_SHIM_DIR" ]; then PATH="$DEPPY_SHIM_DIR:$PATH"; export PATH; fi; exec "${{SHELL:-/bin/sh}}""#
    )
}

/// 저장된 래퍼 실행 인자의 스크립트를 지금 판으로 바꾼다. 래퍼가 아니면 그대로 둔다.
#[cfg(unix)]
pub(crate) fn refresh_agent_wrapper(program: &str, args: &mut [String]) {
    if program == "/bin/sh"
        && let [flag, script, name, ..] = args
        && flag == "-c"
        && name == AGENT_SESSION_WRAPPER_NAME
    {
        *script = agent_then_shell_script();
    }
}

#[cfg(not(unix))]
pub(crate) fn refresh_agent_wrapper(_program: &str, _args: &mut [String]) {}

/// 셸 pane env 중 에이전트가 끝난 뒤의 폴백 셸에 넘길 값.
pub(crate) fn post_agent_shell_env(shell_env: &[(String, String)]) -> Vec<(String, String)> {
    shell_env
        .iter()
        .filter_map(|(key, value)| {
            let name = POST_AGENT_SHELL_ENV
                .iter()
                .find(|(shell_key, _)| shell_key == key)
                .map(|(_, carried)| *carried)
                .or_else(|| (key == SHIM_DIR_ENV).then_some(SHIM_DIR_ENV))?;
            Some((name.to_owned(), value.clone()))
        })
        .collect()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn 저장된_래퍼만_지금_스크립트로_바꾼다() {
        let mut stored = vec![
            "-c".to_owned(),
            "\"$@\"; exec \"${SHELL:-/bin/sh}\"".to_owned(),
            AGENT_SESSION_WRAPPER_NAME.to_owned(),
            "/Users/jr/.deppy-sijo/shims/claude".to_owned(),
            "--model".to_owned(),
        ];
        refresh_agent_wrapper("/bin/sh", &mut stored);
        assert_eq!(stored[1], agent_then_shell_script());
        assert_eq!(
            stored[3..],
            ["/Users/jr/.deppy-sijo/shims/claude", "--model"]
        );

        for (program, args) in [
            ("/bin/zsh", stored.clone()),
            ("/bin/sh", vec!["-c".to_owned(), "echo".to_owned()]),
            (
                "/bin/sh",
                vec!["-c".to_owned(), "echo".to_owned(), "other".to_owned()],
            ),
        ] {
            let mut unchanged = args.clone();
            unchanged[1] = "kept".to_owned();
            let before = unchanged.clone();
            refresh_agent_wrapper(program, &mut unchanged);
            assert_eq!(unchanged, before, "{program}");
        }
    }

    #[test]
    fn 폴백_셸_env는_셸_통합_값만_전달_이름으로_넘긴다() {
        let shell_env = [
            ("ZDOTDIR", "/h/.deppy-sijo/zdot"),
            ("DEPPY_USER_ZDOTDIR", "/h/custom"),
            ("DEPPY_SHIM_DIR", "/h/.deppy-sijo/shims"),
            ("PATH", "/h/.deppy-sijo/shims:/usr/bin"),
        ]
        .map(|(key, value)| (key.to_owned(), value.to_owned()));
        let carried = post_agent_shell_env(&shell_env);
        assert_eq!(
            carried,
            [
                ("DEPPY_SHELL_ZDOTDIR", "/h/.deppy-sijo/zdot"),
                ("DEPPY_SHELL_USER_ZDOTDIR", "/h/custom"),
                ("DEPPY_SHIM_DIR", "/h/.deppy-sijo/shims"),
            ]
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
        );
    }

    /// 실제 `/bin/sh`로 래퍼를 돌려 확인한다 — 에이전트에는 `ZDOTDIR`가 없고, 폴백 셸에는
    /// 되살아나며 shim이 PATH 맨 앞에 오고 전달 이름은 남지 않는다. 폴백 셸은 stdin으로
    /// 명령을 받는 `/bin/sh`라 실행 파일을 새로 쓰지 않는다(ETXTBSY 경합 없음).
    #[test]
    fn 폴백_셸만_셸_통합_env를_되살려_연다() {
        use std::io::Write as _;
        let dir = std::env::temp_dir().join(format!(
            "deppy-agent-wrapper-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let shims = dir.join("shims");
        std::fs::create_dir_all(&shims).unwrap();

        let mut child = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(agent_then_shell_script())
            .arg(AGENT_SESSION_WRAPPER_NAME)
            .arg("/bin/sh")
            .arg("-c")
            .arg("printf 'agent:%s|' \"${ZDOTDIR:-none}\"")
            .env_remove("ZDOTDIR")
            .env_remove("DEPPY_USER_ZDOTDIR")
            .env("DEPPY_SHELL_ZDOTDIR", "/h/.deppy-sijo/zdot")
            .env("DEPPY_SHELL_USER_ZDOTDIR", "/h/custom")
            .env("DEPPY_SHIM_DIR", &shims)
            .env("PATH", "/usr/bin:/bin")
            .env("SHELL", "/bin/sh")
            .env("TMPDIR", &dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(
                b"printf 'shell:%s:%s:%s:%s:%s' \"${ZDOTDIR:-none}\" \"${DEPPY_USER_ZDOTDIR:-none}\" \"${DEPPY_SHELL_ZDOTDIR:-gone}\" \"${DEPPY_SHELL_USER_ZDOTDIR:-gone}\" \"$PATH\"; exit\n",
            )
            .unwrap();
        let output = child.wait_with_output().unwrap();
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            format!(
                "agent:none|shell:/h/.deppy-sijo/zdot:/h/custom:gone:gone:{}:/usr/bin:/bin",
                shims.display()
            )
        );
    }
}
