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
    /// Raw selected API values, including the contents of delivered temporary files.
    /// An empty list retains the legacy subprocess output behavior.
    pub output_secrets: Vec<SecretString>,
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

#[cfg(unix)]
struct TtyOutputSignalMask {
    previous: Option<libc::sigset_t>,
}
#[cfg(unix)]
impl TtyOutputSignalMask {
    fn acquire(target: libc::c_int) -> Result<Option<Self>, ProjectKeysError> {
        if unsafe { libc::isatty(target) } != 1 {
            return Ok(None);
        }
        let mut blocked = unsafe { std::mem::zeroed::<libc::sigset_t>() };
        let mut previous = unsafe { std::mem::zeroed::<libc::sigset_t>() };
        unsafe {
            libc::sigemptyset(&mut blocked);
            libc::sigaddset(&mut blocked, libc::SIGTTOU);
        }
        if unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, &mut previous) } != 0 {
            return Err(ProjectKeysError::Process);
        }
        Ok(Some(Self {
            previous: Some(previous),
        }))
    }

    fn restore(&mut self) -> Result<(), ProjectKeysError> {
        if let Some(previous) = &self.previous {
            if unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, previous, std::ptr::null_mut()) }
                != 0
            {
                return Err(ProjectKeysError::Process);
            }
            self.previous = None;
        }
        Ok(())
    }
}
#[cfg(unix)]
impl Drop for TtyOutputSignalMask {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

/// One bounded streaming projection. Only the child's pipe is made nonblocking: inherited
/// destination flags belong to the caller's open-file description and must stay unchanged.
#[cfg(unix)]
enum SelectedOutputReader {
    Stdout(std::process::ChildStdout),
    Stderr(std::process::ChildStderr),
    Merged(std::os::unix::net::UnixStream),
}
#[cfg(unix)]
impl Read for SelectedOutputReader {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Stdout(pipe) => pipe.read(bytes),
            Self::Stderr(pipe) => pipe.read(bytes),
            Self::Merged(pipe) => pipe.read(bytes),
        }
    }
}
#[cfg(unix)]
impl std::os::fd::AsRawFd for SelectedOutputReader {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        match self {
            Self::Stdout(pipe) => std::os::fd::AsRawFd::as_raw_fd(pipe),
            Self::Stderr(pipe) => std::os::fd::AsRawFd::as_raw_fd(pipe),
            Self::Merged(pipe) => std::os::fd::AsRawFd::as_raw_fd(pipe),
        }
    }
}

#[cfg(unix)]
fn shared_output_destination() -> Result<bool, ProjectKeysError> {
    let mut stdout = unsafe { std::mem::zeroed::<libc::stat>() };
    let mut stderr = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe { libc::fstat(libc::STDOUT_FILENO, &mut stdout) } != 0
        || unsafe { libc::fstat(libc::STDERR_FILENO, &mut stderr) } != 0
    {
        return Err(ProjectKeysError::Process);
    }
    // Include ordinary `> log 2>&1` files as well as PTYs/pipes. Conservatively merge two
    // separate opens of the same output object, too: independently delayed fragments could
    // otherwise reconstruct a raw secret in that object. Different objects stay separate.
    Ok(stdout.st_dev == stderr.st_dev
        && stdout.st_ino == stderr.st_ino
        && stdout.st_mode & libc::S_IFMT == stderr.st_mode & libc::S_IFMT
        && stdout.st_rdev == stderr.st_rdev)
}

#[cfg(unix)]
struct RedactedOutputPipe {
    pipe: SelectedOutputReader,
    target: libc::c_int,
    redactor: secret::FixedStreamRedactor,
    pending: Vec<u8>,
    offset: usize,
    eof: bool,
}
#[cfg(unix)]
impl RedactedOutputPipe {
    fn new(
        pipe: SelectedOutputReader,
        target: libc::c_int,
        redactor: secret::FixedStreamRedactor,
    ) -> Result<Self, ProjectKeysError> {
        nonblocking(&pipe)?;
        Ok(Self {
            pipe,
            target,
            redactor,
            pending: Vec::new(),
            offset: 0,
            eof: false,
        })
    }

    fn done(&self) -> bool {
        self.eof && self.pending.is_empty()
    }

    fn pump(&mut self) -> Result<bool, ProjectKeysError> {
        let mut progressed = false;
        if self.pending.is_empty() && !self.eof {
            let mut chunk = [0u8; 4096];
            match self.pipe.read(&mut chunk) {
                Ok(0) => {
                    self.eof = true;
                    self.pending = self.redactor.flush();
                    progressed = true;
                }
                Ok(count) => {
                    // FixedStreamRedactor bounds carry using the checked corpus maximum.
                    // Do not read another chunk until this projection has been forwarded.
                    self.pending = self.redactor.redact_chunk(&chunk[..count]);
                    progressed = true;
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(_) => return Err(ProjectKeysError::Process),
            }
        }
        if self.pending.is_empty() {
            return Ok(progressed);
        }
        let mut ready = libc::pollfd {
            fd: self.target,
            events: libc::POLLOUT,
            revents: 0,
        };
        let polled = unsafe { libc::poll(&mut ready, 1, 0) };
        if polled < 0 {
            return if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                Ok(progressed)
            } else {
                Err(ProjectKeysError::Process)
            };
        }
        if ready.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            return Err(ProjectKeysError::Process);
        }
        if ready.revents & libc::POLLOUT == 0 {
            return Ok(progressed);
        }
        // POSIX PIPE_BUF is at least 512. A single relay owns both streams and only writes
        // a small chunk after readiness; a blocked consumer leaves cancellation responsive.
        // Unrelated writers sharing a blocking destination can still race poll readiness,
        // as with ordinary inherited output. Never mutate that shared descriptor's flags.
        let bytes = &self.pending[self.offset..];
        // The command owns the foreground process group. With TOSTOP, its relay parent
        // must block SIGTTOU while writing the inherited tty (without changing disposition).
        let mut signal_mask = TtyOutputSignalMask::acquire(self.target)?;
        let count =
            unsafe { libc::write(self.target, bytes.as_ptr().cast(), bytes.len().min(512)) };
        // Capture errno before restoring the calling thread's prior mask.
        let written = if count < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(count as usize)
        };
        if let Some(mask) = &mut signal_mask {
            mask.restore()?;
        }
        match written {
            Ok(0) => Err(ProjectKeysError::Process),
            Ok(count) => {
                self.offset += count;
                if self.offset == self.pending.len() {
                    self.pending.clear();
                    self.offset = 0;
                }
                Ok(true)
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                Ok(progressed)
            }
            Err(_) => Err(ProjectKeysError::Process),
        }
    }
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
        let redaction: Option<(secret::FixedStreamRedactor, secret::RedactionLease)> =
            if environment.output_secrets.is_empty() {
                None
            } else {
                // Selected execution uses inherited stdin and relays both output streams. The
                // separate legacy helper protocol (captured stdout / explicit stdin) stays intact.
                if !inherit || stdin.is_some() {
                    return Err(ProjectKeysError::InvalidInput);
                }
                #[cfg(unix)]
                {
                    let service = secret::RedactionService::new();
                    let lease = service
                        .acquire_execution_lease(
                            &environment.output_secrets.iter().collect::<Vec<_>>(),
                        )
                        .map_err(|_| ProjectKeysError::Process)?;
                    Some((service.fixed_stream_redactor(), lease))
                }
                #[cfg(not(unix))]
                {
                    return Err(ProjectKeysError::Process);
                }
            };
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
        cmd.stdout(if inherit && redaction.is_none() {
            Stdio::inherit()
        } else {
            Stdio::piped()
        });
        // Legacy builds retain direct terminal output. Selected API output is filtered before
        // reaching the caller's PTY, model, or session logger.
        cmd.stderr(if redaction.is_some() {
            Stdio::piped()
        } else if inherit {
            Stdio::inherit()
        } else {
            Stdio::null()
        });
        #[cfg(unix)]
        let merged_output = if redaction.is_some() && shared_output_destination()? {
            use std::os::fd::OwnedFd;
            let (reader, writer) =
                std::os::unix::net::UnixStream::pair().map_err(|_| ProjectKeysError::Process)?;
            let stderr = writer.try_clone().map_err(|_| ProjectKeysError::Process)?;
            cmd.stdout(Stdio::from(OwnedFd::from(writer)));
            cmd.stderr(Stdio::from(OwnedFd::from(stderr)));
            Some(reader)
        } else {
            None
        };
        #[cfg(unix)]
        if redaction.is_some()
            && self.interactive_signals
            && CLI_INTERRUPTED.load(Ordering::Acquire)
        {
            self.cancel();
        }
        if redaction.is_some()
            && (self.cancelled.load(Ordering::Acquire)
                || (operation_deadline.is_some() && Instant::now() >= deadline))
        {
            return Err(ProjectKeysError::Process);
        }
        let mut child = cmd.spawn().map_err(|_| ProjectKeysError::Process)?;
        // Command retains explicitly supplied Stdio handles after spawn. Close its private
        // merged write ends now, or EOF would never reach the relay after the child exits.
        drop(cmd);
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
        #[cfg(unix)]
        if let Some((redactor, _lease)) = &redaction {
            return self.relay_selected_output(
                child,
                merged_output,
                redactor,
                operation_deadline.map(|_| deadline),
            );
        }
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

    #[cfg(unix)]
    fn relay_selected_output(
        &self,
        mut child: std::process::Child,
        merged_output: Option<std::os::unix::net::UnixStream>,
        redactor: &secret::FixedStreamRedactor,
        deadline: Option<Instant>,
    ) -> Result<ProcessOutput, ProjectKeysError> {
        let result = (|| {
            let mut outputs = if let Some(pipe) = merged_output {
                vec![RedactedOutputPipe::new(
                    SelectedOutputReader::Merged(pipe),
                    libc::STDOUT_FILENO,
                    redactor.new_stream(),
                )?]
            } else {
                vec![
                    RedactedOutputPipe::new(
                        SelectedOutputReader::Stdout(
                            child.stdout.take().ok_or(ProjectKeysError::Process)?,
                        ),
                        libc::STDOUT_FILENO,
                        redactor.new_stream(),
                    )?,
                    RedactedOutputPipe::new(
                        SelectedOutputReader::Stderr(
                            child.stderr.take().ok_or(ProjectKeysError::Process)?,
                        ),
                        libc::STDERR_FILENO,
                        redactor.new_stream(),
                    )?,
                ]
            };
            let mut status = None;
            loop {
                if self.interactive_signals && CLI_INTERRUPTED.load(Ordering::Acquire) {
                    self.cancel();
                }
                if self.cancelled.load(Ordering::Acquire)
                    || deadline.is_some_and(|end| Instant::now() >= end)
                {
                    return Err(ProjectKeysError::Process);
                }
                if status.is_none() {
                    status = child.try_wait().map_err(|_| ProjectKeysError::Process)?;
                    if status.is_some() {
                        // Descendants must not retain the command's output pipes after the
                        // direct child exits. Drain their already-written bytes, then flush.
                        unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
                    }
                }
                let mut progressed = false;
                for output in &mut outputs {
                    progressed |= output.pump()?;
                }
                if let Some(status) = status
                    && outputs.iter().all(RedactedOutputPipe::done)
                {
                    return Ok(ProcessOutput {
                        exit_code: status.code().unwrap_or(1),
                        stdout: vec![],
                    });
                }
                if !progressed {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        })();
        // This includes pipe preparation, cancellation, deadline, and downstream failures.
        // The relay owns no output worker threads or unbounded output capture.
        terminate(&mut child);
        result
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
    #[cfg(unix)]
    #[test]
    fn selected_inherited_output_preserves_timeout_and_explicit_deadline_semantics() {
        let mut environment = ProcessEnvironment::default();
        environment
            .output_secrets
            .push(SecretString::new("selected-fixture-secret".into()));
        let runner = NativeProcessRunner::with_timeout(Duration::from_millis(20));
        let started = Instant::now();
        let output = runner
            .run(
                &["/bin/sh".into(), "-c".into(), "sleep 0.1".into()],
                Path::new("/"),
                &environment,
                None,
                true,
            )
            .unwrap();
        assert_eq!(output.exit_code, 0);
        assert!(started.elapsed() >= Duration::from_millis(80));
        let runner = NativeProcessRunner::with_timeout(Duration::from_secs(2));
        let started = Instant::now();
        assert!(matches!(
            runner.run_until(
                &["/bin/sh".into(), "-c".into(), "sleep 1".into()],
                Path::new("/"),
                &environment,
                None,
                true,
                started + Duration::from_millis(80),
            ),
            Err(ProjectKeysError::Process)
        ));
        assert!(started.elapsed() < Duration::from_millis(500));
        assert!(runner.wait_for_idle(Duration::ZERO));

        let started = Instant::now();
        let output = runner
            .run(
                &["/bin/sh".into(), "-c".into(), "sleep 5 & exit 0".into()],
                Path::new("/"),
                &environment,
                None,
                true,
            )
            .unwrap();
        assert_eq!(output.exit_code, 0);
        assert!(started.elapsed() < Duration::from_millis(500));
    }

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
