//! Bounded asynchronous local shell execution; durable evidence is separate from task completion.
use super::verification::{ApprovedVerification, now_ms};
use crate::lazy_worker::{LazyBoundedWorker, LazyWorkerSubmitError};
use deppy_core::WorkspaceId;
use deppy_core::work_management::*;
use secret::RedactionService;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, atomic::AtomicBool};
use std::time::{Duration, Instant};

pub(crate) enum VerificationJob {
    Run(ApprovedVerification),
    Load {
        workspace_id: WorkspaceId,
        token: u64,
    },
    RecordManual {
        evidence: ManualVerificationEvidence,
    },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VerificationError {
    Unavailable,
    Persistence,
    UnsafeMetadata,
    Conflict,
}
pub(crate) enum VerificationResult {
    Finished {
        evidence: Box<CommandVerificationEvidence>,
        cancelled: bool,
    },
    Loaded {
        workspace_id: WorkspaceId,
        token: u64,
        result: Result<Vec<VerificationEvidence>, VerificationError>,
    },
    ManualRecorded {
        evidence: ManualVerificationEvidence,
        result: Result<(), VerificationError>,
    },
    ExecutionFailed {
        id: VerificationId,
        target: VerificationTarget,
        error: VerificationError,
    },
}
pub(crate) struct VerificationWorker {
    worker: LazyBoundedWorker<VerificationJob, VerificationResult>,
    active: Option<(VerificationId, Arc<AtomicBool>)>,
}
impl VerificationWorker {
    pub fn new(
        data_dir: PathBuf,
        redaction: RedactionService,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        let worker = LazyBoundedWorker::new(
            "command-center-verification",
            Duration::from_secs(20),
            move || {
                let data_dir = data_dir.clone();
                let redaction = redaction.clone();
                let mut store = None;
                move |job| execute(&data_dir, &mut store, &redaction, job)
            },
            wake,
        );
        Self {
            worker,
            active: None,
        }
    }
    pub fn try_request(
        &mut self,
        job: VerificationJob,
    ) -> Result<(), LazyWorkerSubmitError<VerificationJob>> {
        let active = match &job {
            VerificationJob::Run(approved) => {
                Some((approved.id().clone(), approved.cancel.clone()))
            }
            _ => None,
        };
        self.worker.try_request(job)?;
        self.active = active;
        Ok(())
    }
    pub fn cancel(&mut self, id: &VerificationId) -> bool {
        if let Some((active, cancel)) = &self.active
            && active == id
        {
            cancel.store(true, Ordering::Release);
            true
        } else {
            false
        }
    }
    pub fn try_recv(&mut self) -> Option<Result<VerificationResult, VerificationError>> {
        let mut result = self
            .worker
            .try_recv()?
            .into_result()
            .map_err(|_| VerificationError::Unavailable);
        let cancelled = self
            .active
            .take()
            .is_some_and(|(_, cancel)| cancel.load(Ordering::Acquire));
        if let Ok(VerificationResult::Finished {
            cancelled: flag, ..
        }) = &mut result
        {
            *flag |= cancelled;
        }
        Some(result)
    }
}
impl Drop for VerificationWorker {
    fn drop(&mut self) {
        if let Some((_, cancel)) = &self.active {
            cancel.store(true, Ordering::Release);
        }
    }
}

fn safe_metadata(
    redaction: &RedactionService,
    id: &VerificationId,
    target: &VerificationTarget,
    spec: Option<&VerificationSpec>,
) -> bool {
    if redaction.ensure_safe().is_err() {
        return false;
    }
    let mut fields = vec![
        id.as_str(),
        target.workspace_id.0.as_str(),
        target.task_id.as_str(),
        target.attempt_id.as_str(),
    ];
    fields.extend(target.source_revision.as_deref());
    fields.extend(target.artifact_revision.as_deref());
    if let Some(spec) = spec {
        fields.push(&spec.command);
        fields.push(&spec.cwd);
    }
    fields
        .into_iter()
        .all(|value| redaction.redact_wrapped_text(value) == value)
}
fn open_store<'a>(
    data_dir: &std::path::Path,
    store: &'a mut Option<storage::work_management::CommandCenterStore>,
) -> Result<&'a mut storage::work_management::CommandCenterStore, VerificationError> {
    if store.is_none() {
        *store = Some(
            storage::work_management::CommandCenterStore::open(data_dir)
                .map_err(|_| VerificationError::Persistence)?,
        );
    }
    store.as_mut().ok_or(VerificationError::Persistence)
}
fn execute(
    data_dir: &std::path::Path,
    store: &mut Option<storage::work_management::CommandCenterStore>,
    redaction: &RedactionService,
    job: VerificationJob,
) -> VerificationResult {
    match job {
        VerificationJob::Run(mut approved) => {
            let failure =
                |e: &CommandVerificationEvidence, error| VerificationResult::ExecutionFailed {
                    id: e.id.clone(),
                    target: e.target.clone(),
                    error,
                };
            if !safe_metadata(
                redaction,
                approved.id(),
                approved.target(),
                Some(approved.spec()),
            ) {
                return failure(&approved.initial, VerificationError::UnsafeMetadata);
            }
            approved.initial.started_at_ms = now_ms();
            let store = match open_store(data_dir, store) {
                Ok(store) => store,
                Err(error) => return failure(&approved.initial, error),
            };
            match store.start_command_verification(&approved.initial) {
                Ok(storage::work_management::VerificationClaim::New) => {}
                Ok(storage::work_management::VerificationClaim::Existing(mut evidence)) => {
                    if evidence.outcome == CommandVerificationOutcome::Running {
                        terminal(
                            &mut evidence,
                            CommandVerificationOutcome::Interrupted,
                            Some("previous verification execution was not observed to finish"),
                        );
                        if store.finish_command_verification(&evidence).is_err() {
                            return failure(&approved.initial, VerificationError::Persistence);
                        }
                    }
                    // A prior journal claim is historical evidence, never permission to execute/apply again.
                    return VerificationResult::Finished {
                        evidence,
                        cancelled: true,
                    };
                }
                Ok(storage::work_management::VerificationClaim::Conflict) => {
                    return failure(&approved.initial, VerificationError::Conflict);
                }
                Err(_) => return failure(&approved.initial, VerificationError::Persistence),
            }
            let evidence = run(approved, redaction);
            if store.finish_command_verification(&evidence).is_err() {
                return failure(&evidence, VerificationError::Persistence);
            }
            VerificationResult::Finished {
                evidence: Box::new(evidence),
                cancelled: false,
            }
        }
        VerificationJob::Load {
            workspace_id,
            token,
        } => {
            let result = (|| {
                let store = open_store(data_dir, store)?;
                let mut history = store
                    .load_verifications(&workspace_id)
                    .map_err(|_| VerificationError::Persistence)?;
                // One outstanding job: Load cannot overlap this worker's execution. Unfinished
                // claims restored from a previous lifetime are terminalized, without spawning.
                for record in &mut history {
                    if let VerificationEvidence::AppLocal(evidence) = record
                        && evidence.outcome == CommandVerificationOutcome::Running
                    {
                        terminal(
                            evidence,
                            CommandVerificationOutcome::Interrupted,
                            Some("previous verification execution was not observed to finish"),
                        );
                        store
                            .finish_command_verification(evidence)
                            .map_err(|_| VerificationError::Persistence)?;
                    }
                }
                Ok(history)
            })();
            VerificationResult::Loaded {
                workspace_id,
                token,
                result,
            }
        }
        VerificationJob::RecordManual { mut evidence } => {
            let result = if !safe_metadata(redaction, &evidence.id, &evidence.target, None) {
                Err(VerificationError::UnsafeMetadata)
            } else {
                evidence.evidence = redaction.redact_wrapped_text(&evidence.evidence);
                open_store(data_dir, store).and_then(|store| {
                    store
                        .record_manual_verification(&evidence)
                        .map_err(|_| VerificationError::Persistence)
                })
            };
            VerificationResult::ManualRecorded { evidence, result }
        }
    }
}

fn terminal(
    evidence: &mut CommandVerificationEvidence,
    outcome: CommandVerificationOutcome,
    error: Option<&str>,
) {
    evidence.outcome = outcome;
    evidence.ended_at_ms = Some(now_ms().max(evidence.started_at_ms));
    evidence.error = error.map(str::to_owned);
}

fn run(
    approved: ApprovedVerification,
    redaction: &RedactionService,
) -> CommandVerificationEvidence {
    let ApprovedVerification {
        mut initial,
        cancel,
        canonical_folder,
    } = approved;
    if cancel.load(Ordering::Acquire) {
        terminal(&mut initial, CommandVerificationOutcome::Cancelled, None);
        return initial;
    }
    if std::fs::canonicalize(&initial.spec.cwd).ok().as_ref() != Some(&canonical_folder) {
        terminal(
            &mut initial,
            CommandVerificationOutcome::SpawnFailed,
            Some("approved local folder changed or is unavailable"),
        );
        return initial;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        use std::process::{Command, Stdio};
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(&initial.spec.command)
            .current_dir(&initial.spec.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let child = match command.spawn() {
            Ok(child) => child,
            Err(_) => {
                terminal(
                    &mut initial,
                    CommandVerificationOutcome::SpawnFailed,
                    Some("local shell process could not start"),
                );
                return initial;
            }
        };
        let mut process = OwnedProcess {
            child,
            reaped: false,
        };
        let mut stdout = process.child.stdout.take().expect("requested stdout pipe");
        let mut stderr = process.child.stderr.take().expect("requested stderr pipe");
        if nonblocking(&stdout).is_err() || nonblocking(&stderr).is_err() {
            terminal(
                &mut initial,
                CommandVerificationOutcome::IoFailed,
                Some("capture pipes could not become nonblocking"),
            );
            return initial;
        }
        let cap = initial.spec.output_limit_bytes as usize;
        let mut out = Capture::new(cap, redaction);
        let mut err = Capture::new(cap, redaction);
        let deadline = Instant::now() + Duration::from_millis(initial.spec.timeout_ms);
        let outcome = loop {
            if cancel.load(Ordering::Acquire) {
                break CommandVerificationOutcome::Cancelled;
            }
            if Instant::now() >= deadline {
                break CommandVerificationOutcome::TimedOut;
            }
            if out.drain(&mut stdout).is_err() || err.drain(&mut stderr).is_err() {
                break CommandVerificationOutcome::IoFailed;
            }
            match process.exited() {
                Ok(true) => break CommandVerificationOutcome::Passed,
                Ok(false) => std::thread::sleep(Duration::from_millis(5)),
                Err(_) => break CommandVerificationOutcome::IoFailed,
            }
        };
        // Keep the leader unreaped until killpg: its pid/pgid cannot be reused meanwhile.
        let status = process.kill_and_reap();
        let drain_deadline = Instant::now() + Duration::from_millis(100);
        while (!out.eof || !err.eof) && Instant::now() < drain_deadline {
            let _ = out.drain(&mut stdout);
            let _ = err.drain(&mut stderr);
            if !out.eof || !err.eof {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        if !out.eof {
            out.truncated = true;
        }
        if !err.eof {
            err.truncated = true;
        }
        initial.exit_code = status.as_ref().ok().and_then(|s| s.code());
        let actual = if outcome == CommandVerificationOutcome::Passed {
            match status {
                Ok(status) if status.success() => CommandVerificationOutcome::Passed,
                Ok(_) => CommandVerificationOutcome::NonzeroExit,
                Err(_) => CommandVerificationOutcome::IoFailed,
            }
        } else {
            outcome
        };
        let (text, truncated) = out.finish();
        initial.stdout = text;
        initial.stdout_truncated = truncated;
        let (text, truncated) = err.finish();
        initial.stderr = text;
        initial.stderr_truncated = truncated;
        terminal(
            &mut initial,
            actual,
            (actual == CommandVerificationOutcome::IoFailed)
                .then_some("verification process or output observation failed"),
        );
        initial
    }
    #[cfg(not(unix))]
    {
        let _ = redaction;
        terminal(
            &mut initial,
            CommandVerificationOutcome::SpawnFailed,
            Some("local verification is supported only on Unix"),
        );
        initial
    }
}

#[cfg(unix)]
fn nonblocking(pipe: &impl std::os::fd::AsRawFd) -> std::io::Result<()> {
    let fd = pipe.as_raw_fd();
    // SAFETY: pipe owns a live descriptor; flags are read and updated without changing ownership.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
struct OwnedProcess {
    child: std::process::Child,
    reaped: bool,
}
#[cfg(unix)]
impl OwnedProcess {
    fn exited(&self) -> std::io::Result<bool> {
        let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                self.child.id() as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(unsafe { info.si_pid() } != 0)
    }
    fn kill_and_reap(&mut self) -> std::io::Result<std::process::ExitStatus> {
        if !self.reaped {
            unsafe {
                libc::killpg(self.child.id() as libc::pid_t, libc::SIGKILL);
            }
            let _ = self.child.kill();
        }
        let status = self.child.wait();
        if status.is_ok() {
            self.reaped = true;
        }
        status
    }
}
#[cfg(unix)]
impl Drop for OwnedProcess {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.kill_and_reap();
        }
    }
}

#[cfg(unix)]
struct Capture {
    bytes: Vec<u8>,
    redactor: secret::StreamRedactor,
    cap: usize,
    truncated: bool,
    eof: bool,
}
#[cfg(unix)]
impl Capture {
    fn new(cap: usize, redaction: &RedactionService) -> Self {
        Self {
            bytes: Vec::new(),
            redactor: redaction.stream_redactor(),
            cap,
            truncated: false,
            eof: false,
        }
    }
    fn append(&mut self, bytes: &[u8]) {
        let count = (self.cap - self.bytes.len()).min(bytes.len());
        self.bytes.extend_from_slice(&bytes[..count]);
        self.truncated |= count < bytes.len();
    }
    fn drain(&mut self, pipe: &mut impl std::io::Read) -> std::io::Result<()> {
        let mut buffer = [0u8; 8192];
        // Bound each poll so a continuously writing child cannot starve cancellation/deadline.
        for _ in 0..16 {
            match pipe.read(&mut buffer) {
                Ok(0) => {
                    self.eof = true;
                    break;
                }
                Ok(count) => {
                    let redacted = self.redactor.redact_chunk(&buffer[..count]);
                    self.append(&redacted);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
    fn finish(mut self) -> (String, bool) {
        let tail = self.redactor.flush();
        self.append(&tail);
        let mut text = String::from_utf8_lossy(&self.bytes).into_owned();
        if text.len() > self.cap {
            let mut end = self.cap;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
            self.truncated = true;
        }
        (text, self.truncated)
    }
}

#[cfg(test)]
#[path = "verification_worker_tests.rs"]
mod tests;
