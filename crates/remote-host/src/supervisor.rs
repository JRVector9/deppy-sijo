//! SSH stdio is a disposable bridge. This private Unix-socket process owns PTYs.
use anyhow::{Context, ensure};
use runtime::{
    InProcessRuntimeClient, RuntimeSecret, RuntimeSecretResolver, SshClientHello, SshRpcHandler,
    SshServerConfig,
};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const ATTACH_TIMEOUT: Duration = Duration::from_secs(5);
const IO_TIMEOUT: Duration = Duration::from_secs(45);

/// Obtained from the effective uid, never from a client-provided USER variable.
pub fn current_account() -> anyhow::Result<String> {
    let mut value = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut result = std::ptr::null_mut();
    let mut buffer = vec![0u8; 64 * 1024];
    // getpwuid_r writes only into the supplied buffers; C strings remain alive
    // until they are copied below. No process-global passwd entry is retained.
    let status = unsafe {
        libc::getpwuid_r(
            libc::geteuid(),
            value.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    ensure!(
        status == 0 && !result.is_null(),
        "ssh_account_lookup_failed"
    );
    let value = unsafe { value.assume_init() };
    ensure!(!value.pw_name.is_null(), "ssh_account_lookup_failed");
    Ok(unsafe { std::ffi::CStr::from_ptr(value.pw_name) }
        .to_str()?
        .to_owned())
}

fn validate_identity(hello: &SshClientHello) -> anyhow::Result<()> {
    hello
        .identity
        .validate()
        .map_err(|_| runtime::SshHandshakeError::InvalidRequest)?;
    ensure!(
        hello.identity.account == current_account()?,
        runtime::SshHandshakeError::TargetMismatch
    );
    let root = std::fs::canonicalize(&hello.identity.canonical_root)
        .map_err(|_| runtime::SshHandshakeError::TargetMismatch)?;
    ensure!(
        root.is_dir() && root.to_str() == Some(&hello.identity.canonical_root),
        runtime::SshHandshakeError::TargetMismatch
    );
    ensure!(
        hello.identity.remote_workspace_id
            == crate::services::workspace_id(
                &hello.identity.account,
                &hello.identity.canonical_root
            ),
        runtime::SshHandshakeError::TargetMismatch
    );
    // Reject protocol, size and generation before creating any process or state.
    SshServerConfig::new(hello.identity.clone(), "preflight".into()).negotiate(
        &SshClientHello {
            resume: None,
            ..hello.clone()
        },
    )?;
    Ok(())
}

fn private_directory(path: &Path) -> anyhow::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.is_dir()
                    && metadata.uid() == unsafe { libc::geteuid() }
                    && metadata.mode() & 0o077 == 0,
                "ssh_state_directory_not_private"
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(path)?;
            let metadata = std::fs::symlink_metadata(path)?;
            ensure!(
                metadata.is_dir()
                    && metadata.uid() == unsafe { libc::geteuid() }
                    && metadata.mode() & 0o077 == 0,
                "ssh_state_directory_not_private"
            );
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

struct StatePaths {
    socket: PathBuf,
    lock: PathBuf,
    logs: PathBuf,
}
fn state_paths(hello: &SshClientHello) -> anyhow::Result<StatePaths> {
    let home = PathBuf::from(std::env::var_os("HOME").context("ssh_home_missing")?);
    ensure!(home.is_absolute(), "ssh_home_invalid");
    let mut base = home.join(".cache/deppy/ssh");
    // macOS has a 104-byte sockaddr_un path; Linux's limit is 108. A private
    // per-uid fallback keeps both development fixtures and long homes valid.
    if base.as_os_str().as_encoded_bytes().len() > 57 {
        base = std::env::temp_dir().join(format!("deppy-ssh-{}", unsafe { libc::geteuid() }));
    }
    private_directory(&base)?;
    let mut digest = Sha256::new();
    digest.update(postcard::to_stdvec(&hello.identity)?);
    digest.update(env!("CARGO_PKG_VERSION").as_bytes());
    let hash: String = digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let prefix = &hash[..30];
    let logs = base.join(format!("{prefix}-logs"));
    private_directory(&logs)?;
    Ok(StatePaths {
        socket: base.join(format!("{prefix}.sock")),
        lock: base.join(format!("{prefix}.lock")),
        logs,
    })
}

fn acquire_lock(path: &Path) -> anyhow::Result<File> {
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = lock.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "ssh_state_lock_not_private"
    );
    let deadline = Instant::now() + ATTACH_TIMEOUT;
    loop {
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(lock);
        }
        let error = std::io::Error::last_os_error();
        ensure!(
            error.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline,
            "ssh_supervisor_lock_timeout"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn read_hello(reader: &mut impl Read) -> anyhow::Result<SshClientHello> {
    let bytes = runtime::read_ssh_frame(reader, 64 * 1024)?.context("ssh_hello_missing")?;
    let hello = postcard::from_bytes(&bytes)?;
    validate_identity(&hello)?;
    Ok(hello)
}

fn remove_stale_socket(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::FileTypeExt;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.file_type().is_socket() && metadata.uid() == unsafe { libc::geteuid() },
                "ssh_socket_not_owned"
            );
            std::fs::remove_file(path)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn duplicate_stdio_input() -> anyhow::Result<File> {
    // This bridge descriptor must not survive the independent supervisor exec.
    let fd = unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_DUPFD_CLOEXEC, 0) };
    ensure!(fd >= 0, "ssh_stdin_unavailable");
    Ok(unsafe { File::from_raw_fd(fd) })
}

struct BridgeOutput {
    file: File,
    original_flags: libc::c_int,
}
impl BridgeOutput {
    fn stdout() -> std::io::Result<Self> {
        let fd = unsafe { libc::fcntl(libc::STDOUT_FILENO, libc::F_DUPFD_CLOEXEC, 0) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Self::new(unsafe { File::from_raw_fd(fd) })
    }

    fn new(file: File) -> std::io::Result<Self> {
        let original_flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
        if original_flags < 0
            || unsafe {
                libc::fcntl(
                    file.as_raw_fd(),
                    libc::F_SETFL,
                    original_flags | libc::O_NONBLOCK,
                )
            } < 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self {
            file,
            original_flags,
        })
    }

    /// Polling alone cannot bound a blocking pipe write. O_NONBLOCK makes every
    /// actual write interruptible; a live reader that stops consuming cannot
    /// trap the bridge's output thread after input EOF or socket shutdown.
    fn write_all(
        &mut self,
        mut bytes: &[u8],
        done: &AtomicBool,
        idle_timeout: Duration,
    ) -> std::io::Result<bool> {
        let mut last_progress = Instant::now();
        while !bytes.is_empty() {
            if done.load(Ordering::Acquire) {
                return Ok(false);
            }
            match self.file.write(bytes) {
                Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
                Ok(count) => {
                    bytes = &bytes[count..];
                    last_progress = Instant::now();
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    let remaining = idle_timeout.saturating_sub(last_progress.elapsed());
                    if remaining.is_zero() {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "ssh_bridge_output_timeout",
                        ));
                    }
                    let mut pollfd = libc::pollfd {
                        fd: self.file.as_raw_fd(),
                        events: libc::POLLOUT,
                        revents: 0,
                    };
                    let timeout_ms = remaining.as_millis().clamp(1, 100) as libc::c_int;
                    if unsafe { libc::poll(&mut pollfd, 1, timeout_ms) } < 0 {
                        let error = std::io::Error::last_os_error();
                        if error.kind() != std::io::ErrorKind::Interrupted {
                            return Err(error);
                        }
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Ok(true)
    }
}
impl Drop for BridgeOutput {
    fn drop(&mut self) {
        // dup shares file-status flags with stdout. Restore those flags before
        // releasing our descriptor, including error and cancelled exits.
        unsafe {
            libc::fcntl(self.file.as_raw_fd(), libc::F_SETFL, self.original_flags);
        }
    }
}

fn prepare_stdio() -> anyhow::Result<(File, SshClientHello, UnixStream)> {
    let mut input = duplicate_stdio_input()?;
    let hello = read_hello(&mut input)?;
    let paths = state_paths(&hello)?;
    let lock = acquire_lock(&paths.lock)?;
    let stream = match UnixStream::connect(&paths.socket) {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            ensure!(
                hello.resume.is_none(),
                runtime::SshHandshakeError::ResumeExpired
            );
            remove_stale_socket(&paths.socket)?;
            let mut command = Command::new(std::env::current_exe()?);
            command
                .arg("--supervisor")
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            crate::agent_environment::configure_child(&mut command);
            // The supervisor receives no SSH PTY/control terminal and survives the
            // bridge's SIGHUP. It owns only its own descendants, never other apps.
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() < 0 {
                        Err(std::io::Error::last_os_error())
                    } else {
                        Ok(())
                    }
                });
            }
            // SSH login accounts normally set SHELL. Minimal Linux containers do
            // not; choose a universally available shell without mutating env.
            if std::env::var_os("SHELL").is_none() {
                command.env("SHELL", "/bin/sh");
            }
            let mut child = command.spawn().context("ssh_supervisor_spawn_failed")?;
            {
                let mut stdin = child.stdin.take().context("ssh_supervisor_stdin_missing")?;
                runtime::write_ssh_frame(&mut stdin, &postcard::to_stdvec(&hello)?, 64 * 1024)?;
            }
            let deadline = Instant::now() + ATTACH_TIMEOUT;
            loop {
                match UnixStream::connect(&paths.socket) {
                    Ok(stream) => break stream,
                    Err(_) if Instant::now() < deadline && child.try_wait()?.is_none() => {
                        std::thread::sleep(Duration::from_millis(10))
                    }
                    Err(error) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(error).context("ssh_supervisor_unavailable");
                    }
                }
            }
        }
        Err(error) => return Err(error).context("ssh_supervisor_connect_failed"),
    };
    drop(lock);
    Ok((input, hello, stream))
}

/// Forward frames only; roots, credentials and commands are never shell argv.
pub fn run_stdio() -> anyhow::Result<()> {
    let (mut input, hello, stream) = match prepare_stdio() {
        Ok(prepared) => prepared,
        Err(error) => {
            let code = error
                .downcast_ref::<runtime::SshHandshakeError>()
                .copied()
                .unwrap_or(runtime::SshHandshakeError::Unavailable);
            runtime::write_ssh_handshake_rejection(&mut std::io::stdout(), code)?;
            return Err(error);
        }
    };
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut upstream = stream.try_clone()?;
    runtime::write_ssh_frame(&mut upstream, &postcard::to_stdvec(&hello)?, 64 * 1024)?;
    let downstream = stream.try_clone()?;
    let done = Arc::new(AtomicBool::new(false));
    let output_done = Arc::clone(&done);
    let stdout = BridgeOutput::stdout()?;
    let output = std::thread::Builder::new()
        .name("ssh-stdio-output".into())
        .spawn(move || -> anyhow::Result<()> {
            let result = (|| -> anyhow::Result<()> {
                let mut downstream = downstream;
                let mut output = stdout;
                let mut buffer = [0; 16 * 1024];
                loop {
                    let count = downstream.read(&mut buffer)?;
                    if count == 0 {
                        break;
                    }
                    if !output.write_all(&buffer[..count], &output_done, IO_TIMEOUT)? {
                        break;
                    }
                }
                Ok(())
            })();
            output_done.store(true, Ordering::Release);
            result
        })?;
    let result = (|| -> std::io::Result<()> {
        let mut buffer = [0; 16 * 1024];
        while !done.load(Ordering::Acquire) {
            let mut pollfd = libc::pollfd {
                fd: input.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut pollfd, 1, 100) };
            if ready < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if ready == 0 {
                continue;
            }
            let count = input.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            upstream.write_all(&buffer[..count])?;
            upstream.flush()?;
        }
        Ok(())
    })();
    done.store(true, Ordering::Release);
    let _ = stream.shutdown(std::net::Shutdown::Both);
    let output_result = output
        .join()
        .map_err(|_| anyhow::anyhow!("ssh_bridge_output_panicked"))?;
    result?;
    output_result?;
    Ok(())
}

struct RemoteSecrets;
impl RuntimeSecretResolver for RemoteSecrets {
    fn resolve(&self, _logical_credential_id: &str) -> anyhow::Result<RuntimeSecret> {
        // Remote processes inherit their server account environment. Mac logical
        // credentials can never resolve against this helper.
        anyhow::bail!("ssh_remote_credential_unavailable")
    }
}

struct SocketCleanup(PathBuf);
impl Drop for SocketCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub fn run_supervisor(handler: SshRpcHandler) -> anyhow::Result<()> {
    let executor = Arc::new(runtime::SshRpcExecutor::new(handler)?);
    let hello = read_hello(&mut std::io::stdin())?;
    let paths = state_paths(&hello)?;
    let listener = UnixListener::bind(&paths.socket)?;
    std::fs::set_permissions(&paths.socket, std::fs::Permissions::from_mode(0o600))?;
    let _cleanup = SocketCleanup(paths.socket);
    listener.set_nonblocking(true)?;
    let mut backend = InProcessRuntimeClient::try_new_with_resolver(
        5,
        Arc::new(RemoteSecrets),
        paths.logs,
        secret::RedactionService::new(),
        None,
        Some(PathBuf::from(&hello.identity.canonical_root)),
        Vec::new(),
    )?;
    let mut config = SshServerConfig::new(hello.identity, backend.instance_token().to_owned());
    config.lease_ttl_secs = std::env::var("DEPPY_REMOTE_LEASE_SECONDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|seconds| (1..=SshServerConfig::MAX_LEASE_TTL_SECS).contains(seconds))
        .unwrap_or(SshServerConfig::DEFAULT_LEASE_TTL_SECS);
    let mut detached_at = Instant::now();
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                // BSD/macOS accepted sockets inherit the listener nonblocking flag.
                // The framing worker uses blocking reads with explicit deadlines.
                stream.set_nonblocking(false)?;
                stream.set_read_timeout(Some(IO_TIMEOUT))?;
                stream.set_write_timeout(Some(IO_TIMEOUT))?;
                // Reject malformed or stale identity/lease hellos before any
                // command can reach the retained runtime. Generation is local
                // to this attachment; a matching lease permits an app restart.
                let mut preflight = stream.try_clone()?;
                preflight.set_read_timeout(Some(ATTACH_TIMEOUT))?;
                let bytes = match runtime::read_ssh_frame(&mut preflight, 64 * 1024) {
                    Ok(Some(bytes)) => bytes,
                    _ => {
                        let _ = runtime::write_ssh_handshake_rejection(
                            &mut stream,
                            runtime::SshHandshakeError::InvalidRequest,
                        );
                        let _ = stream.shutdown(std::net::Shutdown::Both);
                        continue;
                    }
                };
                let request = match postcard::from_bytes::<SshClientHello>(&bytes) {
                    Ok(request) => request,
                    Err(_) => {
                        let _ = runtime::write_ssh_handshake_rejection(
                            &mut stream,
                            runtime::SshHandshakeError::InvalidRequest,
                        );
                        let _ = stream.shutdown(std::net::Shutdown::Both);
                        continue;
                    }
                };
                if let Err(error) = config.negotiate(&request) {
                    let code = error
                        .downcast_ref::<runtime::SshHandshakeError>()
                        .copied()
                        .unwrap_or(runtime::SshHandshakeError::InvalidRequest);
                    let _ = runtime::write_ssh_handshake_rejection(&mut stream, code);
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                    continue;
                }
                stream.set_read_timeout(Some(IO_TIMEOUT))?;
                let close = stream.try_clone()?;
                let mut prefix = Vec::with_capacity(bytes.len() + 4);
                prefix.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
                prefix.extend_from_slice(&bytes);
                let reader = std::io::Cursor::new(prefix).chain(stream.try_clone()?);
                let _ = runtime::serve_ssh_stream_with_rpc_executor(
                    &backend,
                    reader,
                    stream,
                    &config,
                    Arc::clone(&executor),
                    Arc::new(move || {
                        let _ = close.shutdown(std::net::Shutdown::Both);
                    }),
                );
                config.require_resume = true;
                detached_at = Instant::now();
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if detached_at.elapsed() >= Duration::from_secs(config.lease_ttl_secs as u64) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error.into()),
        }
    }
    backend.shutdown();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_output_pipe() -> (File, BridgeOutput, File) {
        let mut descriptors = [-1; 2];
        assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
        for descriptor in descriptors {
            assert_eq!(
                unsafe { libc::fcntl(descriptor, libc::F_SETFD, libc::FD_CLOEXEC) },
                0
            );
        }
        let reader = unsafe { File::from_raw_fd(descriptors[0]) };
        let writer = unsafe { File::from_raw_fd(descriptors[1]) };
        let duplicate = writer.try_clone().unwrap();
        let mut output = BridgeOutput::new(writer).unwrap();
        assert_ne!(
            unsafe { libc::fcntl(output.file.as_raw_fd(), libc::F_GETFL) } & libc::O_NONBLOCK,
            0,
            "the real descriptor must be nonblocking before testing a full pipe"
        );
        loop {
            match output.file.write(&[0; 16 * 1024]) {
                Ok(count) => assert_ne!(count, 0),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("fill stdout fixture: {error}"),
            }
        }
        (reader, output, duplicate)
    }

    #[test]
    fn bridge_output_full_pipe_honors_cancel_and_restores_descriptor_flags() {
        let (_reader, mut output, duplicate) = full_output_pipe();
        let done = Arc::new(AtomicBool::new(false));
        let cancel = Arc::clone(&done);
        let worker = std::thread::spawn(move || {
            output
                .write_all(b"pending", &cancel, Duration::from_secs(5))
                .unwrap()
        });
        std::thread::sleep(Duration::from_millis(20));
        let started = Instant::now();
        done.store(true, Ordering::Release);
        assert!(!worker.join().unwrap());
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(
            unsafe { libc::fcntl(duplicate.as_raw_fd(), libc::F_GETFL) } & libc::O_NONBLOCK,
            0,
            "a duplicate observes restored flags after cancellation"
        );
    }

    #[test]
    fn bridge_output_full_pipe_honors_idle_deadline_with_live_reader() {
        let (_reader, mut output, _duplicate) = full_output_pipe();
        let started = Instant::now();
        let error = output
            .write_all(
                b"pending",
                &AtomicBool::new(false),
                Duration::from_millis(150),
            )
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() >= Duration::from_millis(150));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn bridge_output_slow_progress_resets_idle_deadline_without_losing_bytes() {
        let (mut reader, mut output, duplicate) = full_output_pipe();
        drop(duplicate);
        let mut filled: libc::c_int = 0;
        assert_eq!(
            unsafe { libc::ioctl(reader.as_raw_fd(), libc::FIONREAD, &mut filled) },
            0
        );
        let filled = usize::try_from(filled).unwrap();
        assert!(filled >= 4096);
        let expected: Vec<_> = (0..filled * 32).map(|index| (index % 251) as u8).collect();
        let payload = expected.clone();
        let idle_timeout = Duration::from_secs(1);
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            let started = Instant::now();
            started_tx.send(()).unwrap();
            let result = output.write_all(&payload, &AtomicBool::new(false), idle_timeout);
            (result, started.elapsed())
        });
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        let mut received = vec![0; filled];
        reader.read_exact(&mut received).unwrap();
        assert!(received.iter().all(|byte| *byte == 0));
        // Twelve small drains keep every idle gap far below the one-second
        // deadline while requiring the same write_all call to last longer than
        // that deadline. More payload remains than the pipe can buffer, so this
        // exercises repeated WouldBlock/partial-write cycles before the fast drain.
        for _ in 0..12 {
            std::thread::sleep(Duration::from_millis(100));
            let start = received.len();
            received.resize(start + filled / 2, 0);
            reader.read_exact(&mut received[start..]).unwrap();
        }
        reader.read_to_end(&mut received).unwrap();
        let (result, elapsed) = worker.join().unwrap();
        assert!(result.unwrap());
        assert!(elapsed > idle_timeout);
        assert_eq!(&received[filled..], expected);
    }

    struct LeaseEnvProbe {
        child: std::process::Child,
        home: PathBuf,
    }

    impl Drop for LeaseEnvProbe {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = std::fs::remove_dir_all(&self.home);
        }
    }

    #[test]
    #[ignore = "isolated supervisor subprocess, invoked by the environment boundary test"]
    fn supervisor_lease_env_process_fixture() {
        assert_eq!(
            std::env::var("DEPPY_TEST_SUPERVISOR_LEASE").as_deref(),
            Ok("1")
        );
        run_supervisor(Arc::new(crate::services::handle_rpc)).unwrap();
    }

    #[test]
    fn supervisor_lease_environment_accepts_thirty_days_and_defaults_invalid_values() {
        for (override_value, expected) in [
            (None, SshServerConfig::DEFAULT_LEASE_TTL_SECS),
            (Some("1"), 1),
            (Some("604800"), 7 * 24 * 60 * 60),
            (Some("2592000"), 30 * 24 * 60 * 60),
            (Some("0"), SshServerConfig::DEFAULT_LEASE_TTL_SECS),
            (Some("2592001"), SshServerConfig::DEFAULT_LEASE_TTL_SECS),
            (Some("4294967295"), SshServerConfig::DEFAULT_LEASE_TTL_SECS),
            (Some("invalid"), SshServerConfig::DEFAULT_LEASE_TTL_SECS),
        ] {
            // A short, private HOME keeps the socket below sockaddr_un's path
            // limit and keeps every file inside this subprocess fixture's root.
            let unique = uuid::Uuid::new_v4().simple().to_string();
            let home = PathBuf::from("/tmp").join(format!("ds-lease-{}", &unique[..16]));
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&home)
                .unwrap();
            let base = home.join(".cache/deppy/ssh");
            assert!(base.as_os_str().as_encoded_bytes().len() <= 57);
            let canonical_root = std::fs::canonicalize(&home).unwrap();
            let account = current_account().unwrap();
            let hello = SshClientHello::new(
                runtime::SshTargetIdentity {
                    remote_workspace_id: crate::services::workspace_id(
                        &account,
                        canonical_root.to_str().unwrap(),
                    ),
                    account,
                    canonical_root: canonical_root.to_str().unwrap().into(),
                    host_key_sha256: "SHA256:lease-env-fixture".into(),
                },
                1,
            );
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "supervisor::tests::supervisor_lease_env_process_fixture",
                    "--ignored",
                    "--nocapture",
                ])
                .env("HOME", &home)
                .env("SHELL", "/bin/sh")
                .env("DEPPY_TEST_SUPERVISOR_LEASE", "1")
                .env_remove("DEPPY_REMOTE_LEASE_SECONDS")
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            if let Some(value) = override_value {
                command.env("DEPPY_REMOTE_LEASE_SECONDS", value);
            }
            let mut probe = LeaseEnvProbe {
                child: command.spawn().unwrap(),
                home,
            };
            runtime::write_ssh_frame(
                probe.child.stdin.as_mut().unwrap(),
                &postcard::to_stdvec(&hello).unwrap(),
                64 * 1024,
            )
            .unwrap();
            drop(probe.child.stdin.take());
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                if let Ok(entries) = std::fs::read_dir(&base) {
                    let socket = entries
                        .filter_map(Result::ok)
                        .map(|entry| entry.path())
                        .find(|path| {
                            path.extension()
                                .is_some_and(|extension| extension == "sock")
                        });
                    if let Some(socket) = socket
                        && let Ok(stream) = UnixStream::connect(socket)
                    {
                        break stream;
                    }
                }
                assert!(
                    probe.child.try_wait().unwrap().is_none(),
                    "supervisor fixture exited before accepting {override_value:?}"
                );
                assert!(
                    Instant::now() < deadline,
                    "supervisor fixture startup deadline"
                );
                std::thread::sleep(Duration::from_millis(10));
            };
            stream.set_read_timeout(Some(ATTACH_TIMEOUT)).unwrap();
            stream.set_write_timeout(Some(ATTACH_TIMEOUT)).unwrap();
            runtime::write_ssh_frame(
                &mut stream,
                &postcard::to_stdvec(&hello).unwrap(),
                64 * 1024,
            )
            .unwrap();
            let bytes = runtime::read_ssh_frame(&mut stream, 64 * 1024)
                .unwrap()
                .unwrap();
            let response = postcard::from_bytes::<
                Result<runtime::SshServerHello, runtime::SshHandshakeError>,
            >(&bytes)
            .unwrap()
            .unwrap();
            assert_eq!(
                response.lease_ttl_secs, expected,
                "override {override_value:?}"
            );
        }
    }

    #[test]
    fn supervisor_never_inherits_bridge_input_descriptor() {
        let input = duplicate_stdio_input().unwrap();
        let flags = unsafe { libc::fcntl(input.as_raw_fd(), libc::F_GETFD) };
        assert!(flags >= 0);
        assert_ne!(
            flags & libc::FD_CLOEXEC,
            0,
            "independent supervisor must release SSH stdio on exec"
        );
    }
}
