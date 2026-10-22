//! Bounded runtime/RPC framing over an already authenticated SSH stdio stream.
//! The owner supplies an interrupt that closes BOTH halves. It must unblock I/O;
//! SSH child cancellation and UnixStream::shutdown are suitable implementations.
use super::{OutboundEventQueue, RemoteSubscriber, dispatch, validate_command, validate_event};
pub use crate::protocol::ssh::*;
use crate::protocol::ssh::{
    SshAgentGuard, SshGuardedInput, SshRequestBody, SshRequestFrame, SshResponseFrame,
};
use crate::{
    InProcessRuntimeClient, RuntimeCommand, RuntimeCommandSendError, RuntimeCommandSink,
    RuntimeEventReceiver, RuntimeEventStream,
};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

pub type SshRpcHandler =
    Arc<dyn Fn(&SshRequestContext, &[u8]) -> anyhow::Result<Vec<u8>> + Send + Sync>;
const COMMAND_QUEUE_CAP: usize = 256;
const COMMAND_QUEUE_BYTES: usize = 32 * 1024 * 1024;
const RPC_PENDING_CAP: usize = 32;
const RPC_WORK_QUEUE_CAP: usize = 8;

// A remote supervisor has no Mac persistence DB. Its wire-facing session UUIDs
// must still remain stable across bridge attachments and distinct from every
// other verified target/runtime that can reuse the same numeric session IDs.
fn session_uuid_scope(identity: &SshTargetIdentity, runtime: &str) -> anyhow::Result<Sha256> {
    let identity = postcard::to_stdvec(identity)?;
    let mut scope = Sha256::new();
    scope.update(b"deppy.ssh.session-uuid.v1\0");
    for field in [identity.as_slice(), runtime.as_bytes()] {
        scope.update((field.len() as u64).to_be_bytes());
        scope.update(field);
    }
    Ok(scope)
}

fn project_session_uuids(event: &mut crate::RuntimeEvent, scope: &Sha256) {
    let crate::RuntimeEvent::MuxUpdated { snapshot } = event else {
        return;
    };
    if !snapshot.tabs.iter().any(|tab| {
        tab.panes
            .iter()
            .any(|pane| pane.session_id.is_some() && pane.persistent_session_id.is_none())
    }) {
        return;
    }
    for pane in Arc::make_mut(snapshot)
        .tabs
        .iter_mut()
        .flat_map(|tab| &mut tab.panes)
    {
        if let Some(session) = pane.session_id
            && pane.persistent_session_id.is_none()
        {
            let mut hash = scope.clone();
            hash.update(session.0.to_be_bytes());
            let mut bytes = [0; 16];
            bytes.copy_from_slice(&hash.finalize()[..16]);
            // RFC 9562 UUIDv8 permits an application-defined SHA256 layout.
            bytes[6] = (bytes[6] & 0x0f) | 0x80;
            bytes[8] = (bytes[8] & 0x3f) | 0x80;
            pane.persistent_session_id = Some(uuid::Uuid::from_bytes(bytes).to_string());
        }
    }
}

struct RpcJob {
    context: SshRequestContext,
    payload: Vec<u8>,
    done: Arc<AtomicBool>,
    replies: SyncSender<SshResponseFrame>,
    interrupt: Arc<dyn Fn() + Send + Sync>,
}

/// One serialized service lane per supervisor. Terminal commands and heartbeat
/// never wait for file/Git work, and reconnect shares the same write ordering.
pub struct SshRpcExecutor {
    jobs: SyncSender<RpcJob>,
}

impl SshRpcExecutor {
    pub fn new(handler: SshRpcHandler) -> anyhow::Result<Self> {
        let (jobs, receiver) = sync_channel::<RpcJob>(RPC_WORK_QUEUE_CAP);
        std::thread::Builder::new()
            .name("ssh-rpc-worker".into())
            .spawn(move || {
                while let Ok(job) = receiver.recv() {
                    // This check is the service admission point. EOF cancels every
                    // operation that has not started; admitted operations may finish.
                    if job.done.load(Ordering::Acquire) {
                        continue;
                    }
                    let result =
                        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            handler(&job.context, &job.payload)
                        })) {
                            Ok(Ok(payload)) if payload.len() <= SSH_RPC_BYTES_MAX => Ok(payload),
                            Ok(Ok(_)) => Err("ssh_rpc_reply_too_large".into()),
                            Ok(Err(_)) => Err("ssh_rpc_handler_failed".into()),
                            Err(_) => Err("ssh_rpc_handler_panicked".into()),
                        };
                    if job.done.load(Ordering::Acquire) {
                        continue;
                    }
                    if job
                        .replies
                        .try_send(SshResponseFrame::Rpc(SshRpcReply {
                            context: job.context,
                            result,
                        }))
                        .is_err()
                    {
                        job.done.store(true, Ordering::Release);
                        (job.interrupt)();
                    }
                }
            })?;
        Ok(Self { jobs })
    }

    fn submit(&self, job: RpcJob) -> Result<(), &'static str> {
        self.jobs.try_send(job).map_err(|error| match error {
            TrySendError::Full(_) => "ssh_rpc_backpressure",
            TrySendError::Disconnected(_) => "ssh_rpc_unavailable",
        })
    }
}

/// Read a bounded length-prefixed frame. EOF before a new header is clean;
/// truncation, oversized frames, and deserialization failures are errors.
pub fn read_ssh_frame(reader: &mut impl Read, limit: usize) -> anyhow::Result<Option<Vec<u8>>> {
    let mut length = [0; 4];
    loop {
        match reader.read(&mut length[..1]) {
            Ok(0) => return Ok(None),
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        }
    }
    reader.read_exact(&mut length[1..])?;
    let length = u32::from_le_bytes(length) as usize;
    anyhow::ensure!(
        length <= limit.min(SSH_FRAME_BYTES_MAX),
        "ssh_frame_too_large"
    );
    let mut payload = vec![0; length];
    reader.read_exact(&mut payload)?;
    Ok(Some(payload))
}

pub fn write_ssh_frame(
    writer: &mut impl Write,
    payload: &[u8],
    limit: usize,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        payload.len() <= limit.min(SSH_FRAME_BYTES_MAX),
        "ssh_frame_too_large"
    );
    super::write_frame(writer, payload)?;
    writer.flush()?;
    Ok(())
}

/// A bounded code-only rejection, used before the helper starts bridging frames.
pub fn write_ssh_handshake_rejection(
    writer: &mut impl Write,
    error: SshHandshakeError,
) -> anyhow::Result<()> {
    let reply: Result<SshServerHello, SshHandshakeError> = Err(error);
    write_ssh_frame(writer, &postcard::to_stdvec(&reply)?, 64 * 1024)
}

fn unix_millis() -> anyhow::Result<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis()
        .try_into()?)
}

fn context_matches(context: &SshRequestContext, hello: &SshServerHello) -> bool {
    context.identity == hello.identity
        && context.runtime_instance == hello.runtime_instance
        && context.connection_generation == hello.connection_generation
}

/// Runs one attachment only. EOF detaches; the caller retains ownership of the
/// runtime and its processes. No command queue, input, or event is replayed.
pub fn serve_ssh_stream<R: Read + Send + 'static, W: Write + Send + 'static>(
    backend: &InProcessRuntimeClient,
    reader: R,
    writer: W,
    config: &SshServerConfig,
    handler: SshRpcHandler,
    interrupt: Arc<dyn Fn() + Send + Sync>,
) -> anyhow::Result<()> {
    let executor = Arc::new(SshRpcExecutor::new(handler)?);
    serve_ssh_stream_with_rpc_executor(backend, reader, writer, config, executor, interrupt)
}

/// Reuse one executor across attachments to preserve service write ordering.
pub fn serve_ssh_stream_with_rpc_executor<R: Read + Send + 'static, W: Write + Send + 'static>(
    backend: &InProcessRuntimeClient,
    mut reader: R,
    mut writer: W,
    config: &SshServerConfig,
    executor: Arc<SshRpcExecutor>,
    interrupt: Arc<dyn Fn() + Send + Sync>,
) -> anyhow::Result<()> {
    let result = (|| {
        let payload = read_ssh_frame(&mut reader, 64 * 1024)?
            .ok_or_else(|| anyhow::anyhow!("ssh_hello_missing"))?;
        let request: SshClientHello = postcard::from_bytes(&payload)?;
        let hello = match config.negotiate(&request) {
            Ok(hello) => hello,
            Err(error) => {
                let rejection = error
                    .downcast_ref::<SshHandshakeError>()
                    .copied()
                    .unwrap_or(SshHandshakeError::InvalidRequest);
                write_ssh_handshake_rejection(&mut writer, rejection)?;
                return Err(error);
            }
        };
        write_ssh_frame(
            &mut writer,
            &postcard::to_stdvec(&Ok::<_, SshHandshakeError>(&hello))?,
            64 * 1024,
        )?;
        let limit = hello.max_frame_bytes as usize;
        let receiver = backend.subscribe();
        let (reply_tx, reply_rx) = sync_channel::<SshResponseFrame>(RPC_PENDING_CAP);
        let done = Arc::new(AtomicBool::new(false));
        let pump_done = Arc::clone(&done);
        let pump_interrupt = Arc::clone(&interrupt);
        let event_context = SshRequestContext {
            identity: hello.identity.clone(),
            runtime_instance: hello.runtime_instance.clone(),
            connection_generation: hello.connection_generation,
            request_id: 0,
        };
        let session_scope = session_uuid_scope(&hello.identity, &hello.runtime_instance)?;
        let pump = std::thread::Builder::new()
            .name("ssh-event-pump".into())
            .spawn(move || {
                let result = (|| -> anyhow::Result<()> {
                    let mut outbound = OutboundEventQueue::new();
                    while !pump_done.load(Ordering::Acquire) {
                        // Reuse the existing durable FIFO and coalesced viewport bounds.
                        for event in receiver.drain() {
                            outbound
                                .enqueue(event)
                                .map_err(|_| anyhow::anyhow!("ssh_event_backpressure"))?;
                        }
                        anyhow::ensure!(!receiver.take_overflowed(), "ssh_runtime_event_overflow");
                        for _ in 0..64 {
                            let reply = match reply_rx.try_recv() {
                                Ok(reply) => reply,
                                Err(_) => break,
                            };
                            let payload = postcard::to_stdvec(&reply)?;
                            write_ssh_frame(&mut writer, &payload, limit)?;
                        }
                        for _ in 0..64 {
                            let Some(mut event) = outbound.pop_front() else {
                                break;
                            };
                            project_session_uuids(&mut event, &session_scope);
                            validate_event(&event).map_err(anyhow::Error::msg)?;
                            let payload = postcard::to_stdvec(&SshResponseFrame::Event {
                                context: event_context.clone(),
                                event: Box::new(event),
                            })?;
                            write_ssh_frame(&mut writer, &payload, limit)?;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Ok(())
                })();
                pump_done.store(true, Ordering::Release);
                pump_interrupt();
                result
            })?;
        let connection_permit = crate::InputPermit::new();
        let read_result = (|| -> anyhow::Result<()> {
            backend.send_command(RuntimeCommand::RequestMuxSnapshot)?;
            let mut sequence = 0u64;
            while !done.load(Ordering::Acquire) {
                let Some(payload) = read_ssh_frame(&mut reader, limit)? else {
                    break;
                };
                let frame: SshRequestFrame = postcard::from_bytes(&payload)?;
                anyhow::ensure!(
                    context_matches(&frame.context, &hello) && frame.context.request_id > 0,
                    "ssh_request_target_stale"
                );
                sequence = sequence
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("ssh_sequence_exhausted"))?;
                anyhow::ensure!(frame.sequence == sequence, "ssh_request_replay_or_gap");
                match frame.body {
                    SshRequestBody::Command(command) => {
                        validate_command(&command).map_err(anyhow::Error::msg)?;
                        crate::command::validate_host_command(&command)?;
                        // A remote peer may use only credentials installed on this account.
                        // The helper resolver never has access to the Mac keychain.
                        backend.send_command(*command)?;
                    }
                    SshRequestBody::Rpc(payload) => {
                        anyhow::ensure!(payload.len() <= SSH_RPC_BYTES_MAX, "ssh_rpc_too_large");
                        let context = frame.context;
                        if let Err(code) = executor.submit(RpcJob {
                            context: context.clone(),
                            payload,
                            done: Arc::clone(&done),
                            replies: reply_tx.clone(),
                            interrupt: Arc::clone(&interrupt),
                        }) {
                            reply_tx
                                .try_send(SshResponseFrame::Rpc(SshRpcReply {
                                    context,
                                    result: Err(code.into()),
                                }))
                                .map_err(|_| anyhow::anyhow!("ssh_rpc_reply_backpressure"))?;
                        }
                    }
                    SshRequestBody::GuardedInput(input) => {
                        let now = unix_millis()?;
                        let remaining = input
                            .expires_at_unix_ms
                            .checked_sub(now)
                            .filter(|remaining| (1..=30_000).contains(remaining))
                            .ok_or_else(|| {
                                anyhow::anyhow!("ssh_input_expired_or_clock_mismatch")
                            })?;
                        let command = RuntimeCommand::WriteInputBatchTracked {
                            session: input.session,
                            operation_id: input.operation_id.clone(),
                            parts: input.parts.clone(),
                        };
                        crate::command::validate_host_command(&command)?;
                        let mut admission = crate::InputAdmission::new(
                            connection_permit.clone(),
                            std::time::Instant::now() + Duration::from_millis(remaining),
                            |write| write(),
                        );
                        if input.require_bracketed_paste {
                            admission = admission.with_bracketed_paste_required();
                        }
                        if let Some(guard) = input.guard {
                            anyhow::ensure!(
                                guard.foreground_process_group > 0
                                    && guard.foreground_process_group <= i32::MAX as u32,
                                "ssh_input_guard_invalid"
                            );
                            let provider = match guard.provider {
                                0 => crate::AgentPromptKind::Claude,
                                1 => crate::AgentPromptKind::Codex,
                                2 => crate::AgentPromptKind::Other,
                                _ => anyhow::bail!("ssh_input_guard_invalid"),
                            };
                            admission = admission.with_agent_guard(crate::AgentInputGuard {
                                foreground_process_group: guard.foreground_process_group,
                                provider,
                                intent: if guard.append {
                                    crate::AgentInputIntent::ExplicitAppend
                                } else {
                                    crate::AgentInputIntent::ExplicitPrompt
                                },
                            });
                        }
                        backend.send_guarded_input_batch(
                            input.session,
                            input.operation_id,
                            input.parts,
                            admission,
                        )?;
                    }
                    SshRequestBody::Ping => {
                        reply_tx
                            .try_send(SshResponseFrame::Pong {
                                context: frame.context,
                            })
                            .map_err(|_| anyhow::anyhow!("ssh_pong_backpressure"))?;
                    }
                    SshRequestBody::Detach => break,
                }
            }
            Ok(())
        })();
        connection_permit.revoke();
        done.store(true, Ordering::Release);
        interrupt();
        let pump_result = pump
            .join()
            .map_err(|_| anyhow::anyhow!("ssh_event_pump_panicked"))?;
        // A deliberate EOF can race the output writer's socket shutdown. Only
        // those close errors are expected; validation, overflow and deadline
        // failures must remain observable to the supervisor.
        if let Err(error) = pump_result {
            let closed = error.downcast_ref::<std::io::Error>().is_some_and(|error| {
                matches!(
                    error.kind(),
                    std::io::ErrorKind::BrokenPipe
                        | std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::NotConnected
                )
            });
            if !closed {
                return Err(error);
            }
        }
        read_result
    })();
    interrupt();
    result
}

type WakeEntry = (std::sync::Weak<AtomicBool>, crate::RuntimeWake);

struct SshClientState {
    commands: SyncSender<Vec<u8>>,
    queued_bytes: AtomicUsize,
    sequence: Mutex<u64>,
    next_request: AtomicU64,
    hello: SshServerHello,
    connected: AtomicBool,
    subscribers: Arc<Mutex<Vec<RemoteSubscriber>>>,
    wakes: Mutex<Vec<WakeEntry>>,
    pending_rpc: Mutex<HashSet<u64>>,
    interrupt: Arc<dyn Fn() + Send + Sync>,
    liveness: SyncSender<()>,
}

impl SshClientState {
    fn wake(&self) {
        let wakes: Vec<_> = {
            let mut wakes = self.wakes.lock().expect("ssh wake lock");
            wakes.retain(|(owner, _)| owner.strong_count() > 1);
            wakes.iter().map(|(_, wake)| Arc::clone(wake)).collect()
        };
        for wake in wakes {
            wake();
        }
    }

    fn disconnect(&self) {
        if self.connected.swap(false, Ordering::AcqRel) {
            let _ = self.liveness.try_send(());
            self.wake();
            self.subscribers
                .lock()
                .expect("ssh subscriber lock")
                .clear();
            self.pending_rpc.lock().expect("ssh rpc lock").clear();
            (self.interrupt)();
        }
    }

    fn enqueue(&self, request_id: u64, body: SshRequestBody) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.connected.load(Ordering::Acquire),
            RuntimeCommandSendError::Disconnected
        );
        let mut sequence = self.sequence.lock().expect("ssh sequence lock");
        let next = sequence
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("ssh_sequence_exhausted"))?;
        let frame = SshRequestFrame {
            sequence: next,
            context: SshRequestContext {
                identity: self.hello.identity.clone(),
                runtime_instance: self.hello.runtime_instance.clone(),
                connection_generation: self.hello.connection_generation,
                request_id,
            },
            body,
        };
        let payload = postcard::to_stdvec(&frame)?;
        anyhow::ensure!(
            payload.len() <= self.hello.max_frame_bytes as usize,
            "ssh_frame_too_large"
        );
        let length = payload.len();
        self.queued_bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |bytes| {
                bytes
                    .checked_add(length)
                    .filter(|next| *next <= COMMAND_QUEUE_BYTES)
            })
            .map_err(|_| anyhow::Error::new(RuntimeCommandSendError::Backpressure))?;
        match self.commands.try_send(payload) {
            Ok(()) => {
                *sequence = next;
                Ok(())
            }
            Err(error) => {
                self.queued_bytes.fetch_sub(length, Ordering::AcqRel);
                match error {
                    TrySendError::Full(_) => Err(RuntimeCommandSendError::Backpressure.into()),
                    TrySendError::Disconnected(_) => {
                        Err(RuntimeCommandSendError::Disconnected.into())
                    }
                }
            }
        }
    }

    fn send_owned(
        &self,
        command: RuntimeCommand,
    ) -> Result<(), (anyhow::Error, Box<RuntimeCommand>)> {
        if let Err(error) = crate::command::validate_host_command(&command) {
            return Err((error.into(), Box::new(command)));
        }
        let command = Box::new(command);
        let request_id = self.next_request.fetch_add(1, Ordering::Relaxed);
        // Serialization borrows the retained command so an unadmitted operation is
        // returned exactly once. Only explicit callers can elect to retry it.
        let cloned = match postcard::to_stdvec(&*command)
            .and_then(|bytes| postcard::from_bytes::<RuntimeCommand>(&bytes))
        {
            Ok(command) => command,
            Err(error) => return Err((error.into(), command)),
        };
        self.enqueue(request_id, SshRequestBody::Command(Box::new(cloned)))
            .map_err(|error| (error, command))
    }
}

/// One SSH attachment. Reconnect constructs a NEW client with `resume()`; this
/// instance never reopens a transport or retains input for a subsequent client.
pub struct SshRuntimeClient {
    state: Arc<SshClientState>,
    rpc: Mutex<Receiver<SshRpcReply>>,
    reader_thread: Option<JoinHandle<()>>,
    writer_thread: Option<JoinHandle<()>>,
    liveness_thread: Option<JoinHandle<()>>,
}

impl SshRuntimeClient {
    /// Blocking hello exchange; call on a background connection worker. The
    /// supplied interrupt must unblock reader and writer, including handshake.
    pub fn connect<R: Read + Send + 'static, W: Write + Send + 'static>(
        reader: R,
        writer: W,
        request: SshClientHello,
        interrupt: Arc<dyn Fn() + Send + Sync>,
    ) -> anyhow::Result<Self> {
        Self::connect_timed(
            reader,
            writer,
            request,
            interrupt,
            Duration::from_secs(15),
            Duration::from_secs(45),
        )
    }

    fn connect_timed<R: Read + Send + 'static, W: Write + Send + 'static>(
        mut reader: R,
        mut writer: W,
        request: SshClientHello,
        interrupt: Arc<dyn Fn() + Send + Sync>,
        heartbeat: Duration,
        read_deadline: Duration,
    ) -> anyhow::Result<Self> {
        request.identity.validate()?;
        let (deadline_tx, deadline_rx) = sync_channel::<()>(1);
        let deadline_interrupt = Arc::clone(&interrupt);
        let watchdog = std::thread::Builder::new()
            .name("ssh-hello-deadline".into())
            .spawn(move || {
                if deadline_rx.recv_timeout(Duration::from_secs(5)).is_err() {
                    deadline_interrupt();
                }
            })?;
        let handshake = (|| -> anyhow::Result<SshServerHello> {
            write_ssh_frame(&mut writer, &postcard::to_stdvec(&request)?, 64 * 1024)?;
            let bytes = read_ssh_frame(&mut reader, 64 * 1024)?
                .ok_or_else(|| anyhow::anyhow!("ssh_hello_missing"))?;
            let hello = postcard::from_bytes::<Result<SshServerHello, SshHandshakeError>>(&bytes)??;
            anyhow::ensure!(
                hello.envelope_version == request.envelope_version
                    && hello.runtime_protocol == request.runtime_protocol,
                SshHandshakeError::ProtocolMismatch
            );
            anyhow::ensure!(
                hello.identity == request.identity
                    && hello.connection_generation == request.connection_generation
                    && hello.connection_generation > 0,
                SshHandshakeError::TargetMismatch
            );
            anyhow::ensure!(
                hello.capabilities & SSH_CAPABILITIES == SSH_CAPABILITIES
                    && (1024..=request.max_frame_bytes).contains(&hello.max_frame_bytes)
                    && hello.max_frame_bytes as usize <= SSH_FRAME_BYTES_MAX,
                SshHandshakeError::CapabilityMismatch
            );
            anyhow::ensure!(
                !hello.runtime_instance.is_empty()
                    && hello.runtime_instance.len() <= 128
                    && !hello.lease.is_empty()
                    && hello.lease.len() <= 128
                    && (1..=1800).contains(&hello.lease_ttl_secs),
                SshHandshakeError::InvalidRequest
            );
            if let Some(resume) = &request.resume {
                anyhow::ensure!(
                    resume.runtime_instance == hello.runtime_instance
                        && resume.lease == hello.lease,
                    SshHandshakeError::LeaseMismatch
                );
            }
            Ok(hello)
        })();
        let _ = deadline_tx.send(());
        let _ = watchdog.join();
        // Completed I/O drops the stream normally. The transport owner must
        // still observe natural exits such as a missing helper (127); cancelling
        // here can overwrite that result. The watchdog interrupts blocked I/O.
        let hello = handshake?;
        let (commands, command_rx) = sync_channel::<Vec<u8>>(COMMAND_QUEUE_CAP);
        let (rpc_tx, rpc_rx) = sync_channel(RPC_PENDING_CAP);
        let (liveness, liveness_rx) = sync_channel(1);
        let state = Arc::new(SshClientState {
            commands,
            queued_bytes: AtomicUsize::new(0),
            sequence: Mutex::new(0),
            next_request: AtomicU64::new(1),
            hello,
            connected: AtomicBool::new(true),
            subscribers: Arc::default(),
            wakes: Mutex::new(Vec::new()),
            pending_rpc: Mutex::new(HashSet::new()),
            interrupt,
            liveness,
        });
        let writer_state = Arc::clone(&state);
        let writer_thread = std::thread::Builder::new()
            .name("ssh-runtime-write".into())
            .spawn(move || {
                let mut last_ping = std::time::Instant::now();
                while writer_state.connected.load(Ordering::Acquire) {
                    if last_ping.elapsed() >= heartbeat {
                        let request_id = writer_state.next_request.fetch_add(1, Ordering::Relaxed);
                        if writer_state
                            .enqueue(request_id, SshRequestBody::Ping)
                            .is_ok()
                        {
                            last_ping = std::time::Instant::now();
                        }
                    }
                    match command_rx.recv_timeout(Duration::from_millis(10)) {
                        Ok(payload) => {
                            writer_state
                                .queued_bytes
                                .fetch_sub(payload.len(), Ordering::AcqRel);
                            if !writer_state.connected.load(Ordering::Acquire) {
                                break;
                            }
                            if write_ssh_frame(
                                &mut writer,
                                &payload,
                                writer_state.hello.max_frame_bytes as usize,
                            )
                            .is_err()
                            {
                                break;
                            }
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(_) => break,
                    }
                }
                writer_state.disconnect();
            })?;
        let reader_state = Arc::clone(&state);
        let reader_result = std::thread::Builder::new()
            .name("ssh-runtime-read".into())
            .spawn(move || {
                let _ = (|| -> anyhow::Result<()> {
                    while reader_state.connected.load(Ordering::Acquire) {
                        let Some(bytes) = read_ssh_frame(
                            &mut reader,
                            reader_state.hello.max_frame_bytes as usize,
                        )?
                        else {
                            break;
                        };
                        if bytes.is_empty() {
                            continue;
                        }
                        let mut wake = true;
                        match postcard::from_bytes::<SshResponseFrame>(&bytes)? {
                            SshResponseFrame::Pong { context } => {
                                anyhow::ensure!(
                                    context_matches(&context, &reader_state.hello)
                                        && context.request_id > 0,
                                    "ssh_pong_target_stale"
                                );
                                wake = false;
                            }
                            SshResponseFrame::Event { context, event } => {
                                anyhow::ensure!(
                                    context_matches(&context, &reader_state.hello)
                                        && context.request_id == 0,
                                    "ssh_response_target_stale"
                                );
                                validate_event(&event).map_err(anyhow::Error::msg)?;
                                dispatch(&reader_state.subscribers, *event);
                            }
                            SshResponseFrame::Rpc(reply) => {
                                anyhow::ensure!(
                                    context_matches(&reply.context, &reader_state.hello),
                                    "ssh_response_target_stale"
                                );
                                anyhow::ensure!(
                                    reply.result.as_ref().map_or_else(
                                        |error| error.len() <= 1024,
                                        |bytes| bytes.len() <= SSH_RPC_BYTES_MAX
                                    ),
                                    "ssh_rpc_reply_too_large"
                                );
                                anyhow::ensure!(
                                    reader_state
                                        .pending_rpc
                                        .lock()
                                        .expect("ssh rpc lock")
                                        .contains(&reply.context.request_id),
                                    "ssh_rpc_unrequested"
                                );
                                rpc_tx
                                    .try_send(reply)
                                    .map_err(|_| anyhow::anyhow!("ssh_rpc_backpressure"))?;
                            }
                        }
                        let _ = reader_state.liveness.try_send(());
                        if wake {
                            reader_state.wake();
                        }
                    }
                    Ok(())
                })();
                reader_state.disconnect();
            });
        let reader_thread = match reader_result {
            Ok(thread) => thread,
            Err(error) => {
                state.disconnect();
                let _ = writer_thread.join();
                return Err(error.into());
            }
        };
        let liveness_state = Arc::clone(&state);
        let liveness_result = std::thread::Builder::new()
            .name("ssh-runtime-liveness".into())
            .spawn(move || {
                while liveness_state.connected.load(Ordering::Acquire) {
                    if liveness_rx.recv_timeout(read_deadline).is_err() {
                        liveness_state.disconnect();
                        break;
                    }
                }
            });
        match liveness_result {
            Ok(thread) => Ok(Self {
                state,
                rpc: Mutex::new(rpc_rx),
                reader_thread: Some(reader_thread),
                writer_thread: Some(writer_thread),
                liveness_thread: Some(thread),
            }),
            Err(error) => {
                state.disconnect();
                let _ = reader_thread.join();
                let _ = writer_thread.join();
                Err(error.into())
            }
        }
    }

    pub fn send_guarded_input_batch(
        &self,
        session: crate::SessionId,
        operation_id: String,
        parts: Vec<Vec<u8>>,
        admission: crate::InputAdmission,
    ) -> anyhow::Result<()> {
        let guard = admission
            .agent_guard()
            .map(|guard| -> anyhow::Result<SshAgentGuard> {
                anyhow::ensure!(
                    matches!(
                        guard.intent,
                        crate::AgentInputIntent::ExplicitPrompt
                            | crate::AgentInputIntent::ExplicitAppend
                    ),
                    "ssh_automatic_or_tracked_input_unsupported"
                );
                Ok(SshAgentGuard {
                    foreground_process_group: guard.foreground_process_group,
                    provider: match guard.provider {
                        crate::AgentPromptKind::Claude => 0,
                        crate::AgentPromptKind::Codex => 1,
                        crate::AgentPromptKind::Other => 2,
                    },
                    append: guard.intent == crate::AgentInputIntent::ExplicitAppend,
                })
            })
            .transpose()?;
        let command = RuntimeCommand::WriteInputBatchTracked {
            session,
            operation_id: operation_id.clone(),
            parts: parts.clone(),
        };
        crate::command::validate_host_command(&command)?;
        let remaining = admission.remaining().as_millis().min(30_000) as u64;
        anyhow::ensure!(remaining > 0, "ssh_input_admission_expired");
        let input = SshGuardedInput {
            session,
            operation_id,
            parts,
            guard,
            require_bracketed_paste: admission.requires_bracketed_paste(),
            expires_at_unix_ms: unix_millis()?.saturating_add(remaining),
        };
        let request_id = self.state.next_request.fetch_add(1, Ordering::Relaxed);
        admission
            .admit(|| {
                self.state
                    .enqueue(request_id, SshRequestBody::GuardedInput(input))
            })
            .ok_or_else(|| anyhow::anyhow!("ssh_input_admission_denied"))?
    }
    pub fn send_guarded_input(
        &self,
        session: crate::SessionId,
        operation_id: String,
        bytes: Vec<u8>,
        admission: crate::InputAdmission,
    ) -> anyhow::Result<()> {
        self.send_guarded_input_batch(session, operation_id, vec![bytes], admission)
    }
    pub fn resume(&self) -> SshResume {
        SshResume {
            runtime_instance: self.state.hello.runtime_instance.clone(),
            lease: self.state.hello.lease.clone(),
        }
    }
    pub fn identity(&self) -> &SshTargetIdentity {
        &self.state.hello.identity
    }
    pub fn connection_generation(&self) -> u64 {
        self.state.hello.connection_generation
    }
    pub fn instance_token(&self) -> &str {
        &self.state.hello.runtime_instance
    }
    pub fn is_connected(&self) -> bool {
        self.state.connected.load(Ordering::Acquire)
    }
    pub fn send_command_owned(
        &self,
        command: RuntimeCommand,
    ) -> Result<(), (anyhow::Error, Box<RuntimeCommand>)> {
        self.state.send_owned(command)
    }
    pub fn command_sink(&self) -> Option<Arc<dyn Fn(RuntimeCommand) + Send + Sync>> {
        if !self.is_connected() {
            return None;
        }
        let state = Arc::clone(&self.state);
        Some(Arc::new(move |command| {
            let _ = state.send_owned(command);
        }))
    }
    pub fn send_rpc(&self, request_id: u64, payload: Vec<u8>) -> anyhow::Result<()> {
        anyhow::ensure!(
            request_id > 0 && payload.len() <= SSH_RPC_BYTES_MAX,
            "ssh_rpc_invalid"
        );
        {
            let mut pending = self.state.pending_rpc.lock().expect("ssh rpc lock");
            anyhow::ensure!(
                pending.len() < RPC_PENDING_CAP && pending.insert(request_id),
                "ssh_rpc_backpressure_or_duplicate"
            );
        }
        let result = self.state.enqueue(request_id, SshRequestBody::Rpc(payload));
        if result.is_err() {
            self.state
                .pending_rpc
                .lock()
                .expect("ssh rpc lock")
                .remove(&request_id);
        }
        result
    }
    pub fn try_recv_rpc(&self) -> Option<SshRpcReply> {
        let reply = self
            .rpc
            .lock()
            .expect("ssh rpc receiver lock")
            .try_recv()
            .ok()?;
        self.state
            .pending_rpc
            .lock()
            .expect("ssh rpc lock")
            .remove(&reply.context.request_id);
        Some(reply)
    }
    fn subscribe_inner(&self, wake: Option<crate::RuntimeWake>) -> RuntimeEventReceiver {
        let (tx, events) = sync_channel(crate::client::LOCAL_EVENT_QUEUE_CAP);
        let overflowed = Arc::new(AtomicBool::new(false));
        let viewports = Arc::default();
        let history_results = Arc::default();
        let input_pressures = Arc::default();
        {
            let mut subscribers = self.state.subscribers.lock().expect("ssh subscriber lock");
            // Every subscriber has bounded durable/latest-value queues; subscriber
            // count is bounded too, including abandoned receivers awaiting dispatch.
            if self.is_connected() && subscribers.len() < 64 {
                subscribers.push(RemoteSubscriber {
                    events: tx,
                    overflowed: Arc::clone(&overflowed),
                    viewports: Arc::clone(&viewports),
                    history_results: Arc::clone(&history_results),
                    input_pressures: Arc::clone(&input_pressures),
                });
                if let Some(wake) = wake {
                    self.state
                        .wakes
                        .lock()
                        .expect("ssh wake lock")
                        .push((Arc::downgrade(&overflowed), wake));
                }
            }
        }
        let receiver = RuntimeEventReceiver {
            events,
            pending_durable: Mutex::new(None),
            overflowed,
            viewports,
            history_results,
            input_pressures,
            resource_usage: Arc::default(),
        };
        let _ = self.send_command(RuntimeCommand::RequestMuxSnapshot);
        receiver
    }
    pub fn subscribe_with_wake(&self, wake: crate::RuntimeWake) -> RuntimeEventReceiver {
        self.subscribe_inner(Some(wake))
    }
    pub fn subscribe_with_wake_background(&self, wake: crate::RuntimeWake) -> RuntimeEventReceiver {
        self.subscribe_inner(Some(wake))
    }
    /// Nonblocking detach for owners holding this client through an Arc.
    pub fn disconnect(&self) {
        self.state.disconnect();
    }
    /// Detach only. The remote supervisor retains sessions for its advertised lease.
    pub fn shutdown(&mut self) {
        self.state.disconnect();
        if let Some(thread) = self.reader_thread.take() {
            let _ = thread.join();
        }
        if let Some(thread) = self.writer_thread.take() {
            let _ = thread.join();
        }
        if let Some(thread) = self.liveness_thread.take() {
            let _ = thread.join();
        }
    }
}
impl RuntimeCommandSink for SshRuntimeClient {
    fn send_command(&self, command: RuntimeCommand) -> anyhow::Result<()> {
        self.send_command_owned(command).map_err(|(error, _)| error)
    }
}
impl RuntimeEventStream for SshRuntimeClient {
    fn subscribe(&self) -> RuntimeEventReceiver {
        self.subscribe_inner(None)
    }
}
impl crate::RuntimeClient for SshRuntimeClient {}
impl Drop for SshRuntimeClient {
    fn drop(&mut self) {
        // UI owners may release the last Arc. Interrupt I/O now and let the
        // detached workers release their own state as they exit. Explicit
        // background shutdown remains available when a caller needs to join.
        self.state.disconnect();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    fn backend() -> InProcessRuntimeClient {
        let logs = std::env::temp_dir().join(format!("deppy-ssh-stream-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&logs).unwrap();
        InProcessRuntimeClient::with_shell(
            5,
            crate::test_secret_store::test_store(),
            logs,
            secret::RedactionService::new(),
            pty::CommandSpec {
                program: "/bin/sh".into(),
                args: vec![],
                env: vec![],
                cwd: None,
            },
            None,
        )
    }

    fn config(backend: &InProcessRuntimeClient) -> SshServerConfig {
        SshServerConfig::new(
            SshTargetIdentity {
                remote_workspace_id: "fixture".into(),
                account: "fixture".into(),
                canonical_root: "/tmp".into(),
                host_key_sha256: "SHA256:fixture".into(),
            },
            backend.instance_token().into(),
        )
    }

    fn mux_identity_fixture() -> crate::RuntimeEvent {
        let pane = crate::MuxPaneId::new();
        let tab = crate::MuxTabId::new();
        crate::RuntimeEvent::MuxUpdated {
            snapshot: Arc::new(crate::MuxSnapshot {
                active_tab: Some(tab.clone()),
                focused_pane: Some(pane.clone()),
                tabs: vec![crate::TabSnapshot {
                    id: tab,
                    title: "fixture".into(),
                    layout: crate::LayoutNode::Pane(pane.clone()),
                    panes: vec![crate::PaneSnapshot {
                        id: pane,
                        session_id: Some(crate::SessionId(1)),
                        title: "fixture".into(),
                        persistent_session_id: None,
                    }],
                }],
            }),
        }
    }

    #[test]
    fn ssh_stream_completed_handshake_eof_preserves_owned_transport_exit() {
        let backend = backend();
        let hello = SshClientHello::new(config(&backend).identity, 1);
        let interrupts = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&interrupts);
        let result = SshRuntimeClient::connect(
            std::io::Cursor::new(Vec::<u8>::new()),
            Vec::<u8>::new(),
            hello,
            Arc::new(move || {
                observed.fetch_add(1, Ordering::SeqCst);
            }),
        );
        assert!(result.is_err(), "EOF cannot establish a runtime client");
        assert_eq!(
            interrupts.load(Ordering::SeqCst),
            0,
            "a completed handshake failure must let the SSH owner observe the natural exit code"
        );
    }

    #[test]
    fn ssh_stream_partial_handshake_deadline_still_interrupts_blocked_transport() {
        struct PartialFrame {
            prefix_sent: bool,
            interrupted: Receiver<()>,
        }
        impl Read for PartialFrame {
            fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                if !self.prefix_sent {
                    self.prefix_sent = true;
                    bytes[0] = 1;
                    return Ok(1);
                }
                self.interrupted
                    .recv_timeout(Duration::from_secs(7))
                    .expect("hello deadline must unblock the partial frame");
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "fixture interrupted",
                ))
            }
        }
        let backend = backend();
        let hello = SshClientHello::new(config(&backend).identity, 1);
        let (interrupt, interrupted) = sync_channel(1);
        let interrupts = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&interrupts);
        let result = SshRuntimeClient::connect(
            PartialFrame {
                prefix_sent: false,
                interrupted,
            },
            Vec::<u8>::new(),
            hello,
            Arc::new(move || {
                observed.fetch_add(1, Ordering::SeqCst);
                let _ = interrupt.try_send(());
            }),
        );
        assert!(result.is_err());
        assert!(interrupts.load(Ordering::SeqCst) >= 1);
    }

    fn projected_uuid(identity: &SshTargetIdentity, runtime: &str, session: u64) -> String {
        let mut event = mux_identity_fixture();
        let crate::RuntimeEvent::MuxUpdated { snapshot } = &mut event else {
            unreachable!()
        };
        Arc::make_mut(snapshot).tabs[0].panes[0].session_id = Some(crate::SessionId(session));
        project_session_uuids(&mut event, &session_uuid_scope(identity, runtime).unwrap());
        let crate::RuntimeEvent::MuxUpdated { snapshot } = event else {
            unreachable!()
        };
        snapshot.tabs[0].panes[0]
            .persistent_session_id
            .clone()
            .unwrap()
    }

    #[test]
    fn ssh_stream_session_uuid_is_stable_and_scoped_to_full_target_runtime_and_session() {
        let identity = SshTargetIdentity {
            remote_workspace_id: "workspace".into(),
            account: "account".into(),
            canonical_root: "/project".into(),
            host_key_sha256: "SHA256:key".into(),
        };
        let id = projected_uuid(&identity, "runtime-a", 1);
        let parsed = uuid::Uuid::parse_str(&id).unwrap();
        assert_eq!(parsed.get_version_num(), 8);
        assert_eq!(id, projected_uuid(&identity, "runtime-a", 1));
        assert_ne!(id, projected_uuid(&identity, "runtime-a", 2));
        assert_ne!(id, projected_uuid(&identity, "runtime-b", 1));
        for field in 0..4 {
            let mut other = identity.clone();
            match field {
                0 => other.remote_workspace_id.push('b'),
                1 => other.account.push('b'),
                2 => other.canonical_root.push('b'),
                _ => other.host_key_sha256.push('b'),
            }
            assert_ne!(id, projected_uuid(&other, "runtime-a", 1));
        }
    }

    #[test]
    fn ssh_stream_uuid_projection_preserves_existing_ids_and_backend_snapshot() {
        let backend = backend();
        let identity = config(&backend).identity;
        let scope = session_uuid_scope(&identity, backend.instance_token()).unwrap();
        let original = mux_identity_fixture();
        let mut projected = original.clone();
        project_session_uuids(&mut projected, &scope);
        let crate::RuntimeEvent::MuxUpdated { snapshot } = original else {
            unreachable!()
        };
        assert!(snapshot.tabs[0].panes[0].persistent_session_id.is_none());
        let crate::RuntimeEvent::MuxUpdated { snapshot } = &mut projected else {
            unreachable!()
        };
        let preserved = uuid::Uuid::new_v4().to_string();
        Arc::make_mut(snapshot).tabs[0].panes[0].persistent_session_id = Some(preserved.clone());
        project_session_uuids(&mut projected, &scope);
        let crate::RuntimeEvent::MuxUpdated { snapshot } = projected else {
            unreachable!()
        };
        assert_eq!(
            snapshot.tabs[0].panes[0].persistent_session_id,
            Some(preserved)
        );
    }

    #[test]
    fn ssh_stream_server_sends_handshake_then_executes_structured_rpc_and_scoped_commands() {
        let backend = backend();
        let config = config(&backend);
        let hello = SshClientHello::new(config.identity.clone(), 1);
        let (mut peer, stream) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let close = stream.try_clone().unwrap();
        let worker = std::thread::spawn(move || {
            serve_ssh_stream(
                &backend,
                stream.try_clone().unwrap(),
                stream,
                &config,
                Arc::new(|_, payload| Ok(payload.to_vec())),
                Arc::new(move || {
                    let _ = close.shutdown(std::net::Shutdown::Both);
                }),
            )
        });
        let bytes = postcard::to_stdvec(&hello).unwrap();
        super::super::write_frame(&mut peer, &bytes).unwrap();
        let response = super::super::read_frame(&mut peer).expect("helper must answer hello");
        let answer = postcard::from_bytes::<Result<SshServerHello, SshHandshakeError>>(&response)
            .unwrap()
            .unwrap();
        assert_eq!(answer.connection_generation, 1);
        let context = SshRequestContext {
            identity: hello.identity.clone(),
            runtime_instance: answer.runtime_instance,
            connection_generation: 1,
            request_id: 42,
        };
        let request = crate::protocol::ssh::SshRequestFrame {
            sequence: 1,
            context: context.clone(),
            body: crate::protocol::ssh::SshRequestBody::Rpc(b"structured-body".to_vec()),
        };
        super::super::write_frame(&mut peer, &postcard::to_stdvec(&request).unwrap()).unwrap();
        loop {
            let bytes = super::super::read_frame(&mut peer).unwrap();
            let reply: crate::protocol::ssh::SshResponseFrame =
                postcard::from_bytes(&bytes).unwrap();
            if let crate::protocol::ssh::SshResponseFrame::Rpc(reply) = reply {
                assert_eq!(reply.context, context);
                assert_eq!(reply.result.unwrap(), b"structured-body");
                break;
            }
        }
        let _ = peer.shutdown(std::net::Shutdown::Both);
        worker.join().unwrap().unwrap();
    }

    #[test]
    fn ssh_stream_unknown_target_never_reaches_runtime() {
        let backend = backend();
        let config = config(&backend);
        let mut hello = SshClientHello::new(config.identity.clone(), 1);
        hello.identity.canonical_root = "/another".into();
        let (mut peer, stream) = UnixStream::pair().unwrap();
        let close = stream.try_clone().unwrap();
        let worker = std::thread::spawn(move || {
            serve_ssh_stream(
                &backend,
                stream.try_clone().unwrap(),
                stream,
                &config,
                Arc::new(|_, _| panic!("wrong target may not invoke services")),
                Arc::new(move || {
                    let _ = close.shutdown(std::net::Shutdown::Both);
                }),
            )
        });
        super::super::write_frame(&mut peer, &postcard::to_stdvec(&hello).unwrap()).unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let rejection = super::super::read_frame(&mut peer).unwrap();
        assert!(matches!(
            postcard::from_bytes::<Result<SshServerHello, SshHandshakeError>>(&rejection).unwrap(),
            Err(SshHandshakeError::TargetMismatch)
        ));
        assert!(worker.join().unwrap().is_err());
    }

    #[test]
    fn ssh_stream_duplicate_sequence_cannot_repeat_a_service_operation() {
        let backend = backend();
        let config = config(&backend);
        let request = SshClientHello::new(config.identity.clone(), 1);
        let (mut peer, stream) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let server_calls = Arc::clone(&calls);
        let close = stream.try_clone().unwrap();
        let server = std::thread::spawn(move || {
            serve_ssh_stream(
                &backend,
                stream.try_clone().unwrap(),
                stream,
                &config,
                Arc::new(move |_, bytes| {
                    server_calls.fetch_add(1, Ordering::Relaxed);
                    Ok(bytes.to_vec())
                }),
                Arc::new(move || {
                    let _ = close.shutdown(std::net::Shutdown::Both);
                }),
            )
        });
        write_ssh_frame(&mut peer, &postcard::to_stdvec(&request).unwrap(), 65536).unwrap();
        let hello: SshServerHello =
            postcard::from_bytes::<Result<SshServerHello, SshHandshakeError>>(
                &read_ssh_frame(&mut peer, 65536).unwrap().unwrap(),
            )
            .unwrap()
            .unwrap();
        let request = SshRequestFrame {
            sequence: 1,
            context: SshRequestContext {
                identity: hello.identity,
                runtime_instance: hello.runtime_instance,
                connection_generation: 1,
                request_id: 7,
            },
            body: SshRequestBody::Rpc(b"write-once".to_vec()),
        };
        let bytes = postcard::to_stdvec(&request).unwrap();
        write_ssh_frame(&mut peer, &bytes, SSH_FRAME_BYTES_MAX).unwrap();
        loop {
            let reply = read_ssh_frame(&mut peer, SSH_FRAME_BYTES_MAX)
                .unwrap()
                .unwrap();
            if matches!(
                postcard::from_bytes::<SshResponseFrame>(&reply).unwrap(),
                SshResponseFrame::Rpc(_)
            ) {
                break;
            }
        }
        write_ssh_frame(&mut peer, &bytes, SSH_FRAME_BYTES_MAX).unwrap();
        let error = server.join().unwrap().unwrap_err();
        assert_eq!(error.to_string(), "ssh_request_replay_or_gap");
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }
    fn attach(
        backend: Arc<InProcessRuntimeClient>,
        config: SshServerConfig,
        hello: SshClientHello,
    ) -> (
        SshRuntimeClient,
        std::thread::JoinHandle<anyhow::Result<()>>,
    ) {
        let (stream, peer) = UnixStream::pair().unwrap();
        let close = stream.try_clone().unwrap();
        let worker = std::thread::spawn(move || {
            serve_ssh_stream(
                &backend,
                stream.try_clone().unwrap(),
                stream,
                &config,
                Arc::new(|_, payload| Ok(payload.to_vec())),
                Arc::new(move || {
                    let _ = close.shutdown(std::net::Shutdown::Both);
                }),
            )
        });
        let close = peer.try_clone().unwrap();
        let client = SshRuntimeClient::connect(
            peer.try_clone().unwrap(),
            peer,
            hello,
            Arc::new(move || {
                let _ = close.shutdown(std::net::Shutdown::Both);
            }),
        )
        .unwrap();
        (client, worker)
    }

    fn wait_event(
        receiver: &RuntimeEventReceiver,
        mut predicate: impl FnMut(&crate::RuntimeEvent) -> bool,
    ) {
        let deadline = std::time::Instant::now() + Duration::from_secs(4);
        while std::time::Instant::now() < deadline {
            if receiver.drain().iter().any(&mut predicate) {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("ssh fixture event deadline");
    }

    #[test]
    fn ssh_stream_client_reconnect_has_fresh_snapshot_and_never_replays_input() {
        let backend = Arc::new(backend());
        let mut config = config(&backend);
        let (mut client, first) = attach(
            Arc::clone(&backend),
            config.clone(),
            SshClientHello::new(config.identity.clone(), 1),
        );
        let receiver = client.subscribe();
        client
            .send_command(crate::RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
            })
            .unwrap();
        let mut session = None;
        wait_event(&receiver, |event| {
            if let crate::RuntimeEvent::ShellSpawned { session: found } = event {
                session = Some(*found);
                true
            } else {
                false
            }
        });
        let session = session.unwrap();
        client
            .send_command(crate::RuntimeCommand::Resize {
                session,
                cols: 90,
                rows: 30,
            })
            .unwrap();
        let path = std::env::temp_dir().join(format!("deppy-no-replay-{}", uuid::Uuid::new_v4()));
        client
            .send_command(crate::RuntimeCommand::WriteInput {
                session,
                bytes: format!("printf X >> '{}'\n", path.display()).into_bytes(),
            })
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(4);
        while !path.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(std::fs::read(&path).unwrap(), b"X");
        let resume = client.resume();
        client.shutdown();
        first.join().unwrap().unwrap();
        config.require_resume = true;
        let mut hello = SshClientHello::new(config.identity.clone(), 2);
        hello.resume = Some(resume);
        let (mut client, second) = attach(Arc::clone(&backend), config, hello);
        let receiver = client.subscribe();
        wait_event(
            &receiver,
            |event| matches!(event, crate::RuntimeEvent::MuxUpdated { snapshot } if snapshot.tabs.iter().any(|tab| tab.panes.iter().any(|pane| pane.session_id == Some(session)))),
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"X",
            "reattach must not replay even acknowledged input"
        );
        client
            .send_command(crate::RuntimeCommand::KillSession { session })
            .unwrap();
        wait_event(
            &receiver,
            |event| matches!(event, crate::RuntimeEvent::MuxUpdated { snapshot } if snapshot.tabs.iter().all(|tab| tab.panes.iter().all(|pane| pane.session_id != Some(session)))),
        );
        client.shutdown();
        second.join().unwrap().unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn ssh_stream_explicit_admission_respects_revocation_and_remote_pty_queue() {
        let backend = Arc::new(backend());
        let config = config(&backend);
        let (mut client, worker) = attach(
            Arc::clone(&backend),
            config.clone(),
            SshClientHello::new(config.identity, 1),
        );
        let receiver = client.subscribe();
        client
            .send_command(crate::RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
            })
            .unwrap();
        let mut session = None;
        wait_event(&receiver, |event| {
            if let crate::RuntimeEvent::ShellSpawned { session: found } = event {
                session = Some(*found);
                true
            } else {
                false
            }
        });
        let session = session.unwrap();
        let path = std::env::temp_dir().join(format!("deppy-ssh-guard-{}", uuid::Uuid::new_v4()));
        let part = format!("printf X >> '{}'\n", path.display()).into_bytes();
        let permit = crate::InputPermit::new();
        permit.revoke();
        let admission = crate::InputAdmission::new(
            permit,
            std::time::Instant::now() + Duration::from_secs(3),
            |write| write(),
        );
        assert!(
            client
                .send_guarded_input_batch(session, "revoked".into(), vec![part.clone()], admission)
                .is_err()
        );
        let admission = crate::InputAdmission::new(
            crate::InputPermit::new(),
            std::time::Instant::now() + Duration::from_secs(3),
            |write| write(),
        );
        client
            .send_guarded_input_batch(session, "explicit".into(), vec![part], admission)
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !path.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"X",
            "revoked input never reached server PTY"
        );
        client.shutdown();
        worker.join().unwrap().unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn ssh_stream_ping_returns_exact_scoped_pong_without_runtime_event() {
        let backend = backend();
        let config = config(&backend);
        let hello = SshClientHello::new(config.identity.clone(), 1);
        let (mut peer, stream) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_millis(300)))
            .unwrap();
        let close = stream.try_clone().unwrap();
        let worker = std::thread::spawn(move || {
            serve_ssh_stream(
                &backend,
                stream.try_clone().unwrap(),
                stream,
                &config,
                Arc::new(|_, _| panic!("ping never invokes application RPC")),
                Arc::new(move || {
                    let _ = close.shutdown(std::net::Shutdown::Both);
                }),
            )
        });
        write_ssh_frame(&mut peer, &postcard::to_stdvec(&hello).unwrap(), 65536).unwrap();
        let reply: SshServerHello =
            postcard::from_bytes::<Result<SshServerHello, SshHandshakeError>>(
                &read_ssh_frame(&mut peer, 65536).unwrap().unwrap(),
            )
            .unwrap()
            .unwrap();
        let context = SshRequestContext {
            identity: hello.identity,
            runtime_instance: reply.runtime_instance,
            connection_generation: 1,
            request_id: 99,
        };
        let ping = SshRequestFrame {
            sequence: 1,
            context: context.clone(),
            body: SshRequestBody::Ping,
        };
        write_ssh_frame(
            &mut peer,
            &postcard::to_stdvec(&ping).unwrap(),
            SSH_FRAME_BYTES_MAX,
        )
        .unwrap();
        loop {
            let bytes = read_ssh_frame(&mut peer, SSH_FRAME_BYTES_MAX)
                .expect("healthy idle helper must pong before read deadline")
                .unwrap();
            if let SshResponseFrame::Pong { context: found } = postcard::from_bytes(&bytes).unwrap()
            {
                assert_eq!(found, context);
                break;
            }
        }
        let _ = peer.shutdown(std::net::Shutdown::Both);
        worker.join().unwrap().unwrap();
    }

    #[test]
    fn ssh_stream_silent_peer_deadline_interrupts_reader_and_writer() {
        let backend = backend();
        let config = config(&backend);
        let request = SshClientHello::new(config.identity.clone(), 1);
        let (mut peer, stream) = UnixStream::pair().unwrap();
        let peer_close = peer.try_clone().unwrap();
        let (stop, stopped) = sync_channel::<()>(1);
        let server = std::thread::spawn(move || {
            let bytes = read_ssh_frame(&mut peer, 65536).unwrap().unwrap();
            let request: SshClientHello = postcard::from_bytes(&bytes).unwrap();
            let hello = config.negotiate(&request).unwrap();
            write_ssh_frame(
                &mut peer,
                &postcard::to_stdvec(&Ok::<_, SshHandshakeError>(hello)).unwrap(),
                65536,
            )
            .unwrap();
            let _ = stopped.recv_timeout(Duration::from_secs(2));
        });
        let close = stream.try_clone().unwrap();
        let mut client = SshRuntimeClient::connect_timed(
            stream.try_clone().unwrap(),
            stream,
            request,
            Arc::new(move || {
                let _ = close.shutdown(std::net::Shutdown::Both);
            }),
            Duration::from_millis(20),
            Duration::from_millis(80),
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            !client.is_connected(),
            "silent peer cannot retain reader forever"
        );
        client.shutdown();
        let _ = stop.send(());
        let _ = peer_close.shutdown(std::net::Shutdown::Both);
        server.join().unwrap();
    }

    #[test]
    fn ssh_stream_healthy_idle_heartbeat_keeps_connection_without_ui_wakes() {
        let backend = backend();
        let config = config(&backend);
        let request = SshClientHello::new(config.identity.clone(), 1);
        let (mut peer, stream) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || {
            let bytes = read_ssh_frame(&mut peer, 65536).unwrap().unwrap();
            let request: SshClientHello = postcard::from_bytes(&bytes).unwrap();
            let hello = config.negotiate(&request).unwrap();
            write_ssh_frame(
                &mut peer,
                &postcard::to_stdvec(&Ok::<_, SshHandshakeError>(hello)).unwrap(),
                65536,
            )
            .unwrap();
            let mut pings = 0;
            while let Some(bytes) = read_ssh_frame(&mut peer, SSH_FRAME_BYTES_MAX).unwrap() {
                let request: SshRequestFrame = postcard::from_bytes(&bytes).unwrap();
                if matches!(request.body, SshRequestBody::Ping) {
                    pings += 1;
                    let pong = SshResponseFrame::Pong {
                        context: request.context,
                    };
                    write_ssh_frame(
                        &mut peer,
                        &postcard::to_stdvec(&pong).unwrap(),
                        SSH_FRAME_BYTES_MAX,
                    )
                    .unwrap();
                }
            }
            pings
        });
        let close = stream.try_clone().unwrap();
        let mut client = SshRuntimeClient::connect_timed(
            stream.try_clone().unwrap(),
            stream,
            request,
            Arc::new(move || {
                let _ = close.shutdown(std::net::Shutdown::Both);
            }),
            Duration::from_millis(10),
            Duration::from_millis(80),
        )
        .unwrap();
        let wakes = Arc::new(AtomicUsize::new(0));
        let wake_count = Arc::clone(&wakes);
        let receiver = client.subscribe_with_wake(Arc::new(move || {
            wake_count.fetch_add(1, Ordering::Relaxed);
        }));
        std::thread::sleep(Duration::from_millis(240));
        assert!(
            client.is_connected(),
            "healthy idle peer must survive multiple read deadlines"
        );
        assert!(receiver.drain().is_empty());
        assert_eq!(
            wakes.load(Ordering::Relaxed),
            0,
            "heartbeat is transport work only"
        );
        client.shutdown();
        assert!(server.join().unwrap() >= 3);
    }

    #[test]
    fn ssh_stream_drop_never_waits_for_delayed_transport_cleanup() {
        struct DelayedEof(UnixStream);
        impl Read for DelayedEof {
            fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                let count = self.0.read(bytes)?;
                if count == 0 {
                    std::thread::sleep(Duration::from_millis(300));
                }
                Ok(count)
            }
        }
        let backend = backend();
        let config = config(&backend);
        let hello = SshClientHello::new(config.identity.clone(), 1);
        let (stream, peer) = UnixStream::pair().unwrap();
        let close = stream.try_clone().unwrap();
        let server = std::thread::spawn(move || {
            serve_ssh_stream(
                &backend,
                stream.try_clone().unwrap(),
                stream,
                &config,
                Arc::new(|_, payload| Ok(payload.to_vec())),
                Arc::new(move || {
                    let _ = close.shutdown(std::net::Shutdown::Both);
                }),
            )
        });
        let close = peer.try_clone().unwrap();
        let client = SshRuntimeClient::connect(
            DelayedEof(peer.try_clone().unwrap()),
            peer,
            hello,
            Arc::new(move || {
                let _ = close.shutdown(std::net::Shutdown::Both);
            }),
        )
        .unwrap();
        // The reader must be inside the transport before it is interrupted.
        std::thread::sleep(Duration::from_millis(20));
        let started = std::time::Instant::now();
        drop(client);
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "UI owner cannot join transport cleanup"
        );
        server.join().unwrap().unwrap();
    }

    #[test]
    fn ssh_stream_client_retains_typed_resume_expiry_rejection() {
        let backend = backend();
        let config = config(&backend);
        let request = SshClientHello::new(config.identity, 1);
        let (mut peer, stream) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || {
            read_ssh_frame(&mut peer, 65536).unwrap().unwrap();
            let reply: Result<SshServerHello, SshHandshakeError> =
                Err(SshHandshakeError::ResumeExpired);
            write_ssh_frame(&mut peer, &postcard::to_stdvec(&reply).unwrap(), 65536).unwrap();
        });
        let close = stream.try_clone().unwrap();
        let error = SshRuntimeClient::connect(
            stream.try_clone().unwrap(),
            stream,
            request,
            Arc::new(move || {
                let _ = close.shutdown(std::net::Shutdown::Both);
            }),
        )
        .err()
        .expect("expired proof must reject hello");
        assert_eq!(
            error.downcast_ref::<SshHandshakeError>(),
            Some(&SshHandshakeError::ResumeExpired)
        );
        server.join().unwrap();
    }

    #[test]
    fn ssh_stream_slow_service_never_blocks_terminal_commands() {
        let backend = backend();
        let config = config(&backend);
        let hello = SshClientHello::new(config.identity.clone(), 1);
        let (started_tx, started_rx) = sync_channel(1);
        let (release_tx, release_rx) = sync_channel(1);
        let release_rx = Mutex::new(release_rx);
        let (stream, peer) = UnixStream::pair().unwrap();
        let close = stream.try_clone().unwrap();
        let server = std::thread::spawn(move || {
            serve_ssh_stream(
                &backend,
                stream.try_clone().unwrap(),
                stream,
                &config,
                Arc::new(move |_, payload| {
                    started_tx.try_send(()).unwrap();
                    let _ = release_rx
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(2));
                    Ok(payload.to_vec())
                }),
                Arc::new(move || {
                    let _ = close.shutdown(std::net::Shutdown::Both);
                }),
            )
        });
        let close = peer.try_clone().unwrap();
        let mut client = SshRuntimeClient::connect_timed(
            peer.try_clone().unwrap(),
            peer,
            hello,
            Arc::new(move || {
                let _ = close.shutdown(std::net::Shutdown::Both);
            }),
            Duration::from_millis(10),
            Duration::from_millis(80),
        )
        .unwrap();
        let receiver = client.subscribe();
        client.send_rpc(91, b"slow-file-query".to_vec()).unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        client
            .send_command(RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
            })
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_millis(200);
        let mut spawned = false;
        while std::time::Instant::now() < deadline {
            spawned |= receiver
                .drain()
                .iter()
                .any(|event| matches!(event, crate::RuntimeEvent::ShellSpawned { .. }));
            if spawned {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let connected = client.is_connected();
        release_tx.send(()).unwrap();
        client.shutdown();
        let _ = server.join().unwrap();
        assert!(spawned, "slow service cannot delay terminal commands");
        assert!(connected, "slow service cannot delay transport heartbeat");
    }

    fn attach_rpc_peer(
        backend: Arc<InProcessRuntimeClient>,
        config: SshServerConfig,
        request: SshClientHello,
        executor: Arc<SshRpcExecutor>,
    ) -> (
        UnixStream,
        SshServerHello,
        std::thread::JoinHandle<anyhow::Result<()>>,
    ) {
        let (mut peer, stream) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let close = stream.try_clone().unwrap();
        let server = std::thread::spawn(move || {
            serve_ssh_stream_with_rpc_executor(
                &backend,
                stream.try_clone().unwrap(),
                stream,
                &config,
                executor,
                Arc::new(move || {
                    let _ = close.shutdown(std::net::Shutdown::Both);
                }),
            )
        });
        write_ssh_frame(&mut peer, &postcard::to_stdvec(&request).unwrap(), 65536).unwrap();
        let hello = postcard::from_bytes::<Result<SshServerHello, SshHandshakeError>>(
            &read_ssh_frame(&mut peer, 65536).unwrap().unwrap(),
        )
        .unwrap()
        .unwrap();
        (peer, hello, server)
    }

    fn rpc_peer_send(
        peer: &mut UnixStream,
        hello: &SshServerHello,
        sequence: u64,
        body: SshRequestBody,
    ) -> SshRequestContext {
        let context = SshRequestContext {
            identity: hello.identity.clone(),
            runtime_instance: hello.runtime_instance.clone(),
            connection_generation: hello.connection_generation,
            request_id: sequence,
        };
        let frame = SshRequestFrame {
            sequence,
            context: context.clone(),
            body,
        };
        write_ssh_frame(
            peer,
            &postcard::to_stdvec(&frame).unwrap(),
            SSH_FRAME_BYTES_MAX,
        )
        .unwrap();
        context
    }

    fn rpc_peer_read(peer: &mut UnixStream) -> SshResponseFrame {
        postcard::from_bytes(
            &read_ssh_frame(peer, SSH_FRAME_BYTES_MAX)
                .expect("RPC fixture response deadline")
                .expect("RPC fixture connection must remain alive"),
        )
        .unwrap()
    }

    #[test]
    fn ssh_stream_full_rpc_lane_replies_scoped_error_and_keeps_terminal_and_ping_alive() {
        let backend = Arc::new(backend());
        let config = config(&backend);
        let request = SshClientHello::new(config.identity.clone(), 1);
        let (started_tx, started_rx) = sync_channel(1);
        let (release_tx, release_rx) = sync_channel(1);
        let release_rx = Mutex::new(release_rx);
        let executor = Arc::new(
            SshRpcExecutor::new(Arc::new(move |_, payload| {
                if payload == b"blocked" {
                    started_tx.send(()).unwrap();
                    let _ = release_rx
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(5));
                }
                Ok(payload.to_vec())
            }))
            .unwrap(),
        );
        let (mut peer, hello, server) = attach_rpc_peer(backend, config, request, executor);
        rpc_peer_send(
            &mut peer,
            &hello,
            1,
            SshRequestBody::Rpc(b"blocked".to_vec()),
        );
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        // One running operation plus the bounded eight waiting operations.
        for sequence in 2..=9 {
            rpc_peer_send(&mut peer, &hello, sequence, SshRequestBody::Rpc(vec![0]));
        }
        let rejected = rpc_peer_send(&mut peer, &hello, 10, SshRequestBody::Rpc(vec![1]));
        let ping = rpc_peer_send(&mut peer, &hello, 11, SshRequestBody::Ping);
        rpc_peer_send(
            &mut peer,
            &hello,
            12,
            SshRequestBody::Command(Box::new(RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
            })),
        );
        let (mut backpressure, mut pong, mut spawned) = (false, false, false);
        while !(backpressure && pong && spawned) {
            match rpc_peer_read(&mut peer) {
                SshResponseFrame::Rpc(reply) if reply.context == rejected => {
                    assert_eq!(reply.result.unwrap_err(), "ssh_rpc_backpressure");
                    backpressure = true;
                }
                SshResponseFrame::Pong { context } => {
                    assert_eq!(context, ping);
                    pong = true;
                }
                SshResponseFrame::Event { event, .. } => {
                    spawned |= matches!(*event, crate::RuntimeEvent::ShellSpawned { .. });
                }
                _ => {}
            }
        }
        let _ = peer.shutdown(std::net::Shutdown::Both);
        server.join().unwrap().unwrap();
        release_tx.send(()).unwrap();
    }

    #[test]
    fn ssh_stream_detach_cancels_waiting_rpc_and_old_reply_cannot_cross_reattach() {
        let backend = Arc::new(backend());
        let mut config = config(&backend);
        let request = SshClientHello::new(config.identity.clone(), 1);
        let (started_tx, started_rx) = sync_channel(1);
        let (release_tx, release_rx) = sync_channel(1);
        let release_rx = Mutex::new(release_rx);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let handler_calls = Arc::clone(&calls);
        let executor = Arc::new(
            SshRpcExecutor::new(Arc::new(move |context, payload| {
                handler_calls
                    .lock()
                    .unwrap()
                    .push((context.clone(), payload.to_vec()));
                if payload == b"old-active" {
                    started_tx.send(()).unwrap();
                    let _ = release_rx
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(5));
                }
                Ok(payload.to_vec())
            }))
            .unwrap(),
        );
        let (mut old, first, server) = attach_rpc_peer(
            Arc::clone(&backend),
            config.clone(),
            request,
            Arc::clone(&executor),
        );
        rpc_peer_send(
            &mut old,
            &first,
            1,
            SshRequestBody::Rpc(b"old-active".to_vec()),
        );
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        rpc_peer_send(
            &mut old,
            &first,
            2,
            SshRequestBody::Rpc(b"must-cancel".to_vec()),
        );
        rpc_peer_send(&mut old, &first, 3, SshRequestBody::Ping);
        // Pong proves the preceding service request reached the server queue.
        while !matches!(rpc_peer_read(&mut old), SshResponseFrame::Pong { .. }) {}
        let _ = old.shutdown(std::net::Shutdown::Both);
        server.join().unwrap().unwrap();
        config.require_resume = true;
        let mut request = SshClientHello::new(config.identity.clone(), 2);
        request.resume = Some(SshResume {
            runtime_instance: first.runtime_instance.clone(),
            lease: first.lease.clone(),
        });
        let (mut fresh, hello, server) = attach_rpc_peer(backend, config, request, executor);
        // Reuse id 1 deliberately: an old reply must not satisfy this new request.
        let expected = rpc_peer_send(
            &mut fresh,
            &hello,
            1,
            SshRequestBody::Rpc(b"fresh".to_vec()),
        );
        rpc_peer_send(&mut fresh, &hello, 2, SshRequestBody::Ping);
        while !matches!(rpc_peer_read(&mut fresh), SshResponseFrame::Pong { .. }) {}
        assert_eq!(
            calls.lock().unwrap().len(),
            1,
            "reattach shares the serialized lane"
        );
        release_tx.send(()).unwrap();
        loop {
            if let SshResponseFrame::Rpc(reply) = rpc_peer_read(&mut fresh) {
                assert_eq!(reply.context, expected);
                assert_eq!(reply.result.unwrap(), b"fresh");
                break;
            }
        }
        let _ = fresh.shutdown(std::net::Shutdown::Both);
        server.join().unwrap().unwrap();
        let calls = calls.lock().unwrap();
        assert_eq!(
            calls.len(),
            2,
            "unstarted operations from EOF are never executed"
        );
        assert_eq!(calls[0].1, b"old-active");
        assert_eq!(calls[1].1, b"fresh");
        assert_eq!(calls[1].0, expected);
    }

    #[test]
    fn ssh_stream_rpc_handler_error_never_discloses_request_or_error_payload() {
        let backend = Arc::new(backend());
        let config = config(&backend);
        let request = SshClientHello::new(config.identity.clone(), 1);
        let executor = Arc::new(
            SshRpcExecutor::new(Arc::new(|_, payload| {
                anyhow::bail!(
                    "private service error includes {}",
                    String::from_utf8_lossy(payload)
                );
            }))
            .unwrap(),
        );
        let (mut peer, hello, server) = attach_rpc_peer(backend, config, request, executor);
        let expected = rpc_peer_send(
            &mut peer,
            &hello,
            1,
            SshRequestBody::Rpc(b"secret-request".to_vec()),
        );
        loop {
            if let SshResponseFrame::Rpc(reply) = rpc_peer_read(&mut peer) {
                assert_eq!(reply.context, expected);
                assert_eq!(reply.result.unwrap_err(), "ssh_rpc_handler_failed");
                break;
            }
        }
        let _ = peer.shutdown(std::net::Shutdown::Both);
        server.join().unwrap().unwrap();
    }
}
