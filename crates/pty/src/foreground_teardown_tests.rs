//! Real job-control foreground regressions. Every signal is scoped to an owned PTY or a
//! birth/group/session-checked fixture PID; the Python child also has a hard lifetime.

use super::*;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const WORKLOAD: &str = r#"import os, signal, sys, time
signal.alarm(15)
if sys.argv[2] != "normal":
    signal.signal(signal.SIGHUP, signal.SIG_IGN)
if sys.argv[2] == "ignore-hup-term":
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
with open(sys.argv[1], "x") as output:
    output.write(f"{os.getpid()} {os.getpgrp()} {os.getsid(0)}\n")
while True:
    time.sleep(0.01)
"#;

struct Fixture {
    session: Option<Box<dyn PtySession>>,
    output: Option<PtyOutputReceiver>,
    process: Option<workload_signal::Process>,
    directory: PathBuf,
}

impl Fixture {
    fn spawn(mode: &str) -> Self {
        Self::spawn_with_shell_exit(mode, false)
    }

    fn spawn_with_shell_exit(mode: &str, shell_exits: bool) -> Self {
        Self::spawn_with_reaper(mode, shell_exits, false)
    }

    fn spawn_with_reaper(mode: &str, shell_exits: bool, pause_reaper: bool) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "deppy-pty-foreground-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).unwrap();
        let pid_path = directory.join("identity");
        let spec = CommandSpec {
            program: "/bin/bash".into(),
            args: vec!["--noprofile".into(), "--norc".into(), "-i".into()],
            env: vec![
                ("HISTFILE".into(), "/dev/null".into()),
                ("BASH_ENV".into(), "/dev/null".into()),
                ("ENV".into(), "/dev/null".into()),
                ("PS1".into(), "FIXTURE> ".into()),
                (
                    "DEPPY_FIXTURE_PID".into(),
                    pid_path.to_str().unwrap().to_owned(),
                ),
                ("DEPPY_FIXTURE_MODE".into(), mode.into()),
            ],
            cwd: Some(directory.clone()),
        };
        let session: Box<dyn PtySession> = if pause_reaper {
            // Only the before-poll regression uses the private concrete constructor. Stop and
            // join its reaper while the shell is idle, before issuing the exit command.
            let pair = openpty_with_retry(80, 24).unwrap();
            let mut command = portable_pty::CommandBuilder::new(&spec.program);
            command.args(&spec.args);
            for (key, value) in &spec.env {
                command.env(key, value);
            }
            command.cwd(spec.cwd.as_ref().unwrap());
            let child = pair.slave.spawn_command(command).unwrap();
            let mut session = spawn_unix_session(
                pair.master,
                pair.slave,
                child,
                None,
                None,
                None,
                TestWorkerSpawnFailure::None,
            )
            .unwrap();
            session.reaper_stop.store(true, Ordering::Release);
            join_worker(session.reaper_thread.take(), "fixture reaper");
            Box::new(session)
        } else {
            PortablePtyBackend.spawn(&spec, 80, 24).unwrap()
        };
        let mut fixture = Self {
            session: Some(session),
            output: None,
            process: None,
            directory,
        };
        let session = fixture.session.as_mut().unwrap();
        let shell = session.process_identity();
        fixture.output = session.take_output();
        // WORKLOAD contains no single quotes. Paths/modes are environment values, never shell code.
        let invocation =
            format!("python3 -c '{WORKLOAD}' \"$DEPPY_FIXTURE_PID\" \"$DEPPY_FIXTURE_MODE\"");
        let command = if shell_exits {
            // Disable job control to reproduce the existing same-shell-group descendant contract.
            format!(
                "set +m; {invocation} & while [ ! -s \"$DEPPY_FIXTURE_PID\" ]; do /bin/sleep 0.005; done; exit\n"
            )
        } else {
            format!("{invocation}\n")
        };
        assert!(
            session
                .write_input(command.as_bytes())
                .unwrap()
                .is_accepted()
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        let fields = loop {
            if let Ok(text) = std::fs::read_to_string(fixture.directory.join("identity")) {
                let fields: Vec<i32> = text
                    .split_whitespace()
                    .filter_map(|field| field.parse().ok())
                    .collect();
                if fields.len() == 3 {
                    break fields;
                }
            }
            while fixture.output.as_ref().unwrap().try_recv().is_ok() {}
            assert!(Instant::now() < deadline, "foreground fixture not ready");
            std::thread::sleep(Duration::from_millis(5));
        };
        let process = workload_signal::inspect(fields[0]).unwrap();
        fixture.process = Some(process);
        assert_eq!(process.group, fields[1]);
        assert_eq!(process.session, fields[2]);
        assert!(!process.zombie_for_test());
        if shell_exits {
            assert_eq!(shell.process_group, Some(process.group as u32));
            // Darwin hides zombies from proc_pidinfo before they are reaped. Non-consuming
            // waitid must prove ECHILD for the periodic-reaper case, or an unreaped exit for
            // the deterministic before-poll case. Do not call try_exit_code in either wait.
            let deadline = Instant::now() + Duration::from_secs(3);
            let shell_pid = shell.pid.unwrap() as i32;
            loop {
                let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
                let result = unsafe {
                    libc::waitid(
                        libc::P_PID,
                        shell_pid as libc::id_t,
                        info.as_mut_ptr(),
                        libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                    )
                };
                if result != 0 {
                    let error = std::io::Error::last_os_error();
                    if error.raw_os_error() == Some(libc::ECHILD) {
                        assert!(!pause_reaper, "stopped fixture reaper consumed shell");
                        break;
                    }
                    assert_eq!(error.raw_os_error(), Some(libc::EINTR));
                } else {
                    let info = unsafe { info.assume_init() };
                    let pid = unsafe { info.si_pid() };
                    assert!(pid == 0 || pid == shell_pid);
                    if pause_reaper
                        && pid == shell_pid
                        && matches!(
                            info.si_code,
                            libc::CLD_EXITED | libc::CLD_KILLED | libc::CLD_DUMPED
                        )
                    {
                        break;
                    }
                }
                assert!(
                    Instant::now() < deadline,
                    "fixture shell exit/reap not observed"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(
                fixture.same_process_alive(),
                "descendant must outlive its shell"
            );
        } else {
            assert_eq!(
                session.foreground_process_group(),
                Some(process.group as u32)
            );
            assert_ne!(
                shell.process_group,
                Some(process.group as u32),
                "fixture must run in a real job-control group separate from its shell"
            );
        }
        fixture
    }

    fn same_process_alive(&self) -> bool {
        self.process.is_some_and(|process| {
            workload_signal::inspect(process.pid).is_some_and(|current| {
                current.same_execution(process) && !current.zombie_for_test()
            })
        })
    }

    fn wait_for_exit(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while self.same_process_alive() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        !self.same_process_alive()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Rescue only this captured execution, even if the assertion or product teardown failed.
        if self.same_process_alive() {
            unsafe {
                libc::kill(self.process.unwrap().pid, libc::SIGKILL);
            }
        }
        drop(self.session.take());
        drop(self.output.take());
        let _ = self.wait_for_exit(Duration::from_secs(2));
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn foreground_teardown(mode: &str, use_kill: bool, frozen: bool) {
    let mut fixture = Fixture::spawn(mode);
    if frozen {
        fixture.session.as_ref().unwrap().freeze().unwrap();
    }
    let started = Instant::now();
    if use_kill {
        // Check kill itself; Drop must not supply the missing foreground escalation.
        fixture.session.as_mut().unwrap().kill().unwrap();
    } else {
        drop(fixture.session.take());
    }
    let elapsed = started.elapsed();
    let gone_without_rescue = fixture.wait_for_exit(Duration::from_millis(500));
    let process = fixture.process.unwrap();
    // Complete identity-checked rescue before reporting the protective assertion.
    drop(fixture);
    assert!(
        gone_without_rescue,
        "same foreground execution survived: mode={mode} kill={use_kill} frozen={frozen} identity={process:?} teardown={elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(4),
        "unbounded teardown: {elapsed:?}"
    );
}

#[test]
fn normal_foreground_hup_control_exits_on_drop() {
    foreground_teardown("normal", false, false);
}

#[test]
fn nonfrozen_foreground_ignoring_hup_exits_on_drop() {
    foreground_teardown("ignore-hup", false, false);
}

#[test]
fn nonfrozen_foreground_ignoring_hup_exits_on_kill() {
    foreground_teardown("ignore-hup", true, false);
}

#[test]
fn nonfrozen_foreground_ignoring_hup_term_is_killed_on_drop() {
    foreground_teardown("ignore-hup-term", false, false);
}

#[test]
fn nonfrozen_foreground_ignoring_hup_term_is_killed_on_kill() {
    foreground_teardown("ignore-hup-term", true, false);
}

#[test]
fn frozen_foreground_control_exits_on_drop() {
    foreground_teardown("ignore-hup-term", false, true);
}

fn descendant_after_shell_exit_teardown(use_kill: bool, pause_reaper: bool) {
    let mut fixture = Fixture::spawn_with_reaper("ignore-hup", true, pause_reaper);
    if use_kill {
        fixture.session.as_mut().unwrap().kill().unwrap();
    } else {
        drop(fixture.session.take());
    }
    let gone_without_rescue = fixture.wait_for_exit(Duration::from_millis(500));
    let process = fixture.process.unwrap();
    drop(fixture);
    assert!(
        gone_without_rescue,
        "captured same-group descendant survived after shell exit: kill={use_kill} pause_reaper={pause_reaper} identity={process:?}"
    );
}

#[test]
fn samegroup_descendant_after_shell_reap_exits_on_drop() {
    descendant_after_shell_exit_teardown(false, false);
}

#[test]
fn samegroup_descendant_after_shell_reap_exits_on_kill() {
    descendant_after_shell_exit_teardown(true, false);
}

#[test]
fn samegroup_descendant_before_reaper_poll_exits_on_drop() {
    descendant_after_shell_exit_teardown(false, true);
}

#[test]
fn samegroup_descendant_before_reaper_poll_exits_on_kill() {
    descendant_after_shell_exit_teardown(true, true);
}
