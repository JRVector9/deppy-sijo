use super::contracts::ProjectKeysError;
use secret::SecretString;
use std::{
    io::{Read, Write},
    path::Path,
    process::{Command, Stdio},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

pub struct ProcessOutput {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
}
#[derive(Default)]
pub struct ProcessEnvironment {
    pub values: Vec<(String, SecretString)>,
    pub remove_names: Vec<String>,
}
pub trait ProcessRunner: Send + Sync {
    fn run(
        &self,
        command: &[String],
        cwd: &Path,
        environment: &ProcessEnvironment,
        stdin: Option<&SecretString>,
        inherit_output: bool,
    ) -> Result<ProcessOutput, ProjectKeysError>;
    fn run_until(
        &self,
        command: &[String],
        cwd: &Path,
        environment: &ProcessEnvironment,
        stdin: Option<&SecretString>,
        inherit_output: bool,
        deadline: Instant,
    ) -> Result<ProcessOutput, ProjectKeysError> {
        if Instant::now() >= deadline {
            return Err(ProjectKeysError::Process);
        }
        let result = self.run(command, cwd, environment, stdin, inherit_output);
        if Instant::now() >= deadline {
            return Err(ProjectKeysError::Process);
        }
        result
    }
    fn run_in_directory(
        &self,
        command: &[String],
        cwd: &super::sources::AnchoredRoot,
        environment: &ProcessEnvironment,
        stdin: Option<&SecretString>,
        inherit_output: bool,
    ) -> Result<ProcessOutput, ProjectKeysError> {
        self.run(command, cwd.path(), environment, stdin, inherit_output)
    }
    fn cancel(&self) {}
    fn wait_for_idle(&self, _timeout: Duration) -> bool {
        true
    }
}
pub struct NativeProcessRunner {
    pub timeout: Duration,
    cancelled: Arc<AtomicBool>,
    interactive_signals: bool,
    activity: Activity,
}
#[derive(Default)]
struct Activity {
    count: Mutex<usize>,
    idle: Condvar,
}
impl Activity {
    fn enter(&self) -> ActivityGuard<'_> {
        *self.count.lock().unwrap_or_else(|error| error.into_inner()) += 1;
        ActivityGuard(self)
    }
    fn wait(&self, timeout: Duration) -> bool {
        let count = self.count.lock().unwrap_or_else(|error| error.into_inner());
        let (count, _) = self
            .idle
            .wait_timeout_while(count, timeout, |count| *count != 0)
            .unwrap_or_else(|error| error.into_inner());
        *count == 0
    }
}
struct ActivityGuard<'a>(&'a Activity);
impl Drop for ActivityGuard<'_> {
    fn drop(&mut self) {
        *self
            .0
            .count
            .lock()
            .unwrap_or_else(|error| error.into_inner()) -= 1;
        self.0.idle.notify_all();
    }
}
struct PipeTask<T> {
    rx: std::sync::mpsc::Receiver<T>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}
impl<T> Drop for PipeTask<T> {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
fn terminate(child: &mut std::process::Child) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}
#[cfg(unix)]
struct ForegroundTerminal {
    owner: i32,
    attributes: libc::termios,
}
#[cfg(unix)]
impl ForegroundTerminal {
    fn foreground(owner: i32) -> Result<(), ProjectKeysError> {
        let mut blocked = unsafe { std::mem::zeroed::<libc::sigset_t>() };
        let mut previous = unsafe { std::mem::zeroed::<libc::sigset_t>() };
        unsafe {
            libc::sigemptyset(&mut blocked);
            libc::sigaddset(&mut blocked, libc::SIGTTOU);
        }
        if unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, &mut previous) } != 0 {
            return Err(ProjectKeysError::Process);
        }
        let result = unsafe { libc::tcsetpgrp(libc::STDIN_FILENO, owner) };
        unsafe {
            libc::pthread_sigmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut());
        }
        if result < 0 {
            Err(ProjectKeysError::Process)
        } else {
            Ok(())
        }
    }
    fn acquire(child: i32) -> Result<Self, ProjectKeysError> {
        let owner = unsafe { libc::tcgetpgrp(libc::STDIN_FILENO) };
        let mut attributes = unsafe { std::mem::zeroed::<libc::termios>() };
        if owner != unsafe { libc::getpgrp() }
            || unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut attributes) } != 0
        {
            return Err(ProjectKeysError::Process);
        }
        Self::foreground(child)?;
        // A child can attempt its first read before the parent transfers the terminal.
        unsafe {
            libc::kill(-child, libc::SIGCONT);
        }
        Ok(Self { owner, attributes })
    }
}
#[cfg(unix)]
impl Drop for ForegroundTerminal {
    fn drop(&mut self) {
        if Self::foreground(self.owner).is_ok() {
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.attributes);
            }
        }
    }
}
impl Default for NativeProcessRunner {
    fn default() -> Self {
        Self::with_timeout(Duration::from_secs(30))
    }
}
impl NativeProcessRunner {
    pub fn with_timeout(timeout: Duration) -> Self {
        Self {
            timeout,
            cancelled: Arc::new(AtomicBool::new(false)),
            interactive_signals: false,
            activity: Activity::default(),
        }
    }
    pub fn for_cli() -> Self {
        Self {
            interactive_signals: true,
            ..Self::default()
        }
    }
    pub fn was_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}
#[cfg(unix)]
static CLI_INTERRUPTED: AtomicBool = AtomicBool::new(false);
#[cfg(unix)]
extern "C" fn cli_signal(_signal: libc::c_int) {
    CLI_INTERRUPTED.store(true, Ordering::Release);
}
#[cfg(unix)]
struct CliSignalGuard {
    previous: [libc::sigaction; 2],
}
#[cfg(unix)]
impl CliSignalGuard {
    fn install() -> Result<Self, ProjectKeysError> {
        CLI_INTERRUPTED.store(false, Ordering::Release);
        let mut previous = unsafe { std::mem::zeroed::<[libc::sigaction; 2]>() };
        let mut action = unsafe { std::mem::zeroed::<libc::sigaction>() };
        action.sa_sigaction = cli_signal as *const () as usize;
        unsafe {
            libc::sigemptyset(&mut action.sa_mask);
        }
        for (index, signal) in [libc::SIGINT, libc::SIGTERM].into_iter().enumerate() {
            if unsafe { libc::sigaction(signal, &action, &mut previous[index]) } != 0 {
                if index == 1 {
                    unsafe {
                        libc::sigaction(libc::SIGINT, &previous[0], std::ptr::null_mut());
                    }
                }
                return Err(ProjectKeysError::Process);
            }
        }
        Ok(Self { previous })
    }
}
#[cfg(unix)]
impl Drop for CliSignalGuard {
    fn drop(&mut self) {
        for (index, signal) in [libc::SIGINT, libc::SIGTERM].into_iter().enumerate() {
            unsafe {
                libc::sigaction(signal, &self.previous[index], std::ptr::null_mut());
            }
        }
    }
}
#[cfg(unix)]
fn nonblocking<T: std::os::fd::AsRawFd>(pipe: &T) -> Result<(), ProjectKeysError> {
    let fd = pipe.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(ProjectKeysError::Process);
    }
    Ok(())
}
struct RunOptions<'a> {
    directory: Option<&'a std::fs::File>,
    deadline: Option<Instant>,
}
impl ProcessRunner for NativeProcessRunner {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
    fn wait_for_idle(&self, timeout: Duration) -> bool {
        self.activity.wait(timeout)
    }
    fn run(
        &self,
        argv: &[String],
        cwd: &Path,
        environment: &ProcessEnvironment,
        stdin: Option<&SecretString>,
        inherit: bool,
    ) -> Result<ProcessOutput, ProjectKeysError> {
        self.run_internal(
            argv,
            cwd,
            environment,
            stdin,
            inherit,
            RunOptions {
                directory: None,
                deadline: None,
            },
        )
    }
    fn run_until(
        &self,
        argv: &[String],
        cwd: &Path,
        environment: &ProcessEnvironment,
        stdin: Option<&SecretString>,
        inherit: bool,
        deadline: Instant,
    ) -> Result<ProcessOutput, ProjectKeysError> {
        self.run_internal(
            argv,
            cwd,
            environment,
            stdin,
            inherit,
            RunOptions {
                directory: None,
                deadline: Some(deadline),
            },
        )
    }
    fn run_in_directory(
        &self,
        argv: &[String],
        cwd: &super::sources::AnchoredRoot,
        environment: &ProcessEnvironment,
        stdin: Option<&SecretString>,
        inherit: bool,
    ) -> Result<ProcessOutput, ProjectKeysError> {
        self.run_internal(
            argv,
            cwd.path(),
            environment,
            stdin,
            inherit,
            RunOptions {
                directory: Some(cwd.file()),
                deadline: None,
            },
        )
    }
}
impl NativeProcessRunner {
    fn run_internal(
        &self,
        argv: &[String],
        cwd: &Path,
        environment: &ProcessEnvironment,
        stdin: Option<&SecretString>,
        inherit: bool,
        options: RunOptions<'_>,
    ) -> Result<ProcessOutput, ProjectKeysError> {
        let RunOptions {
            directory,
            deadline: operation_deadline,
        } = options;
        let _activity = self.activity.enter();
        let deadline = operation_deadline.map_or_else(
            || Instant::now() + self.timeout,
            |end| end.min(Instant::now() + self.timeout),
        );
        if Instant::now() >= deadline {
            return Err(ProjectKeysError::Process);
        }
        if self.cancelled.load(Ordering::Acquire) {
            return Err(ProjectKeysError::Process);
        }
        #[cfg(unix)]
        let _signals = if self.interactive_signals {
            Some(CliSignalGuard::install()?)
        } else {
            None
        };
        let executable = argv
            .first()
            .filter(|a| !a.is_empty())
            .ok_or(ProjectKeysError::InvalidInput)?;
        let mut cmd = Command::new(executable);
        cmd.args(&argv[1..]);
        if directory.is_none() {
            cmd.current_dir(cwd);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
            if let Some(directory) = directory {
                use std::os::fd::AsRawFd;
                let fd = directory.as_raw_fd();
                // The retained, verified directory remains alive through spawn. No pathname is reopened.
                unsafe {
                    cmd.pre_exec(move || {
                        if libc::fchdir(fd) != 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }
            }
        }
        // Captured helper IO requires cancellable native pipes. Unsupported platforms fail closed.
        #[cfg(not(unix))]
        if !inherit || stdin.is_some() {
            return Err(ProjectKeysError::Process);
        }
        // gh ambient tokens must not silently replace an explicitly chosen authenticated account.
        if executable == "gh" {
            cmd.env_remove("GH_TOKEN")
                .env_remove("GITHUB_TOKEN")
                .env_remove("GH_ENTERPRISE_TOKEN")
                .env_remove("GITHUB_ENTERPRISE_TOKEN")
                .env_remove("GH_HOST");
        }
        for name in &environment.remove_names {
            cmd.env_remove(name);
        }
        for (name, value) in &environment.values {
            cmd.env(name, value.expose());
        }
        cmd.stdin(if stdin.is_some() {
            Stdio::piped()
        } else if inherit {
            Stdio::inherit()
        } else {
            Stdio::null()
        });
        cmd.stdout(if inherit {
            Stdio::inherit()
        } else {
            Stdio::piped()
        });
        // Never collect raw network/subprocess errors; child build output is intentionally its own terminal output.
        cmd.stderr(if inherit {
            Stdio::inherit()
        } else {
            Stdio::null()
        });
        let mut child = cmd.spawn().map_err(|_| ProjectKeysError::Process)?;
        #[cfg(unix)]
        let _foreground = if self.interactive_signals
            && inherit
            && unsafe { libc::isatty(libc::STDIN_FILENO) } == 1
        {
            match ForegroundTerminal::acquire(child.id() as i32) {
                Ok(guard) => Some(guard),
                Err(error) => {
                    terminate(&mut child);
                    return Err(error);
                }
            }
        } else {
            None
        };
        let reader = child.stdout.take().map(|mut pipe| {
            let cancelled = self.cancelled.clone();
            let stop = Arc::new(AtomicBool::new(false));
            let thread_stop = stop.clone();
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            #[cfg(unix)]
            let prepared = nonblocking(&pipe);
            #[cfg(not(unix))]
            let prepared = Err(ProjectKeysError::Process);
            let handle = std::thread::spawn(move || {
                let result = (|| {
                    prepared?;
                    let mut bytes = Vec::new();
                    let mut chunk = [0u8; 4096];
                    loop {
                        if cancelled.load(Ordering::Acquire)
                            || thread_stop.load(Ordering::Acquire)
                            || Instant::now() >= deadline
                        {
                            return Err(ProjectKeysError::Process);
                        }
                        match pipe.read(&mut chunk) {
                            Ok(0) => return Ok(bytes),
                            Ok(count) => {
                                if bytes.len() + count > 512 * 1024 {
                                    return Err(ProjectKeysError::Process);
                                }
                                bytes.extend_from_slice(&chunk[..count]);
                            }
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                std::thread::sleep(Duration::from_millis(5))
                            }
                            Err(_) => return Err(ProjectKeysError::Process),
                        }
                    }
                })();
                let _ = tx.send(result);
            });
            PipeTask {
                rx,
                stop,
                handle: Some(handle),
            }
        });
        let writer = if let Some(value) = stdin {
            let cancelled = self.cancelled.clone();
            let stop = Arc::new(AtomicBool::new(false));
            let thread_stop = stop.clone();
            let mut pipe = child.stdin.take().ok_or(ProjectKeysError::Process)?;
            // A writer thread prevents a blocked pipe from defeating the gh timeout.
            let value = SecretString::new(value.expose().to_owned());
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            #[cfg(unix)]
            let prepared = nonblocking(&pipe);
            #[cfg(not(unix))]
            let prepared = Err(ProjectKeysError::Process);
            let handle = std::thread::spawn(move || {
                let result = (|| {
                    prepared?;
                    let mut bytes = value.expose().as_bytes();
                    while !bytes.is_empty() {
                        if cancelled.load(Ordering::Acquire)
                            || thread_stop.load(Ordering::Acquire)
                            || Instant::now() >= deadline
                        {
                            return Err(ProjectKeysError::Process);
                        }
                        match pipe.write(bytes) {
                            Ok(0) => return Err(ProjectKeysError::Process),
                            Ok(count) => bytes = &bytes[count..],
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                std::thread::sleep(Duration::from_millis(5))
                            }
                            Err(_) => return Err(ProjectKeysError::Process),
                        }
                    }
                    Ok(())
                })();
                let _ = tx.send(result);
            });
            Some(PipeTask {
                rx,
                stop,
                handle: Some(handle),
            })
        } else {
            None
        };
        let mut status = None;
        let mut stdout = None;
        let mut written = writer.is_none();
        loop {
            #[cfg(unix)]
            if self.interactive_signals && CLI_INTERRUPTED.load(Ordering::Acquire) {
                self.cancel();
            }
            if self.cancelled.load(Ordering::Acquire) {
                terminate(&mut child);
                return Err(ProjectKeysError::Process);
            }
            if status.is_none() {
                match child.try_wait() {
                    Ok(next) => status = next,
                    Err(_) => {
                        terminate(&mut child);
                        return Err(ProjectKeysError::Process);
                    }
                }
            }
            if stdout.is_none() {
                if let Some(reader) = &reader {
                    match reader.rx.try_recv() {
                        Ok(Ok(bytes)) => stdout = Some(bytes),
                        Ok(Err(_)) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            terminate(&mut child);
                            return Err(ProjectKeysError::Process);
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => {}
                    }
                } else {
                    stdout = Some(vec![]);
                }
            }
            if !written && let Some(writer) = &writer {
                match writer.rx.try_recv() {
                    Ok(Ok(())) => written = true,
                    Ok(Err(_)) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        terminate(&mut child);
                        return Err(ProjectKeysError::Process);
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {}
                }
            }
            if let Some(status) = status
                && written
                && let Some(stdout) = stdout.take()
            {
                #[cfg(unix)]
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
                return Ok(ProcessOutput {
                    exit_code: status.code().unwrap_or(1),
                    stdout,
                });
            }
            if (!inherit || operation_deadline.is_some()) && Instant::now() >= deadline {
                terminate(&mut child);
                return Err(ProjectKeysError::Process);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

pub trait Clipboard: Send + Sync {
    fn copy(&self, value: &SecretString) -> Result<(), ProjectKeysError>;
    fn clear_if_equal(&self, _value: &SecretString) -> Result<(), ProjectKeysError> {
        Ok(())
    }
}
#[cfg_attr(test, allow(dead_code))]
pub struct NativeClipboard;
impl Clipboard for NativeClipboard {
    fn copy(&self, value: &SecretString) -> Result<(), ProjectKeysError> {
        arboard::Clipboard::new()
            .map_err(|_| ProjectKeysError::Clipboard)?
            .set_text(value.expose().to_owned())
            .map_err(|_| ProjectKeysError::Clipboard)
    }
    fn clear_if_equal(&self, value: &SecretString) -> Result<(), ProjectKeysError> {
        let mut clipboard = arboard::Clipboard::new().map_err(|_| ProjectKeysError::Clipboard)?;
        let current = SecretString::new(
            clipboard
                .get_text()
                .map_err(|_| ProjectKeysError::Clipboard)?,
        );
        if current.expose() == value.expose() {
            clipboard
                .set_text(String::new())
                .map_err(|_| ProjectKeysError::Clipboard)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_child_uses_retained_directory_after_path_replacement() {
        let root = std::env::temp_dir().join(format!("deppy-key-cwd-{}", uuid::Uuid::new_v4()));
        let old = root.with_extension("original");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("marker"), "original").unwrap();
        let directory = super::super::sources::AnchoredRoot::open(&root).unwrap();
        std::fs::rename(&root, &old).unwrap();
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("marker"), "replacement").unwrap();
        let output = NativeProcessRunner::default()
            .run_in_directory(
                &["/bin/cat".into(), "marker".into()],
                &directory,
                &ProcessEnvironment::default(),
                None,
                false,
            )
            .unwrap();
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(old).unwrap();
        assert_eq!(output.exit_code, 0);
        assert_eq!(output.stdout, b"original");
    }
    #[cfg(unix)]
    #[test]
    fn cancellation_idle_ack_waits_for_child_and_pipe_workers() {
        let runner = Arc::new(NativeProcessRunner::with_timeout(Duration::from_secs(5)));
        let running = runner.clone();
        let handle = std::thread::spawn(move || {
            running.run(
                &["/bin/sh".into(), "-c".into(), "sleep 5".into()],
                Path::new("/"),
                &ProcessEnvironment::default(),
                Some(&SecretString::new("x".repeat(256 * 1024))),
                false,
            )
        });
        let until = Instant::now() + Duration::from_secs(1);
        while runner.wait_for_idle(Duration::ZERO) && Instant::now() < until {
            std::thread::yield_now();
        }
        assert!(!runner.wait_for_idle(Duration::from_millis(10)));
        runner.cancel();
        assert!(runner.wait_for_idle(Duration::from_secs(1)));
        assert!(matches!(
            handle.join().unwrap(),
            Err(ProjectKeysError::Process)
        ));
    }
    #[cfg(unix)]
    #[test]
    fn foreground_cli_pty_helper() {
        if std::env::var_os("DEPPY_KEY_IO_PTY_FIXTURE").is_none() {
            return;
        }
        let runner = Arc::new(NativeProcessRunner::for_cli());
        let cancel = runner.clone();
        let timer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(700));
            cancel.cancel();
        });
        println!("pty-fixture-start");
        let result = runner.run(
            &[
                "/bin/sh".into(),
                "-c".into(),
                "read line; test \"$line\" = fixture-input".into(),
            ],
            Path::new("/"),
            &ProcessEnvironment::default(),
            None,
            true,
        );
        timer.join().unwrap();
        assert!(matches!(result, Ok(ProcessOutput { exit_code: 0, .. })));
        assert_eq!(unsafe { libc::tcgetpgrp(libc::STDIN_FILENO) }, unsafe {
            libc::getpgrp()
        });
    }
    #[cfg(unix)]
    #[test]
    fn foreground_cli_child_reads_controlling_terminal_and_restores_owner() {
        use std::os::{fd::FromRawFd, unix::process::CommandExt};
        let mut master_fd = -1;
        let mut slave_fd = -1;
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master_fd,
                    &mut slave_fd,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );
        let mut master = unsafe { std::fs::File::from_raw_fd(master_fd) };
        let slave = unsafe { std::fs::File::from_raw_fd(slave_fd) };
        nonblocking(&master).unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "project_keys::process::tests::foreground_cli_pty_helper",
                "--exact",
                "--nocapture",
            ])
            .env("DEPPY_KEY_IO_PTY_FIXTURE", "1")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0
                    || libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY as libc::c_ulong, 0) < 0
                    || libc::tcsetpgrp(libc::STDIN_FILENO, libc::getpgrp()) < 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        let until = Instant::now() + Duration::from_secs(3);
        let mut output = Vec::new();
        let mut sent = false;
        let status = loop {
            let mut bytes = [0u8; 1024];
            if let Ok(count) = master.read(&mut bytes) {
                output.extend_from_slice(&bytes[..count]);
            }
            if !sent && String::from_utf8_lossy(&output).contains("pty-fixture-start") {
                master.write_all(b"fixture-input\n").unwrap();
                sent = true;
            }
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= until {
                let _ = child.kill();
                break child.wait().unwrap();
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(
            status.success(),
            "isolated PTY child failed: {}",
            String::from_utf8_lossy(&output)
        );
        assert!(sent);
    }
    #[cfg(unix)]
    #[test]
    fn explicit_deadline_bounds_native_child_and_inherited_pipes() {
        let runner = NativeProcessRunner::with_timeout(Duration::from_secs(2));
        let start = Instant::now();
        assert!(matches!(
            runner.run_until(
                &["/bin/sh".into(), "-c".into(), "sleep 1".into()],
                Path::new("/"),
                &ProcessEnvironment::default(),
                None,
                false,
                start + Duration::from_millis(80),
            ),
            Err(ProjectKeysError::Process)
        ));
        assert!(start.elapsed() < Duration::from_millis(500));
    }
    #[cfg(unix)]
    #[test]
    fn inherited_descendant_pipe_and_blocked_stdin_obey_whole_operation_deadline() {
        let runner = NativeProcessRunner::with_timeout(Duration::from_millis(120));
        for script in ["sleep 5 & exit 0", "sleep 5"] {
            let start = Instant::now();
            let result = runner.run(
                &["/bin/sh".into(), "-c".into(), script.into()],
                Path::new("/"),
                &ProcessEnvironment::default(),
                Some(&SecretString::new("x".repeat(256 * 1024))),
                false,
            );
            assert!(matches!(result, Err(ProjectKeysError::Process)));
            assert!(start.elapsed() < Duration::from_secs(2));
        }
    }
}
