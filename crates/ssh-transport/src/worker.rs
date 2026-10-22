use super::*;
use ssh_askpass::{MAX_REQUEST_BYTES, SOCKET_ENV, TOKEN_ENV};
use std::{
    collections::VecDeque,
    fs::{self, DirBuilder, File},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
            net::{UnixListener, UnixStream},
            process::CommandExt,
        },
    },
    process::{Child, Command, Stdio},
    sync::mpsc::{TryRecvError, sync_channel},
};
const TICK: Duration = Duration::from_millis(2);
const MAX_CONFIG: usize = 128 * 1024;

pub(super) fn spawn(options: SshLaunchOptions) -> io::Result<(SshWorker, SshStream)> {
    options.destination.validate()?;
    if options.port == Some(0)
        || options.connect_timeout.is_zero()
        || options.io_timeout.is_zero()
        || !options.ssh_program.is_absolute()
        || !options.askpass_program.is_absolute()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid SSH worker options",
        ));
    }
    let (events_tx, events) = sync_channel(QUEUE_CAP);
    let (replies, replies_rx) = sync_channel(4);
    let (outgoing_tx, outgoing) = sync_channel(QUEUE_CAP);
    let (incoming_tx, incoming) = sync_channel(QUEUE_CAP);
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_cancelled = cancelled.clone();
    let authenticated = Arc::new(AtomicBool::new(false));
    let worker_authenticated = authenticated.clone();
    let timeout = options.io_timeout;
    thread::Builder::new()
        .name("deppy-ssh".into())
        .spawn(move || {
            let result = run(
                &options,
                (&worker_cancelled, &worker_authenticated),
                &events_tx,
                replies_rx,
                outgoing,
                incoming_tx,
            );
            // run returns only after child and pipes are reclaimed. The event never exposes raw stderr.
            let _ = events_tx.try_send(SshEvent::Ended(result));
        })?;
    Ok((
        SshWorker {
            cancelled: cancelled.clone(),
            authenticated,
            events,
            replies,
        },
        SshStream {
            reader: SshReader {
                incoming,
                buffered: Vec::new(),
                offset: 0,
                timeout: None,
            },
            writer: SshWriter {
                outgoing: outgoing_tx,
                cancelled,
                timeout,
            },
        },
    ))
}
fn emit(events: &SyncSender<SshEvent>, event: SshEvent) -> Result<(), FailureKind> {
    events
        .try_send(event)
        .map_err(|_| FailureKind::Backpressure)
}
fn nonblocking(fd: &impl AsRawFd) -> io::Result<()> {
    let raw = fd.as_raw_fd();
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
/// The child stays unreaped while checking exit so its process-group ID cannot be reused before cleanup.
struct OwnedChild(Child);
impl OwnedChild {
    fn exit_code(&self) -> io::Result<Option<i32>> {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                self.0.id(),
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result == -1 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { info.si_pid() } == 0 {
            return Ok(None);
        }
        let status = unsafe { info.si_status() };
        Ok(Some(if info.si_code == libc::CLD_EXITED {
            status
        } else {
            128 + status
        }))
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        // process_group(0) gave this worker exclusive ownership of SSH, ProxyJump and askpass descendants.
        unsafe {
            libc::kill(-(self.0.id() as i32), libc::SIGKILL);
        }
        let _ = self.0.wait();
    }
}
fn base_command(
    options: &SshLaunchOptions,
    resolve: bool,
    diagnostics: Option<&std::path::Path>,
) -> Command {
    let mut cmd = Command::new(&options.ssh_program);
    cmd.process_group(0).env("LC_ALL", "C").env("LANG", "C");
    if resolve {
        cmd.arg("-G");
    }
    cmd.args([
        "-T",
        "-o",
        "StrictHostKeyChecking=ask",
        "-o",
        "FingerprintHash=sha256",
        "-o",
        "UpdateHostKeys=no",
        "-o",
        "VerifyHostKeyDNS=no",
        "-o",
        "ControlMaster=no",
        "-o",
        "ControlPath=none",
        "-o",
        "ControlPersist=no",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=3",
        "-o",
        "TCPKeepAlive=yes",
        "-o",
        "ForwardAgent=no",
        "-o",
        "ClearAllForwardings=yes",
        "-o",
        "LogLevel=DEBUG1",
        "-o",
        "LogVerbose=none",
        "-o",
        "BatchMode=no",
        "-o",
        "PermitLocalCommand=no",
        "-o",
        "RequestTTY=no",
        "-o",
        "RemoteCommand=none",
        "-o",
        "SessionType=default",
        "-o",
        "StdinNull=no",
        "-o",
        "ForkAfterAuthentication=no",
    ]);
    if let Some(path) = diagnostics {
        cmd.arg("-E").arg(path);
    }
    if let Some(port) = options.port.or(options.destination.port) {
        cmd.arg("-p").arg(port.to_string());
    }
    if let Some(user) = &options.destination.user {
        cmd.arg("-l").arg(user);
    }
    if let Some(identity) = &options.identity_file {
        cmd.arg("-i").arg(identity);
    }
    cmd.arg("--").arg(&options.destination.host);
    cmd.arg(options.command.script());
    cmd
}
fn resolve(
    options: &SshLaunchOptions,
    cancelled: &AtomicBool,
    deadline: Instant,
) -> Result<ResolvedIdentity, FailureKind> {
    let mut child = OwnedChild(
        base_command(options, true, None)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| FailureKind::Resolve)?,
    );
    let mut stdout = child.0.stdout.take().ok_or(FailureKind::Io)?;
    nonblocking(&stdout).map_err(|_| FailureKind::Io)?;
    let mut bytes = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Err(FailureKind::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(FailureKind::Timeout);
        }
        match stdout.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                if bytes.len() + count > MAX_CONFIG {
                    return Err(FailureKind::Resolve);
                }
                bytes.extend_from_slice(&buffer[..count]);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => thread::sleep(TICK),
            Err(_) => return Err(FailureKind::Resolve),
        }
    }
    // stdout EOF alone is insufficient: a malicious/failed ssh -G could keep running indefinitely.
    let exit_code = loop {
        if let Some(code) = child.exit_code().map_err(|_| FailureKind::Resolve)? {
            break code;
        }
        if cancelled.load(Ordering::Acquire) {
            return Err(FailureKind::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(FailureKind::Timeout);
        }
        thread::sleep(TICK);
    };
    if exit_code != 0 {
        return Err(FailureKind::Resolve);
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| FailureKind::Resolve)?;
    let value = |key: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(key).map(str::trim))
    };
    let host = value("hostname ")
        .filter(|s| !s.is_empty() && !s.chars().any(char::is_control))
        .ok_or(FailureKind::Resolve)?;
    let account = value("user ")
        .filter(|s| !s.is_empty())
        .ok_or(FailureKind::Resolve)?;
    let port = value("port ")
        .and_then(|s| s.parse::<u16>().ok())
        .filter(|n| *n > 0)
        .ok_or(FailureKind::Resolve)?;
    Ok(ResolvedIdentity {
        host: host.into(),
        account: account.into(),
        port,
    })
}
struct PrivateBroker {
    root: PathBuf,
    listener: UnixListener,
    token: String,
}
impl PrivateBroker {
    fn new() -> io::Result<Self> {
        let mut random = [0; 24];
        File::open("/dev/urandom")?.read_exact(&mut random)?;
        let token: String = random.iter().map(|b| format!("{b:02x}")).collect();
        // Short path fits macOS sockaddr_un, independent of the user's potentially long TMPDIR.
        let root = PathBuf::from("/tmp").join(format!("deppy-ssh-{token}"));
        DirBuilder::new().mode(0o700).create(&root)?;
        let result = (|| {
            let listener = UnixListener::bind(root.join("askpass"))?;
            fs::set_permissions(root.join("askpass"), fs::Permissions::from_mode(0o600))?;
            listener.set_nonblocking(true)?;
            Ok(Self {
                root: root.clone(),
                listener,
                token,
            })
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&root);
        }
        result
    }
    fn diagnostics(&self) -> io::Result<(File, PathBuf)> {
        let path = self.root.join("diagnostics");
        let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        if unsafe { libc::mkfifo(name.as_ptr(), 0o600) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // A FIFO bounds diagnostics in kernel memory and never retains a raw log on disk.
        // Open the reader before SSH opens its -E writer so neither side blocks spawning.
        let reader = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)?;
        Ok((reader, path))
    }
}
impl Drop for PrivateBroker {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.root.join("askpass"));
        let _ = fs::remove_file(self.root.join("diagnostics"));
        let _ = fs::remove_dir(&self.root);
    }
}
struct PendingPrompt {
    stream: UnixStream,
    bytes: Vec<u8>,
    id: u64,
    kind: Option<PromptKind>,
    response: Option<Secret>,
    sent: usize,
    deadline: Instant,
}
fn prompts_tick(
    broker: &PrivateBroker,
    pending: &mut VecDeque<PendingPrompt>,
    next: &mut u64,
    events: &SyncSender<SshEvent>,
    replies: &Receiver<(u64, PromptReply)>,
    timeout: Duration,
) -> Result<(), FailureKind> {
    // Bound both unauthenticated clients and user-visible pending authentication requests.
    if pending.len() < 4 {
        match broker.listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(true).map_err(|_| FailureKind::Io)?;
                *next += 1;
                pending.push_back(PendingPrompt {
                    stream,
                    bytes: Vec::new(),
                    id: *next,
                    kind: None,
                    response: None,
                    sent: 0,
                    deadline: Instant::now() + timeout.min(Duration::from_secs(90)),
                });
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(_) => return Err(FailureKind::Io),
        }
    }
    for _ in 0..4 {
        let Ok((id, reply)) = replies.try_recv() else {
            break;
        };
        let Some(prompt) = pending
            .iter_mut()
            .find(|p| p.id == id && p.kind.is_some() && p.response.is_none())
        else {
            continue;
        };
        let secret = match (prompt.kind.as_ref(), reply) {
            (Some(PromptKind::HostTrust), PromptReply::ApproveHost)
            | (Some(PromptKind::AuthenticationConfirmation), PromptReply::ApproveAuthentication) => {
                Secret::new(b"yes".to_vec()).map_err(|_| FailureKind::Io)?
            }
            (_, PromptReply::Reject) => {
                return Err(FailureKind::Cancelled);
            }
            (Some(PromptKind::Authentication), PromptReply::Secret(secret)) => secret,
            // A string response can never approve a host; approval must be explicitly typed.
            _ => return Err(FailureKind::HostVerification),
        };
        let mut framed = Vec::with_capacity(secret.as_bytes().len() + 4);
        framed.extend_from_slice(&(secret.as_bytes().len() as u32).to_be_bytes());
        framed.extend_from_slice(secret.as_bytes());
        // Internal framed bytes may contain newline/NUL in the prefix, so keep them in separate secret storage below.
        prompt.bytes = framed;
        prompt.response = Some(secret);
    }
    let mut completed = Vec::new();
    for (index, prompt) in pending.iter_mut().enumerate() {
        if Instant::now() >= prompt.deadline {
            return Err(FailureKind::Timeout);
        }
        if prompt.response.is_some() {
            match prompt.stream.write(&prompt.bytes[prompt.sent..]) {
                Ok(0) => return Err(FailureKind::Authentication),
                Ok(n) => {
                    prompt.sent += n;
                    if prompt.sent == prompt.bytes.len() {
                        completed.push(index);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => return Err(FailureKind::Authentication),
            }
        } else if prompt.kind.is_none() {
            let mut buffer = [0; 1024];
            match prompt.stream.read(&mut buffer) {
                Ok(0) => return Err(FailureKind::Authentication),
                Ok(n) => {
                    if prompt.bytes.len() + n > MAX_REQUEST_BYTES + 4 {
                        return Err(FailureKind::Authentication);
                    }
                    prompt.bytes.extend_from_slice(&buffer[..n]);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(_) => return Err(FailureKind::Authentication),
            }
            if prompt.bytes.len() >= 4 {
                let len = u32::from_be_bytes(
                    prompt.bytes[..4]
                        .try_into()
                        .map_err(|_| FailureKind::Authentication)?,
                ) as usize;
                if len > MAX_REQUEST_BYTES {
                    return Err(FailureKind::Authentication);
                }
                if prompt.bytes.len() == len + 4 {
                    let (token, text, confirmation) =
                        ssh_askpass::decode_request(&prompt.bytes[4..])
                            .map_err(|_| FailureKind::Authentication)?;
                    if token != broker.token {
                        return Err(FailureKind::Authentication);
                    }
                    let kind = if confirmation
                        && text.contains("key fingerprint is")
                        && text.contains("SHA256:")
                        && text.contains("(yes/no")
                    {
                        PromptKind::HostTrust
                    } else if confirmation {
                        PromptKind::AuthenticationConfirmation
                    } else {
                        PromptKind::Authentication
                    };
                    let text: String = text
                        .chars()
                        .map(|c| {
                            if c.is_control() && !matches!(c, '\n' | '\t') {
                                '�'
                            } else {
                                c
                            }
                        })
                        .collect();
                    prompt.kind = Some(kind.clone());
                    prompt.bytes.clear();
                    emit(
                        events,
                        SshEvent::Prompt(AuthPrompt {
                            id: prompt.id,
                            kind,
                            text,
                        }),
                    )?;
                } else if prompt.bytes.len() > len + 4 {
                    return Err(FailureKind::Authentication);
                }
            }
        }
    }
    for index in completed.into_iter().rev() {
        pending.remove(index);
    }
    Ok(())
}
impl Drop for PendingPrompt {
    fn drop(&mut self) {
        for b in &mut self.bytes {
            unsafe {
                std::ptr::write_volatile(b, 0);
            }
        }
    }
}
#[derive(Default)]
struct Diagnostic {
    line: Vec<u8>,
    candidate_key: Option<String>,
    sent_key: bool,
    saw_authenticated: bool,
    failure: Option<FailureKind>,
}
impl Diagnostic {
    fn observe(&mut self, bytes: &[u8], events: &SyncSender<SshEvent>) -> Result<(), FailureKind> {
        for byte in bytes {
            if *byte == b'\n' {
                let line = String::from_utf8_lossy(&self.line);
                if !self.sent_key
                    && line.starts_with("Authenticated to ")
                    && line.contains(" using ")
                {
                    self.saw_authenticated = true;
                }
                if !self.sent_key && line.contains("REMOTE HOST IDENTIFICATION HAS CHANGED") {
                    self.failure = Some(FailureKind::HostKeyMismatch);
                } else if !self.sent_key
                    && self.failure.is_none()
                    && line.contains("Host key verification failed")
                {
                    self.failure = Some(FailureKind::HostVerification);
                } else if !self.sent_key
                    && self.failure.is_none()
                    && line.contains("Permission denied")
                {
                    self.failure = Some(FailureKind::Authentication);
                }
                let host_key = line
                    .strip_prefix("debug1: Server host key: ")
                    .map(|fields| (fields, false))
                    .or_else(|| {
                        line.strip_prefix("debug1: Server host certificate: ")
                            .map(|fields| (fields, true))
                    });
                if !self.sent_key
                    && let Some((fields, certificate)) = host_key
                {
                    // The first certificate fingerprint is the plain host key. Later fields
                    // include the certificate ID and CA fingerprint, neither of which identifies it.
                    self.candidate_key = fields
                        .split_whitespace()
                        .nth(1)
                        .and_then(|field| {
                            if certificate {
                                field.strip_suffix(',')
                            } else {
                                Some(field)
                            }
                        })
                        .filter(|s| {
                            s.starts_with("SHA256:")
                                && s.len() == 50
                                && s[7..]
                                    .bytes()
                                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/'))
                        })
                        .map(str::to_owned);
                }
                if !self.sent_key
                    && line.starts_with("debug1: Sending command:")
                    && let Some(key) = self.candidate_key.take()
                {
                    emit(events, SshEvent::HostKeyObserved(key))?;
                    self.sent_key = true;
                }
                self.line.fill(0);
                self.line.clear();
            } else if self.line.len() < 8192 {
                self.line.push(*byte);
            }
        }
        Ok(())
    }
}
impl Drop for Diagnostic {
    fn drop(&mut self) {
        self.line.fill(0);
    }
}
fn run(
    options: &SshLaunchOptions,
    state: (&AtomicBool, &AtomicBool),
    events: &SyncSender<SshEvent>,
    replies: Receiver<(u64, PromptReply)>,
    outgoing: Receiver<Outbound>,
    incoming: SyncSender<Vec<u8>>,
) -> FailureKind {
    match run_inner(options, state, events, replies, outgoing, incoming) {
        Ok(()) => FailureKind::Exit,
        Err(failure) => failure,
    }
}
fn run_inner(
    options: &SshLaunchOptions,
    state: (&AtomicBool, &AtomicBool),
    events: &SyncSender<SshEvent>,
    replies: Receiver<(u64, PromptReply)>,
    outgoing: Receiver<Outbound>,
    incoming: SyncSender<Vec<u8>>,
) -> Result<(), FailureKind> {
    let (cancelled, authenticated) = state;
    let deadline = Instant::now() + options.connect_timeout;
    emit(
        events,
        SshEvent::Resolved(resolve(options, cancelled, deadline)?),
    )?;
    let broker = PrivateBroker::new().map_err(|_| FailureKind::Io)?;
    let (mut diagnostics, diagnostic_path) = broker.diagnostics().map_err(|_| FailureKind::Io)?;
    let mut command = base_command(options, false, Some(&diagnostic_path));
    command
        .env("SSH_ASKPASS", &options.askpass_program)
        .env("SSH_ASKPASS_REQUIRE", "force")
        .env("DISPLAY", "deppy:0")
        .env(SOCKET_ENV, broker.root.join("askpass"))
        .env(TOKEN_ENV, &broker.token);
    let mut child = OwnedChild(
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|_| FailureKind::Io)?,
    );
    let mut stdin = Some(child.0.stdin.take().ok_or(FailureKind::Io)?);
    let mut stdout = child.0.stdout.take().ok_or(FailureKind::Io)?;
    let mut stderr = child.0.stderr.take().ok_or(FailureKind::Io)?;
    nonblocking(stdin.as_ref().ok_or(FailureKind::Io)?).map_err(|_| FailureKind::Io)?;
    nonblocking(&stdout).map_err(|_| FailureKind::Io)?;
    nonblocking(&stderr).map_err(|_| FailureKind::Io)?;
    let mut pending_prompts = VecDeque::new();
    let mut next_prompt = 0;
    let mut diagnostic = Diagnostic::default();
    let mut read_buffer = [0; CHUNK_BYTES];
    let mut write_pending: Option<(Vec<u8>, usize)> = None;
    let mut read_pending = None;
    let mut read_stalled: Option<Instant> = None;
    let mut write_stalled: Option<Instant> = None;
    let mut ready = false;
    let mut stdout_eof = false;
    let mut stdout_closed_at: Option<Instant> = None;
    let mut stdin_broken_at: Option<Instant> = None;
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Err(FailureKind::Cancelled);
        }
        if !ready && Instant::now() >= deadline {
            return Err(FailureKind::Timeout);
        }
        prompts_tick(
            &broker,
            &mut pending_prompts,
            &mut next_prompt,
            events,
            &replies,
            options.connect_timeout,
        )?;
        // Server banners and remote stderr are untrusted and may imitate SSH debug
        // lines. Drain them independently; only OpenSSH's private -E stream proves a peer.
        for _ in 0..8 {
            match stderr.read(&mut read_buffer) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => return Err(FailureKind::Io),
            }
        }
        for _ in 0..8 {
            match diagnostics.read(&mut read_buffer) {
                Ok(0) => break,
                Ok(n) => diagnostic.observe(&read_buffer[..n], events)?,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => return Err(FailureKind::Io),
            }
        }
        if let Some(failure) = &diagnostic.failure {
            return Err(failure.clone());
        }
        if diagnostic.sent_key && diagnostic.saw_authenticated {
            authenticated.store(true, Ordering::Release);
            ready = true;
        }
        if let Some(bytes) = read_pending.take() {
            match incoming.try_send(bytes) {
                Ok(()) => {
                    read_stalled = None;
                }
                Err(TrySendError::Full(bytes)) => {
                    read_pending = Some(bytes);
                    let stalled = read_stalled.get_or_insert_with(Instant::now);
                    if stalled.elapsed() >= options.io_timeout {
                        return Err(FailureKind::Backpressure);
                    }
                }
                Err(_) => return Err(FailureKind::Cancelled),
            }
        }
        if read_pending.is_none() && !stdout_eof {
            match stdout.read(&mut read_buffer) {
                Ok(0) => {
                    stdout_eof = true;
                    stdout_closed_at = Some(Instant::now());
                }
                Ok(n) => {
                    ready = true;
                    read_pending = Some(read_buffer[..n].to_vec());
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => return Err(FailureKind::Io),
            }
        }
        if write_pending.is_none() && stdin.is_some() {
            match outgoing.try_recv() {
                Ok(Outbound::Data(bytes)) => {
                    write_pending = Some((bytes, 0));
                    write_stalled = Some(Instant::now());
                }
                Ok(Outbound::Flush(done)) => {
                    let _ = done.try_send(());
                }
                Err(TryRecvError::Disconnected) => {
                    stdin.take();
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        if write_stalled.is_some_and(|start| start.elapsed() >= options.io_timeout) {
            return Err(FailureKind::Timeout);
        }
        if let (Some((bytes, offset)), Some(pipe)) = (&mut write_pending, &mut stdin) {
            match pipe.write(&bytes[*offset..]) {
                Ok(0) => return Err(FailureKind::Io),
                Ok(n) => {
                    *offset += n;
                    write_stalled = Some(Instant::now());
                    if *offset == bytes.len() {
                        write_pending = None;
                        write_stalled = None;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {
                    // A short helper can close stdin before OpenSSH publishes its exit
                    // status. Keep draining its output/status boundedly instead of
                    // replacing a meaningful remote exit (such as missing-helper127).
                    stdin.take();
                    write_pending = None;
                    write_stalled = None;
                    stdin_broken_at = Some(Instant::now());
                }
                Err(_) => return Err(FailureKind::Io),
            }
        }
        if stdout_eof
            && read_pending.is_none()
            && let Some(code) = child.exit_code().map_err(|_| FailureKind::Io)?
        {
            return if code == 0 {
                Ok(())
            } else {
                Err(FailureKind::RemoteExit(code))
            };
        }
        if stdout_eof
            && stdout_closed_at.is_some_and(|at| at.elapsed() >= Duration::from_millis(250))
        {
            return Err(FailureKind::Timeout);
        }
        if stdin_broken_at.is_some_and(|at| at.elapsed() >= Duration::from_millis(250)) {
            return Err(FailureKind::Io);
        }
        thread::sleep(TICK);
    }
}
