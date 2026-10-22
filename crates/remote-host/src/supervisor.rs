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
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let output_done = Arc::clone(&done);
    let output = std::thread::Builder::new()
        .name("ssh-stdio-output".into())
        .spawn(move || -> anyhow::Result<()> {
            let result = (|| -> anyhow::Result<()> {
                let mut downstream = downstream;
                let mut output = std::io::stdout();
                let mut buffer = [0; 16 * 1024];
                loop {
                    let count = downstream.read(&mut buffer)?;
                    if count == 0 {
                        break;
                    }
                    output.write_all(&buffer[..count])?;
                    output.flush()?;
                }
                Ok(())
            })();
            output_done.store(true, std::sync::atomic::Ordering::Release);
            result
        })?;
    let result = (|| -> std::io::Result<()> {
        let mut buffer = [0; 16 * 1024];
        while !done.load(std::sync::atomic::Ordering::Acquire) {
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
        .filter(|seconds| (1..=1800).contains(seconds))
        .unwrap_or(300);
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
