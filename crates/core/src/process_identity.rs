//! Kernel process identities carried by pane hook events. Birth values are opaque
//! platform identities, never timestamps: Linux reports boot-relative raw ticks.

pub const HOOK_EXECUTION_ANCESTORS_MAX: usize = 32;
pub const HOOK_EXECUTION_STAMP_BYTES_MAX: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessBirth {
    pub seconds: u64,
    pub microseconds: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub birth: ProcessBirth,
}

impl ProcessIdentity {
    pub fn capture(pid: u32) -> Option<Self> {
        Some(Self {
            pid,
            birth: pid_start_time(pid)?,
        })
    }

    pub fn is_current(self) -> bool {
        Self::capture(self.pid) == Some(self)
    }

    fn is_valid(self) -> bool {
        self.pid > 0 && self.pid <= i32::MAX as u32 && self.birth.microseconds < 1_000_000
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookExecutionStamp {
    pub runtime_instance: String,
    pub ancestors: Vec<ProcessIdentity>,
}

impl HookExecutionStamp {
    pub fn capture(runtime_instance: &str) -> Option<Self> {
        Self::capture_with(runtime_instance, std::process::id(), kernel_process)
    }

    fn capture_with(
        runtime_instance: &str,
        mut pid: u32,
        mut read: impl FnMut(u32) -> Option<(ProcessBirth, u32)>,
    ) -> Option<Self> {
        if runtime_instance.is_empty()
            || runtime_instance.len() > 128
            || runtime_instance.chars().any(char::is_control)
        {
            return None;
        }
        let mut ancestors = Vec::with_capacity(HOOK_EXECUTION_ANCESTORS_MAX);
        while pid > 0 && ancestors.len() < HOOK_EXECUTION_ANCESTORS_MAX {
            if ancestors
                .iter()
                .any(|identity: &ProcessIdentity| identity.pid == pid)
            {
                return None;
            }
            let Some((birth, parent)) = read(pid) else {
                break;
            };
            ancestors.push(ProcessIdentity { pid, birth });
            if pid == 1 {
                break;
            }
            pid = parent;
        }
        let stamp = Self {
            runtime_instance: runtime_instance.to_owned(),
            ancestors,
        };
        stamp.is_valid().then_some(stamp)
    }

    pub fn is_valid(&self) -> bool {
        !self.runtime_instance.is_empty()
            && self.runtime_instance.len() <= 128
            && !self.runtime_instance.chars().any(char::is_control)
            && !self.ancestors.is_empty()
            && self.ancestors.len() <= HOOK_EXECUTION_ANCESTORS_MAX
            && self.ancestors.iter().all(|identity| identity.is_valid())
            && self.ancestors.iter().enumerate().all(|(index, identity)| {
                !self.ancestors[..index]
                    .iter()
                    .any(|prior| prior.pid == identity.pid)
            })
    }

    pub fn contains(&self, identity: ProcessIdentity) -> bool {
        self.is_valid() && self.ancestors.contains(&identity)
    }
}

pub fn pid_start_time(pid: u32) -> Option<ProcessBirth> {
    kernel_process(pid).map(|(birth, _)| birth)
}

pub fn pid_parent(pid: u32) -> Option<u32> {
    kernel_process(pid).map(|(_, parent)| parent)
}

fn kernel_process(pid: u32) -> Option<(ProcessBirth, u32)> {
    if pid == 0 || pid > i32::MAX as u32 {
        return None;
    }
    #[cfg(target_os = "macos")]
    {
        read_macos_process(pid)
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(format!("/proc/{pid}/stat"))
            .ok()?;
        read_linux_process(pid, file)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = pid;
        None
    }
}

#[cfg(any(target_os = "linux", test))]
fn read_linux_process(pid: u32, reader: impl std::io::Read) -> Option<(ProcessBirth, u32)> {
    use std::io::Read as _;
    const MAX_STAT_BYTES: usize = 4096;
    if pid == 0 || pid > i32::MAX as u32 {
        return None;
    }
    let mut stat = String::new();
    reader
        .take((MAX_STAT_BYTES + 1) as u64)
        .read_to_string(&mut stat)
        .ok()?;
    if stat.len() > MAX_STAT_BYTES {
        return None;
    }
    let (head, fields) = stat.rsplit_once(") ")?;
    if head.split_once(" (")?.0.parse::<u32>().ok()? != pid {
        return None;
    }
    let fields: Vec<_> = fields.split_ascii_whitespace().collect();
    let parent = fields.get(1)?.parse::<u32>().ok()?;
    if parent > i32::MAX as u32 {
        return None;
    }
    let ticks = fields.get(19)?.parse::<u64>().ok()?;
    Some((
        ProcessBirth {
            seconds: ticks,
            microseconds: 0,
        },
        parent,
    ))
}

#[cfg(target_os = "macos")]
fn read_macos_process(pid: u32) -> Option<(ProcessBirth, u32)> {
    let pid_i32 = i32::try_from(pid).ok()?;
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let size = i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;
    // SAFETY: proc_pidinfo writes to the correctly sized buffer. No partial result is read.
    let written = unsafe {
        libc::proc_pidinfo(
            pid_i32,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written != size {
        return None;
    }
    // SAFETY: a successful full-sized kernel write initialized every field.
    let info = unsafe { info.assume_init() };
    if info.pbi_pid != pid
        || info.pbi_start_tvsec == 0
        || info.pbi_start_tvusec >= 1_000_000
        || info.pbi_ppid > i32::MAX as u32
    {
        return None;
    }
    Some((
        ProcessBirth {
            seconds: info.pbi_start_tvsec,
            microseconds: info.pbi_start_tvusec,
        },
        info.pbi_ppid,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn hook_execution_capture_contains_the_current_process_and_parent() {
        let own = ProcessIdentity::capture(std::process::id()).expect("live process identity");
        let parent = ProcessIdentity::capture(pid_parent(own.pid).expect("live parent pid"))
            .expect("live parent identity");
        let stamp = HookExecutionStamp::capture("isolated-runtime").expect("live ancestry");
        assert!(stamp.is_valid());
        assert!(stamp.contains(own));
        assert!(stamp.contains(parent));
        assert!(own.is_current());
        assert!(
            stamp.ancestors.len() >= 2,
            "hook must preserve its CLI ancestors"
        );
        assert!(HookExecutionStamp::capture("").is_none());
    }

    #[test]
    fn linux_stat_birth_is_raw_ticks_with_parent_and_bounded_input() {
        let stat = format!(
            "42 (worker name) tricky) R {} 987654 0",
            ["0"; 18].join(" ")
        );
        let (birth, parent) = read_linux_process(42, stat.as_bytes()).unwrap();
        assert_eq!(
            birth,
            ProcessBirth {
                seconds: 987654,
                microseconds: 0
            }
        );
        assert_eq!(parent, 0);
        assert!(read_linux_process(43, stat.as_bytes()).is_none());
        assert!(read_linux_process(0, stat.as_bytes()).is_none());
        assert!(read_linux_process(42, b"42 malformed".as_slice()).is_none());
        assert!(
            read_linux_process(
                42,
                stat.replace("987654", "18446744073709551616").as_bytes()
            )
            .is_none()
        );
        assert!(read_linux_process(42, format!("{stat}{}", " ".repeat(4096)).as_bytes()).is_none());
        let zero = stat.replace("987654", "0");
        assert_eq!(
            read_linux_process(42, zero.as_bytes()).unwrap().0.seconds,
            0
        );
    }

    #[test]
    fn hook_ancestry_is_bounded_and_rejects_cycles_bad_identity_and_runtime() {
        let birth = ProcessBirth {
            seconds: 0,
            microseconds: 0,
        };
        let stamp =
            HookExecutionStamp::capture_with("runtime", 100, |pid| Some((birth, pid - 1))).unwrap();
        assert_eq!(stamp.ancestors.len(), HOOK_EXECUTION_ANCESTORS_MAX);
        assert_eq!(stamp.ancestors[0].birth, birth);
        assert!(
            HookExecutionStamp::capture_with("runtime", 42, |pid| Some((birth, pid))).is_none()
        );
        assert!(HookExecutionStamp::capture_with("runtime", 42, |_| None).is_none());
        assert!(
            HookExecutionStamp::capture_with("bad\nruntime", 42, |_| Some((birth, 0))).is_none()
        );
        assert!(
            HookExecutionStamp::capture_with(&"x".repeat(129), 42, |_| Some((birth, 0))).is_none()
        );
        let mut invalid = stamp.clone();
        invalid.ancestors[0].birth.microseconds = 1_000_000;
        assert!(!invalid.is_valid());
    }
}
