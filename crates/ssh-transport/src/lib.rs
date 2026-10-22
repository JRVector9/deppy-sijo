//! OpenSSH runs on an owned background worker. UI callers only use bounded, nonblocking controls.
//! The stream adapters may block and belong on a protocol worker, never on the UI thread.
#![cfg(unix)]
mod destination;
mod worker;
pub use destination::SshDestination;
pub use ssh_askpass::Secret;
use std::{
    io::{self, Read, Write},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, SyncSender, TrySendError},
    },
    thread,
    time::{Duration, Instant},
};

pub(crate) const CHUNK_BYTES: usize = 16 * 1024;
pub(crate) const QUEUE_CAP: usize = 32;
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedIdentity {
    pub host: String,
    pub account: String,
    pub port: u16,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelperVersion(String);
impl HelperVersion {
    pub fn parse(value: &str) -> io::Result<Self> {
        let parts: Vec<_> = value.split('.').collect();
        if parts.len() != 3
            || parts
                .iter()
                .any(|s| s.is_empty() || s.len() > 8 || !s.bytes().all(|b| b.is_ascii_digit()))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid helper version",
            ));
        }
        Ok(Self(value.to_owned()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelperDigest(String);
impl HelperDigest {
    pub fn parse(value: &str) -> io::Result<Self> {
        if value.len() != 64
            || !value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid helper SHA256",
            ));
        }
        Ok(Self(value.to_owned()))
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelperTarget {
    LinuxX86_64,
    LinuxArm64,
}
#[derive(Debug, Clone)]
pub enum RemoteCommand {
    Probe,
    Helper {
        version: HelperVersion,
    },
    InstallHelper {
        version: HelperVersion,
        digest: HelperDigest,
        target: HelperTarget,
    },
}
impl RemoteCommand {
    fn script(&self) -> String {
        match self {
            Self::Probe => "uname -s; uname -m; id -un; printf '%s\\n' \"$HOME\"".to_owned(),
            Self::Helper { version } => format!(
                "exec \"$HOME/.local/share/deppy/remote-host/{}/deppy-remote-host\" --stdio",
                version.as_str()
            ),
            Self::InstallHelper {
                version,
                digest,
                target,
            } => install_script(version, digest, *target),
        }
    }
}
#[derive(Debug, Clone)]
pub struct SshLaunchOptions {
    pub destination: SshDestination,
    /// Defaults to /usr/bin/ssh. Injectable for isolated executable fixtures.
    pub ssh_program: PathBuf,
    pub askpass_program: PathBuf,
    pub identity_file: Option<PathBuf>,
    pub port: Option<u16>,
    pub command: RemoteCommand,
    pub connect_timeout: Duration,
    /// Deadline only for a stalled stdin write or a full stdout consumer queue; idle sessions stay alive.
    pub io_timeout: Duration,
}
impl SshLaunchOptions {
    pub fn new(
        destination: SshDestination,
        askpass_program: PathBuf,
        command: RemoteCommand,
    ) -> Self {
        Self {
            destination,
            ssh_program: "/usr/bin/ssh".into(),
            askpass_program,
            identity_file: None,
            port: None,
            command,
            connect_timeout: Duration::from_secs(60),
            io_timeout: Duration::from_secs(30),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptKind {
    HostTrust,
    Authentication,
    AuthenticationConfirmation,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthPrompt {
    pub id: u64,
    pub kind: PromptKind,
    pub text: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailureKind {
    InvalidConfiguration,
    Resolve,
    HostKeyMismatch,
    HostVerification,
    Authentication,
    Timeout,
    Cancelled,
    Backpressure,
    Io,
    /// A clean expected EOF (for example a successful bootstrap probe).
    Exit,
    RemoteExit(i32),
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SshEvent {
    Resolved(ResolvedIdentity),
    /// OpenSSH's observed key. Persist only after the remote protocol handshake validates the peer.
    HostKeyObserved(String),
    Prompt(AuthPrompt),
    Ended(FailureKind),
}
pub enum PromptReply {
    ApproveHost,
    ApproveAuthentication,
    Reject,
    Secret(Secret),
}
pub struct SshWorker {
    cancelled: Arc<AtomicBool>,
    authenticated: Arc<AtomicBool>,
    events: Receiver<SshEvent>,
    replies: SyncSender<(u64, PromptReply)>,
}
impl SshWorker {
    /// Whether this worker reached authenticated remote command startup.
    /// Historical readiness; callers must also consume terminal events before starting a protocol.
    pub fn authenticated(&self) -> bool {
        self.authenticated.load(Ordering::Acquire)
    }
    /// Only validates/spawns a Rust thread here; process/filesystem/DNS/SSH I/O happens on that thread.
    pub fn spawn(options: SshLaunchOptions) -> io::Result<(Self, SshStream)> {
        worker::spawn(options)
    }
    pub fn try_event(&self) -> Option<SshEvent> {
        self.events.try_recv().ok()
    }
    pub fn reply(&self, id: u64, reply: PromptReply) -> io::Result<()> {
        self.replies.try_send((id, reply)).map_err(|_| {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                "authentication queue full or closed",
            )
        })
    }
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
    pub fn cancel_handle(&self) -> Arc<dyn Fn() + Send + Sync> {
        let cancelled = self.cancelled.clone();
        Arc::new(move || {
            cancelled.store(true, Ordering::Release);
        })
    }
}
impl Drop for SshWorker {
    fn drop(&mut self) {
        self.cancel();
    }
}
pub struct SshStream {
    reader: SshReader,
    writer: SshWriter,
}
impl SshStream {
    pub fn split(self) -> (SshReader, SshWriter) {
        (self.reader, self.writer)
    }
}
pub struct SshReader {
    incoming: Receiver<Vec<u8>>,
    buffered: Vec<u8>,
    offset: usize,
    timeout: Option<Duration>,
}
impl SshReader {
    /// Optional bootstrap-only read deadline. Leave None for long-lived runtime sessions.
    pub fn set_read_timeout(&mut self, timeout: Option<Duration>) {
        self.timeout = timeout;
    }
}
pub(crate) enum Outbound {
    Data(Vec<u8>),
    Flush(SyncSender<()>),
}
pub struct SshWriter {
    outgoing: SyncSender<Outbound>,
    cancelled: Arc<AtomicBool>,
    timeout: Duration,
}
impl Read for SshReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.offset == self.buffered.len() {
            let chunk = match self.timeout {
                Some(timeout) => self.incoming.recv_timeout(timeout),
                None => self
                    .incoming
                    .recv()
                    .map_err(|_| std::sync::mpsc::RecvTimeoutError::Disconnected),
            };
            match chunk {
                Ok(bytes) => {
                    self.buffered = bytes;
                    self.offset = 0;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Ok(0),
                Err(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "SSH stream read deadline",
                    ));
                }
            }
        }
        let count = buf.len().min(self.buffered.len() - self.offset);
        buf[..count].copy_from_slice(&self.buffered[self.offset..self.offset + count]);
        self.offset += count;
        Ok(count)
    }
}
impl SshWriter {
    fn enqueue(&self, mut message: Outbound) -> io::Result<()> {
        let deadline = Instant::now() + self.timeout;
        loop {
            if self.cancelled.load(Ordering::Acquire) {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "SSH transport cancelled",
                ));
            }
            match self.outgoing.try_send(message) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Disconnected(_)) => {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "SSH stream closed",
                    ));
                }
                Err(TrySendError::Full(value)) => message = value,
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "SSH stream write deadline",
                ));
            }
            thread::sleep(Duration::from_millis(2));
        }
    }
}
impl Write for SshWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let len = bytes.len().min(CHUNK_BYTES);
        self.enqueue(Outbound::Data(bytes[..len].to_vec()))?;
        Ok(len)
    }
    fn flush(&mut self) -> io::Result<()> {
        let (done, receipt) = std::sync::mpsc::sync_channel(1);
        self.enqueue(Outbound::Flush(done))?;
        receipt
            .recv_timeout(self.timeout)
            .map_err(|error| match error {
                std::sync::mpsc::RecvTimeoutError::Timeout => {
                    io::Error::new(io::ErrorKind::TimedOut, "SSH stream flush deadline")
                }
                std::sync::mpsc::RecvTimeoutError::Disconnected => {
                    io::Error::new(io::ErrorKind::BrokenPipe, "SSH stream closed before flush")
                }
            })
    }
}

fn install_script(version: &HelperVersion, digest: &HelperDigest, target: HelperTarget) -> String {
    let arch = match target {
        HelperTarget::LinuxX86_64 => "x86_64",
        HelperTarget::LinuxArm64 => "aarch64|arm64",
    };
    // Every interpolated value is either a fixed enum or privately constructed ASCII numeric/hex type.
    format!(
        r#"set -eu
umask 077
[ "$(uname -s)" = Linux ]
case "$(uname -m)" in {arch}) ;; *) exit 65 ;; esac
base="$HOME/.local/share/deppy/remote-host"
mkdir -p "$base"
lock="$base/.install-{version}.lock"
mkdir "$lock"
stage=""
trap 'test -z "$stage" || rm -rf "$stage"; rmdir "$lock"' EXIT HUP INT TERM
stage=$(mktemp -d "$base/.install-XXXXXX")
cat > "$stage/deppy-remote-host"
printf '%s  %s\n' '{digest}' "$stage/deppy-remote-host" | sha256sum -c - >/dev/null
chmod 700 "$stage/deppy-remote-host"
[ "$("$stage/deppy-remote-host" --version)" = 'deppy-remote-host {version}' ]
dest="$base/{version}"
if [ -e "$dest" ] || [ -L "$dest" ]; then
    [ -d "$dest" ] && [ ! -L "$dest" ]
    printf '%s  %s\n' '{digest}' "$dest/deppy-remote-host" | sha256sum -c - >/dev/null
else
    mv -T "$stage" "$dest"
    stage=""
fi
printf 'DEPPY_HELPER_READY {version}\n'
"#,
        version = version.as_str(),
        digest = digest.0
    )
}
