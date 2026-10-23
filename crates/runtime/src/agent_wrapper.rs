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

/// Discard only input already buffered at agent exit; waiting here would swallow later user
/// keystrokes. A PID-specific, invisible output boundary also retires the old query owner:
/// the PTY reader observes it ahead of a delayed Session pump. The existing exit sentinel
/// retires that same owner even under reader backpressure, and queued protocol responses carry
/// the owner through the writer. Queries after the marker retain normal shell replies.
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
/// 종료 stamp가 regular/owned 파일로 작성되지 않으면 invisible marker 뒤 원래 agent
/// 종료 코드로 끝낸다. reader backpressure 때 독립 소유권 근거 없이 폴백 셸을 열면 옛
/// 터미널 질의 응답이 새 셸 입력으로 들어갈 수 있기 때문이다. 세션 로그
/// redaction과도 무관하다 — PTY로 나가는 바이트가 아니라 파일 시스템 쓰기라 로그에
/// 찍힐 게 없다.
///
/// 폴백 셸은 에이전트 env를 물려받으므로 셸 통합(`ZDOTDIR`)을 `DEPPY_SHELL_*`에서 되살리고
/// shim을 PATH 앞에 둔 뒤 연다. 안 하면 그 셸에서 직접 친 `claude`/`codex`가 shim을 거치지
/// 않아 hook이 없다. zsh 통합은 사용자 rc 뒤에 shim을 다시 맨 앞으로 옮기고 중복을 지운다.
#[cfg(unix)]
pub fn agent_then_shell_script() -> String {
    let prefix = session::AGENT_EXIT_SENTINEL_PREFIX;
    let fence_prefix = pty::query_fence::AGENT_QUERY_FENCE_PREFIX.replace("\x1b", "\\033");
    format!(
        r#""$@"; __deppy_exit=$?; __deppy_stamp="${{TMPDIR:-/tmp}}/{prefix}$$"; __deppy_stamp_safe=0; if [ ! -e "$__deppy_stamp" ] && [ ! -L "$__deppy_stamp" ]; then __deppy_stamp_safe=1; elif [ -f "$__deppy_stamp" ] && [ ! -L "$__deppy_stamp" ] && [ -O "$__deppy_stamp" ]; then __deppy_stamp_safe=1; fi; __deppy_stamp_ok=0; if [ "$__deppy_stamp_safe" -eq 1 ] && printf '%s' "$__deppy_exit" 2>/dev/null > "$__deppy_stamp"; then __deppy_stamp_ok=1; fi; if [ -t 1 ]; then printf '{fence_prefix}%s\007' "$$"; fi; if [ "$__deppy_stamp_ok" -ne 1 ]; then exit "$__deppy_exit"; fi; stty sane 2>/dev/null || true; {DRAIN_PENDING_TTY_INPUT}; stty sane 2>/dev/null || true; unset DEPPY_AGENT_EXECUTABLE DEPPY_SHIM_GUARD DEPPY_PROJECT_API_CAPABILITY; if [ -n "${{DEPPY_SHELL_ZDOTDIR:-}}" ]; then ZDOTDIR="$DEPPY_SHELL_ZDOTDIR"; export ZDOTDIR; fi; if [ -n "${{DEPPY_SHELL_USER_ZDOTDIR:-}}" ]; then DEPPY_USER_ZDOTDIR="$DEPPY_SHELL_USER_ZDOTDIR"; export DEPPY_USER_ZDOTDIR; fi; unset DEPPY_SHELL_ZDOTDIR DEPPY_SHELL_USER_ZDOTDIR; if [ -n "${{DEPPY_SHIM_DIR:-}}" ] && [ -d "$DEPPY_SHIM_DIR" ]; then PATH="$DEPPY_SHIM_DIR:$PATH"; export PATH; fi; exec "${{SHELL:-/bin/sh}}""#
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

    #[test]
    fn project_api_capability_reaches_agent_but_is_absent_from_fallback_shell() {
        use std::io::Write as _;
        let directory = std::env::temp_dir().join(format!(
            "deppy-agent-capability-boundary-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&directory).unwrap();
        let mut child = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(agent_then_shell_script())
            .arg(AGENT_SESSION_WRAPPER_NAME)
            .arg("/bin/sh")
            .arg("-c")
            .arg("printf 'agent:%s|' \"${DEPPY_PROJECT_API_CAPABILITY:+present}\"")
            .env_clear()
            .env(
                "DEPPY_PROJECT_API_CAPABILITY",
                "synthetic-session-capability",
            )
            .env("PATH", "/usr/bin:/bin")
            .env("SHELL", "/bin/sh")
            .env("TMPDIR", &directory)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"printf 'shell:%s' \"${DEPPY_PROJECT_API_CAPABILITY-gone}\"; exit\n")
            .unwrap();
        let output = child.wait_with_output().unwrap();
        std::fs::remove_dir_all(&directory).unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "agent:present|shell:gone",
            "fallback shell must not inherit the finished agent's API bearer"
        );
    }

    #[cfg(unix)]
    #[test]
    fn full_review_20261014_late_agent_query_cannot_reach_fallback_shell() {
        use std::os::unix::fs::PermissionsExt;
        let dir =
            std::env::temp_dir().join(format!("deppy-late-query-audit-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let fallback = dir.join("fallback-shell");
        let ready = dir.join("fallback-ready");
        let script = format!(
            "#!/bin/sh\nstty -echo -icanon min 1 time 0\nprintf READY\n: > '{}'\ndd bs=1 count=6 2>/dev/null | od -An -tx1\nsleep 30\n",
            ready.display()
        );
        std::fs::write(&fallback, script).unwrap();
        std::fs::set_permissions(&fallback, std::fs::Permissions::from_mode(0o700)).unwrap();
        let command = pty::CommandSpec {
            program: "/bin/sh".to_owned(),
            args: vec![
                "-c".to_owned(),
                crate::agent_wrapper::agent_then_shell_script(),
                crate::agent_wrapper::AGENT_SESSION_WRAPPER_NAME.to_owned(),
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                "printf '\\033[6n'".to_owned(),
            ],
            env: vec![
                ("SHELL".to_owned(), fallback.display().to_string()),
                ("TMPDIR".to_owned(), dir.display().to_string()),
            ],
            cwd: Some(dir.clone()),
        };
        let mut live = session::Session::spawn_with_spec(
            deppy_core::SessionId(1),
            session::SessionKind::Agent,
            &command,
            80,
            24,
            100,
        )
        .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !ready.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            ready.exists(),
            "agent must be gone and fallback raw reader active before parser feed"
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        let mut output = Vec::new();
        while std::time::Instant::now() < deadline {
            live.pump(|bytes| output.extend_from_slice(bytes));
            if String::from_utf8_lossy(&output)
                .split_whitespace()
                .collect::<Vec<_>>()
                .windows(6)
                .any(|bytes| bytes == ["1b", "5b", "31", "3b", "31", "52"])
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        println!("late_query_output={:?}", String::from_utf8_lossy(&output));
        let received_late_query_response = String::from_utf8_lossy(&output)
            .split_whitespace()
            .collect::<Vec<_>>()
            .windows(6)
            .any(|bytes| bytes == ["1b", "5b", "31", "3b", "31", "52"]);
        drop(live);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            !received_late_query_response,
            "fallback shell received late CPR after production wrapper's time-0 drain: {:?}",
            String::from_utf8_lossy(&output)
        );
    }

    fn raw_query_fixture(
        agent: &str,
        fallback_body: &str,
    ) -> (std::path::PathBuf, session::Session, std::path::PathBuf) {
        raw_query_fixture_with_wake(agent, fallback_body, None)
    }

    fn raw_query_fixture_with_wake(
        agent: &str,
        fallback_body: &str,
        wake: Option<pty::PtyOutputWake>,
    ) -> (std::path::PathBuf, session::Session, std::path::PathBuf) {
        raw_query_fixture_inner(agent, fallback_body, wake, true)
    }

    fn raw_query_fixture_inner(
        agent: &str,
        fallback_body: &str,
        wake: Option<pty::PtyOutputWake>,
        writable_tmpdir: bool,
    ) -> (std::path::PathBuf, session::Session, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("deppy-query-owners-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let ready = dir.join("ready");
        let fallback = dir.join("fallback");
        std::fs::write(
            &fallback,
            format!("#!/bin/sh\nstty -echo -icanon min 1 time 0\n{fallback_body}\n"),
        )
        .unwrap();
        std::fs::set_permissions(&fallback, std::fs::Permissions::from_mode(0o700)).unwrap();
        let spec = pty::CommandSpec {
            program: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                agent_then_shell_script(),
                AGENT_SESSION_WRAPPER_NAME.into(),
                "/bin/sh".into(),
                "-c".into(),
                agent.into(),
            ],
            env: vec![
                ("SHELL".into(), fallback.display().to_string()),
                (
                    "TMPDIR".into(),
                    if writable_tmpdir {
                        dir.clone()
                    } else {
                        dir.join("missing-tmpdir")
                    }
                    .display()
                    .to_string(),
                ),
                ("FIXTURE_READY".into(), ready.display().to_string()),
                (
                    "FIXTURE_ALLOW_EXIT".into(),
                    dir.join("allow-exit").display().to_string(),
                ),
                (
                    "FIXTURE_RESULT".into(),
                    dir.join("result").display().to_string(),
                ),
            ],
            cwd: Some(dir.clone()),
        };
        let live = match wake {
            Some(wake) => session::Session::spawn_with_spec_and_output_wake(
                deppy_core::SessionId(1),
                session::SessionKind::Agent,
                &spec,
                80,
                24,
                100,
                wake,
            ),
            None => session::Session::spawn_with_spec(
                deppy_core::SessionId(1),
                session::SessionKind::Agent,
                &spec,
                80,
                24,
                100,
            ),
        }
        .unwrap();
        (dir, live, ready)
    }

    fn wait_fixture_ready(ready: &std::path::Path) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !ready.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(ready.exists(), "fixture must enter its raw reader");
    }

    fn pump_until(live: &mut session::Session, needle: &str) -> String {
        let mut output = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            live.pump(|bytes| output.extend_from_slice(bytes));
            if String::from_utf8_lossy(&output).contains(needle) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        String::from_utf8_lossy(&output).into_owned()
    }

    #[test]
    fn full_fix_lifecycle_fallback_query_retains_only_its_own_response() {
        let (dir, mut live, ready) = raw_query_fixture(
            "printf '\\033[6n'",
            r#"printf '\033[6n'; : > "$FIXTURE_READY"; dd bs=1 count=6 2>/dev/null | od -An -tx1; stty min 0 time 1; dd bs=4096 count=1 2>/dev/null | od -An -tx1; printf QUERY_DONE; sleep 30"#,
        );
        wait_fixture_ready(&ready);
        let output = pump_until(&mut live, "QUERY_DONE");
        drop(live);
        std::fs::remove_dir_all(dir).unwrap();
        let tokens: Vec<_> = output.split_whitespace().collect();
        assert_eq!(
            tokens
                .windows(6)
                .filter(|bytes| *bytes == ["1b", "5b", "31", "3b", "31", "52"])
                .count(),
            1,
            "{output:?}"
        );
        assert!(
            output.contains("QUERY_DONE"),
            "new shell must receive its response: {output:?}"
        );
    }

    #[test]
    fn full_fix_lifecycle_input_fifty_ms_after_exit_survives_delayed_pump() {
        let (dir, mut live, ready) = raw_query_fixture(
            "printf '\\033[6n'",
            r#": > "$FIXTURE_READY"; dd bs=1 count=1 2>/dev/null | od -An -tx1; printf INPUT_DONE; sleep 30"#,
        );
        wait_fixture_ready(&ready);
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(live.write_input(b"K").unwrap().is_accepted());
        let output = pump_until(&mut live, "INPUT_DONE");
        drop(live);
        std::fs::remove_dir_all(dir).unwrap();
        assert!(
            output.split_whitespace().any(|byte| byte == "4b"),
            "{output:?}"
        );
        assert!(output.contains("INPUT_DONE"), "{output:?}");
    }

    #[test]
    fn full_fix_lifecycle_live_new_agent_query_still_receives_response() {
        let (dir, mut live, ready) = raw_query_fixture(
            r#"stty -echo -icanon min 1 time 0; printf '\033[6n'; : > "$FIXTURE_READY"; dd bs=1 count=6 2>/dev/null | od -An -tx1; printf AGENT_QUERY_DONE; sleep 30"#,
            "sleep 30",
        );
        wait_fixture_ready(&ready);
        let output = pump_until(&mut live, "AGENT_QUERY_DONE");
        drop(live);
        std::fs::remove_dir_all(dir).unwrap();
        let tokens: Vec<_> = output.split_whitespace().collect();
        assert!(
            tokens
                .windows(6)
                .any(|bytes| bytes == ["1b", "5b", "31", "3b", "31", "52"]),
            "{output:?}"
        );
        assert!(output.contains("AGENT_QUERY_DONE"), "{output:?}");
    }

    #[test]
    fn full_fix_lifecycle_failed_exit_stamp_cannot_launch_a_fallback_owner() {
        let (dir, mut live, ready) = raw_query_fixture_inner(
            r#"printf '\033[6n'; exit 23"#,
            r#": > "$FIXTURE_READY"; dd bs=1 count=6 > "$FIXTURE_RESULT" 2>/dev/null; printf FALLBACK_RESPONSE; sleep 30"#,
            None,
            false,
        );
        let mut output = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            live.pump(|bytes| output.extend_from_slice(bytes));
            if ready.exists()
                || matches!(live.lifecycle(), session::SessionLifecycle::Exited { .. })
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let fallback_started = ready.exists();
        let lifecycle = live.lifecycle();
        let received = std::fs::read(dir.join("result")).unwrap_or_default();
        drop(live);
        std::fs::remove_dir_all(dir).unwrap();
        assert!(
            !fallback_started,
            "failed exit stamp created an untracked fallback owner"
        );
        assert_eq!(
            lifecycle,
            session::SessionLifecycle::Exited {
                exit_code: Some(23)
            }
        );
        assert!(
            received.is_empty(),
            "fallback received a stale query response: {received:02x?}"
        );
    }

    #[test]
    fn full_fix_lifecycle_nonregular_exit_stamp_closes_without_opening_a_new_owner() {
        use std::os::unix::ffi::OsStrExt as _;
        for kind in ["symlink", "fifo"] {
            let (dir, mut live, ready) = raw_query_fixture(
                r#"while [ ! -f "$FIXTURE_ALLOW_EXIT" ]; do sleep 0.005; done; printf '\033[6n'; exit 23"#,
                r#": > "$FIXTURE_READY"; dd bs=1 count=6 > "$FIXTURE_RESULT" 2>/dev/null; sleep 30"#,
            );
            let stamp =
                session::agent_exit_sentinel_path(&dir, live.process_identity().pid.unwrap());
            let protected = dir.join("protected");
            std::fs::write(&protected, b"untouched").unwrap();
            if kind == "symlink" {
                std::os::unix::fs::symlink(&protected, &stamp).unwrap();
            } else {
                let name = std::ffi::CString::new(stamp.as_os_str().as_bytes()).unwrap();
                assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
            }
            std::fs::write(dir.join("allow-exit"), b"").unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while std::time::Instant::now() < deadline {
                live.pump(|_| {});
                if ready.exists()
                    || matches!(live.lifecycle(), session::SessionLifecycle::Exited { .. })
                {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            let fallback_started = ready.exists();
            let lifecycle = live.lifecycle();
            let received = std::fs::read(dir.join("result")).unwrap_or_default();
            let protected_bytes = std::fs::read(&protected).unwrap();
            drop(live);
            std::fs::remove_dir_all(dir).unwrap();
            assert!(
                !fallback_started,
                "{kind} stamp created an untracked fallback owner"
            );
            assert_eq!(
                lifecycle,
                session::SessionLifecycle::Exited {
                    exit_code: Some(23)
                },
                "{kind}"
            );
            assert!(
                received.is_empty(),
                "{kind} fallback received stale CPR: {received:02x?}"
            );
            assert_eq!(
                protected_bytes, b"untouched",
                "{kind} stamp changed another file"
            );
        }
    }

    #[test]
    fn full_fix_lifecycle_status_consuming_current_stamp_does_not_close_fallback() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir =
            std::env::temp_dir().join(format!("deppy-stamp-consume-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let consumed = dir.join("consumed");
        let ready = dir.join("ready");
        let allow = dir.join("allow-exit");
        let expected = dir.join("expected-stamp");
        let printf_shim = dir.join("printf");
        std::fs::write(
            &printf_shim,
            r#"#!/bin/sh
/usr/bin/printf "$@" || exit 1
stamp=$(cat "$FIXTURE_EXPECTED_STAMP")
if [ -n "$stamp" ] && [ "$stamp" = "$TMPDIR/deppy-agent-exit-$PPID" ] && [ -f "$stamp" ]; then
    rm -f "$stamp" || exit 1
    : > "$FIXTURE_CONSUMED"
fi
"#,
        )
        .unwrap();
        std::fs::set_permissions(&printf_shim, std::fs::Permissions::from_mode(0o700)).unwrap();
        let fallback = dir.join("fallback");
        std::fs::write(
            &fallback,
            r#"#!/bin/sh
: > "$FIXTURE_READY"
sleep 30
"#,
        )
        .unwrap();
        std::fs::set_permissions(&fallback, std::fs::Permissions::from_mode(0o700)).unwrap();
        // Invoke only the stamp printf through an owned PATH shim. It writes valid bytes,
        // then models the status consumer unlinking that exact file before printf returns.
        let script = agent_then_shell_script().replace(
            r#"printf '%s' "$__deppy_exit""#,
            r#"env printf '%s' "$__deppy_exit""#,
        );
        let spec = pty::CommandSpec {
            program: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                script,
                AGENT_SESSION_WRAPPER_NAME.into(),
                "/bin/sh".into(),
                "-c".into(),
                r#"while [ ! -f "$FIXTURE_ALLOW_EXIT" ]; do sleep 0.005; done; exit 23"#.into(),
            ],
            env: vec![
                ("TMPDIR".into(), dir.display().to_string()),
                ("SHELL".into(), fallback.display().to_string()),
                ("PATH".into(), format!("{}:/usr/bin:/bin", dir.display())),
                (
                    "FIXTURE_EXPECTED_STAMP".into(),
                    expected.display().to_string(),
                ),
                ("FIXTURE_CONSUMED".into(), consumed.display().to_string()),
                ("FIXTURE_READY".into(), ready.display().to_string()),
                ("FIXTURE_ALLOW_EXIT".into(), allow.display().to_string()),
            ],
            cwd: Some(dir.clone()),
        };
        let mut live = session::Session::spawn_with_spec(
            deppy_core::SessionId(1),
            session::SessionKind::Agent,
            &spec,
            80,
            24,
            100,
        )
        .unwrap();
        let stamp = session::agent_exit_sentinel_path(&dir, live.process_identity().pid.unwrap());
        std::fs::write(&expected, stamp.as_os_str().as_encoded_bytes()).unwrap();
        std::fs::write(&allow, b"").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !ready.exists() && std::time::Instant::now() < deadline {
            live.pump(|_| {});
            if matches!(live.lifecycle(), session::SessionLifecycle::Exited { .. }) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let qualified = consumed.exists();
        let fallback_started = ready.exists();
        drop(live);
        std::fs::remove_dir_all(dir).unwrap();
        assert!(
            qualified,
            "fixture must consume only its valid current exit stamp"
        );
        assert!(
            fallback_started,
            "status consumption was mistaken for stamp write failure"
        );
    }

    #[test]
    fn full_fix_lifecycle_backpressured_reader_cannot_send_old_query_to_fallback() {
        use std::sync::{Arc, Condvar, Mutex};
        struct ReaderGate(Arc<(Mutex<(bool, bool)>, Condvar)>);
        impl Drop for ReaderGate {
            fn drop(&mut self) {
                let (state, changed) = &*self.0;
                state.lock().unwrap().1 = true;
                changed.notify_all();
            }
        }
        let gate = ReaderGate(Arc::new((Mutex::new((false, false)), Condvar::new())));
        let reader_gate = Arc::clone(&gate.0);
        let wake: pty::PtyOutputWake = Arc::new(move || {
            let (state, changed) = &*reader_gate;
            let mut state = state.lock().unwrap();
            if state.0 {
                return;
            }
            state.0 = true;
            changed.notify_all();
            // A stopped consumer's bounded output send prevents the reader reaching the
            // marker. Gate that same boundary deterministically after the first query chunk.
            while !state.1 {
                let (next, timeout) = changed
                    .wait_timeout(state, std::time::Duration::from_secs(3))
                    .unwrap();
                state = next;
                if timeout.timed_out() {
                    break;
                }
            }
        });
        let (dir, mut live, ready) = raw_query_fixture_with_wake(
            r#"printf '\033[6n'; while [ ! -f "$FIXTURE_ALLOW_EXIT" ]; do sleep 0.005; done"#,
            r#": > "$FIXTURE_READY"; dd bs=1 count=6 > "$FIXTURE_RESULT" 2>/dev/null; printf STALE_RESPONSE_RECEIVED; sleep 30"#,
            Some(wake),
        );
        let gate_deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !gate.0.0.lock().unwrap().0 && std::time::Instant::now() < gate_deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            gate.0.0.lock().unwrap().0,
            "reader must park after publishing the old query"
        );
        std::fs::write(dir.join("allow-exit"), b"").unwrap();
        wait_fixture_ready(&ready);
        let mut output = Vec::new();
        live.pump(|bytes| output.extend_from_slice(bytes));
        let reader_still_blocked = {
            let state = gate.0.0.lock().unwrap();
            state.0 && !state.1
        };
        assert!(
            reader_still_blocked,
            "query parser and writer run while reader cannot observe the marker"
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        let result = dir.join("result");
        while std::fs::read(&result).unwrap_or_default().is_empty()
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let received = std::fs::read(&result).unwrap_or_default();
        drop(gate); // Always release the test-owned reader before joining workers.
        drop(live);
        std::fs::remove_dir_all(dir).unwrap();
        assert!(
            received.is_empty(),
            "fallback received old CPR while the reader boundary was unobserved: {received:02x?}"
        );
    }
}
