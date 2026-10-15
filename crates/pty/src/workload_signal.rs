//! Identity-checked signals for a PTY's shell and its current foreground job.

use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Process {
    pub pid: libc::pid_t,
    birth: (u64, u64),
    pub group: libc::pid_t,
    pub session: libc::pid_t,
    stopped: bool,
    zombie: bool,
}

impl Process {
    #[cfg(test)]
    pub(super) fn stopped_for_test(self) -> bool {
        self.stopped
    }
    #[cfg(test)]
    pub(super) fn zombie_for_test(self) -> bool {
        self.zombie
    }
    pub(super) fn same_execution(self, other: Self) -> bool {
        self.pid == other.pid
            && self.birth == other.birth
            && self.group == other.group
            && self.session == other.session
    }
}

pub(super) fn inspect(pid: libc::pid_t) -> Option<Process> {
    if pid <= 0 {
        return None;
    }
    #[cfg(target_os = "macos")]
    {
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
        // SAFETY: info is writable, correctly sized storage; only a full result is read.
        if unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        } != size
        {
            return None;
        }
        let info = unsafe { info.assume_init() };
        let session = unsafe { libc::getsid(pid) };
        if info.pbi_pid != pid as u32 || session <= 0 {
            return None;
        }
        Some(Process {
            pid,
            birth: (info.pbi_start_tvsec, info.pbi_start_tvusec),
            group: info.pbi_pgid as i32,
            session,
            stopped: info.pbi_status == libc::SSTOP,
            zombie: info.pbi_status == libc::SZOMB,
        })
    }
    #[cfg(target_os = "linux")]
    {
        use std::io::Read as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(format!("/proc/{pid}/stat"))
            .ok()?;
        let mut stat = String::new();
        file.take(4097).read_to_string(&mut stat).ok()?;
        if stat.len() > 4096 {
            return None;
        }
        let (head, fields) = stat.rsplit_once(") ")?;
        if head.split_once(" (")?.0.parse::<i32>().ok()? != pid {
            return None;
        }
        let fields: Vec<_> = fields.split_ascii_whitespace().collect();
        Some(Process {
            pid,
            birth: (fields.get(19)?.parse().ok()?, 0),
            group: fields.get(2)?.parse().ok()?,
            session: fields.get(3)?.parse().ok()?,
            stopped: matches!(*fields.first()?, "T" | "t"),
            zombie: *fields.first()? == "Z",
        })
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = pid;
        None
    }
}

fn members(group: libc::pid_t) -> anyhow::Result<Vec<Process>> {
    #[cfg(target_os = "macos")]
    let pids = {
        // PROC_PGRP_ONLY is 2 in Darwin's sys/proc_info.h. A full result is retried with
        // bounded slack; truncated membership must never be reported as fully frozen.
        const PROC_PGRP_ONLY: u32 = 2;
        let bytes =
            unsafe { libc::proc_listpids(PROC_PGRP_ONLY, group as u32, std::ptr::null_mut(), 0) };
        anyhow::ensure!(bytes >= 0, "process group inspection failed");
        let capacity = (bytes as usize / std::mem::size_of::<libc::pid_t>()).saturating_add(32);
        anyhow::ensure!(capacity <= 16384, "process group inspection limit");
        let mut pids = vec![0i32; capacity];
        let bytes = unsafe {
            libc::proc_listpids(
                PROC_PGRP_ONLY,
                group as u32,
                pids.as_mut_ptr().cast(),
                (capacity * std::mem::size_of::<libc::pid_t>()) as i32,
            )
        };
        anyhow::ensure!(
            bytes >= 0 && (bytes as usize) < capacity * std::mem::size_of::<libc::pid_t>(),
            "process group inspection incomplete"
        );
        pids.truncate(bytes as usize / std::mem::size_of::<libc::pid_t>());
        pids
    };
    #[cfg(target_os = "linux")]
    let pids = std::fs::read_dir("/proc")?
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<i32>().ok())
        .collect::<Vec<_>>();
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let pids: Vec<libc::pid_t> = Vec::new();
    Ok(pids
        .into_iter()
        .filter_map(inspect)
        .filter(|process| process.group == group && !process.zombie)
        .collect())
}

#[derive(Debug)]
pub(super) struct FrozenGroup {
    group: libc::pid_t,
    members: Vec<Process>,
    session: libc::pid_t,
    stop_requested: bool,
}

impl FrozenGroup {
    pub fn capture(group: libc::pid_t, owner: Process) -> anyhow::Result<Self> {
        anyhow::ensure!(
            group > 0 && group != unsafe { libc::getpgrp() },
            "unsafe process group"
        );
        let members = members(group)?;
        anyhow::ensure!(
            !members.is_empty() && members.iter().all(|p| p.session == owner.session),
            "foreground process ownership changed"
        );
        Ok(Self {
            group,
            members,
            session: owner.session,
            stop_requested: false,
        })
    }

    pub fn group(&self) -> libc::pid_t {
        self.group
    }

    pub fn merge_capture(&mut self, other: Self) {
        debug_assert_eq!(self.group, other.group);
        debug_assert_eq!(self.session, other.session);
        for process in other.members {
            if !self.members.iter().any(|old| old.same_execution(process)) {
                self.members.push(process);
            }
        }
    }

    pub fn captured_alive(&self) -> bool {
        self.members.iter().any(|captured| {
            inspect(captured.pid)
                .is_some_and(|current| current.same_execution(*captured) && !current.zombie)
        })
    }

    /// Normal teardown can outlive the shell and race a running workload's forks. Signal only
    /// the executions pinned before terminal close; a new group member must not broaden that
    /// authority or prevent the already captured members from being terminated.
    pub fn signal_captured_members(&self, signal: libc::c_int) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.group > 0 && self.group != unsafe { libc::getpgrp() },
            "unsafe process group"
        );
        let mut first_error = None;
        for captured in &self.members {
            let Some(current) = inspect(captured.pid) else {
                continue;
            };
            if !current.same_execution(*captured)
                || current.zombie
                || current.pid == unsafe { libc::getpid() }
            {
                continue;
            }
            // SAFETY: this individual member still has the captured PID/birth/group/session.
            // No killpg is used after shell reap, so unpinned or reused groups are untouched.
            if unsafe { libc::kill(current.pid, signal) } != 0 {
                let error = std::io::Error::last_os_error();
                if !super::signal_failure_is_already_gone(error.raw_os_error()) {
                    first_error.get_or_insert(error);
                }
            }
        }
        match first_error {
            Some(error) => Err(error.into()),
            None => Ok(()),
        }
    }

    fn current_members(&self) -> anyhow::Result<Vec<Process>> {
        let current = members(self.group)?;
        anyhow::ensure!(
            current
                .iter()
                .all(|p| self.members.iter().any(|old| old.same_execution(*p))),
            "process group identity changed"
        );
        Ok(current)
    }

    pub fn signal(&mut self, signal: libc::c_int) -> anyhow::Result<()> {
        let current = if signal == libc::SIGSTOP {
            // A running workload may fork between capture and STOP. Same-session members are
            // legitimate; capture their births before stopping instead of rejecting normal jobs.
            let current = members(self.group)?;
            anyhow::ensure!(
                current.iter().all(|p| p.session == self.session),
                "process group ownership changed"
            );
            self.members = current.clone();
            current
        } else {
            self.current_members()?
        };
        if current.is_empty() {
            return Ok(());
        }
        // SAFETY: every live member still has the captured birth/group/session, and our own
        // group was rejected. The session leader is pinned by the caller's child mutex.
        if unsafe { libc::killpg(self.group, signal) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if signal == libc::SIGSTOP {
            self.stop_requested = true;
        } else if signal == libc::SIGCONT {
            self.stop_requested = false;
        }
        Ok(())
    }

    pub fn rollback_stop(&mut self) -> anyhow::Result<()> {
        if !self.stop_requested {
            return Ok(());
        }
        // Our STOP can race a fork even after the last failed verification. The caller still
        // pins the owning session leader, so same-session births are part of that STOP attempt.
        let current = members(self.group)?;
        anyhow::ensure!(
            current.iter().all(|p| p.session == self.session),
            "process group ownership changed"
        );
        self.members = current;
        if !self.members.is_empty() && unsafe { libc::killpg(self.group, libc::SIGCONT) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        self.stop_requested = false;
        Ok(())
    }

    pub fn verify(&mut self, stopped: bool, deadline: Instant) -> anyhow::Result<()> {
        loop {
            let current = {
                let current = members(self.group)?;
                anyhow::ensure!(
                    current.iter().all(|p| p.session == self.session),
                    "process group ownership changed"
                );
                current
            };
            if stopped {
                // Retain new births on every observation, including the timeout/error path.
                // A failed freeze must still be able to undo the STOP it already delivered.
                self.members = current.clone();
            }
            if current.is_empty() && !stopped {
                return Ok(());
            }
            anyhow::ensure!(
                !current.is_empty(),
                "process group exited during transition"
            );
            if current.iter().all(|p| p.stopped == stopped) {
                if stopped {
                    self.members = current;
                }
                return Ok(());
            }
            anyhow::ensure!(
                Instant::now() < deadline,
                "process group transition not observed"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

#[cfg(test)]
pub(super) fn group_stopped_for_test(group: libc::pid_t) -> bool {
    members(group).is_ok_and(|members| !members.is_empty() && members.iter().all(|p| p.stopped))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead as _, Write as _};
    use std::os::unix::process::CommandExt as _;
    use std::process::{Command, Stdio};

    #[test]
    fn captured_signals_skip_changed_birth_group_and_session() {
        let mut command = Command::new("python3");
        command
            .args([
                "-c",
                "import signal,time; signal.alarm(15); print('READY',flush=True); time.sleep(30)",
            ])
            .stdout(Stdio::piped());
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        struct Fixture(std::process::Child);
        impl Drop for Fixture {
            fn drop(&mut self) {
                // This direct child has not been reaped, so its PID remains owned by this handle.
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut fixture = Fixture(command.spawn().unwrap());
        let mut output = std::io::BufReader::new(fixture.0.stdout.take().unwrap());
        let mut line = String::new();
        output.read_line(&mut line).unwrap();
        assert_eq!(line.trim(), "READY");
        let owner = inspect(fixture.0.id() as i32).unwrap();
        assert!(FrozenGroup::capture(unsafe { libc::getpgrp() }, owner).is_err());
        for field in 0..3 {
            let mut group = FrozenGroup::capture(owner.group, owner).unwrap();
            match field {
                0 => group.members[0].birth.0 = group.members[0].birth.0.wrapping_add(1),
                1 => group.members[0].group = -1,
                _ => group.members[0].session = -1,
            }
            group.signal_captured_members(libc::SIGKILL).unwrap();
            assert!(
                !group.captured_alive(),
                "changed identity must not remain pinned"
            );
            std::thread::sleep(Duration::from_millis(20));
            assert!(
                fixture.0.try_wait().unwrap().is_none(),
                "a stale birth/group/session snapshot must not signal the live fixture"
            );
        }
        let group = FrozenGroup::capture(owner.group, owner).unwrap();
        assert!(group.captured_alive());
        group.signal_captured_members(libc::SIGTERM).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while fixture.0.try_wait().unwrap().is_none() {
            assert!(
                Instant::now() < deadline,
                "matching captured process must terminate"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn failed_stop_verification_rolls_back_a_same_session_fork() {
        let mut command = Command::new("python3");
        command.args(["-c", "import os,sys,time; print('READY',flush=True); sys.stdin.buffer.read(1); child=os.fork(); print(child,flush=True) if child else None; time.sleep(30)"])
            .stdin(Stdio::piped()).stdout(Stdio::piped());
        // This fixture creates its own session; cleanup signals only that recorded group.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        struct Fixture(std::process::Child);
        impl Drop for Fixture {
            fn drop(&mut self) {
                unsafe {
                    libc::killpg(self.0.id() as i32, libc::SIGKILL);
                }
                let _ = self.0.wait();
            }
        }
        let mut fixture = Fixture(command.spawn().unwrap());
        let pid = fixture.0.id() as i32;
        let mut output = std::io::BufReader::new(fixture.0.stdout.take().unwrap());
        let mut line = String::new();
        output.read_line(&mut line).unwrap();
        assert_eq!(line.trim(), "READY");
        let owner = inspect(pid).unwrap();
        let mut group = FrozenGroup::capture(pid, owner).unwrap();
        fixture.0.stdin.as_mut().unwrap().write_all(b"x").unwrap();
        line.clear();
        output.read_line(&mut line).unwrap();
        let fork = line.trim().parse::<i32>().unwrap();
        // Inject the narrow post-snapshot fork race: the original member is stopped while
        // the just-born member is still running when transition verification expires.
        assert_eq!(unsafe { libc::killpg(pid, libc::SIGSTOP) }, 0);
        assert_eq!(unsafe { libc::kill(fork, libc::SIGCONT) }, 0);
        let deadline = Instant::now() + Duration::from_millis(500);
        while !inspect(pid).is_some_and(|p| p.stopped) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(inspect(pid).unwrap().stopped);
        assert!(group.verify(true, Instant::now()).is_err());
        let rollback = group.signal(libc::SIGCONT);
        let deadline = Instant::now() + Duration::from_millis(500);
        while inspect(pid).is_some_and(|p| p.stopped) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            rollback.is_ok(),
            "failed STOP must retain new same-session births: {rollback:?}"
        );
        assert!(
            !inspect(pid).unwrap().stopped,
            "rollback left its original member stopped"
        );
    }
}
