//! Task input is admitted against a server-captured execution, never a client-supplied PID.
use crate::remote_worker::RemoteWorkIdentity;
use crate::{PtyInputRejectReason, SessionId};
use deppy_core::process_identity::ProcessIdentity;
use deppy_core::work_management::{AgentSelection, OperationId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteTaskTarget {
    pub identity: RemoteWorkIdentity,
    pub session: SessionId,
    pub execution_generation: u64,
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RemoteTaskInputRequest {
    Observe {
        target: RemoteTaskTarget,
        request_id: u64,
    },
    Deliver {
        target: RemoteTaskTarget,
        execution_handle: String,
        operation_id: OperationId,
        prompt: String,
        deadline_unix_ms: u64,
    },
    ObserveSession {
        target: RemoteTaskTarget,
        request_id: u64,
    },
    CloseSession {
        target: RemoteTaskTarget,
        execution_handle: String,
        operation_id: OperationId,
        deadline_unix_ms: u64,
    },
}
impl std::fmt::Debug for RemoteTaskInputRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteTaskInputRequest")
            .field("target", self.target())
            .finish_non_exhaustive()
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteExecutionProof {
    pub target: RemoteTaskTarget,
    pub execution_handle: String,
    pub foreground: ProcessIdentity,
    pub execution: ProcessIdentity,
    pub profile: AgentSelection,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RemoteTaskInputResult {
    Admitted,
    Rejected { reason: PtyInputRejectReason },
    ObservationUnknown,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoteSessionSnapshot {
    pub terminal: Option<std::sync::Arc<terminal::TerminalViewportSnapshot>>,
    pub detected_status: Option<crate::SessionStatus>,
    pub exit_code: Option<Option<u32>>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RemoteSessionCloseResult {
    Closed,
    Rejected,
    ObservationUnknown,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RemoteTaskInputReply {
    Observed {
        request_id: u64,
        execution: Option<Box<RemoteExecutionProof>>,
    },
    Delivered {
        operation_id: OperationId,
        result: RemoteTaskInputResult,
    },
    SessionObserved {
        request_id: u64,
        snapshot: Option<Box<RemoteSessionSnapshot>>,
    },
    SessionClosed {
        operation_id: OperationId,
        result: RemoteSessionCloseResult,
    },
}
impl RemoteTaskInputRequest {
    pub fn target(&self) -> &RemoteTaskTarget {
        match self {
            Self::Observe { target, .. }
            | Self::ObserveSession { target, .. }
            | Self::Deliver { target, .. }
            | Self::CloseSession { target, .. } => target,
        }
    }
    pub fn is_valid(&self) -> bool {
        let t = self.target();
        if !t.identity.is_valid() || t.session.0 == 0 || t.execution_generation == 0 {
            return false;
        }
        match self {
            Self::Observe { request_id, .. } | Self::ObserveSession { request_id, .. } => {
                *request_id > 0
            }
            Self::CloseSession {
                execution_handle,
                operation_id,
                deadline_unix_ms,
                ..
            } => {
                *deadline_unix_ms > 0
                    && uuid::Uuid::parse_str(execution_handle).is_ok()
                    && uuid::Uuid::parse_str(operation_id.as_str()).is_ok()
            }
            Self::Deliver {
                execution_handle,
                operation_id,
                prompt,
                deadline_unix_ms,
                ..
            } => {
                *deadline_unix_ms > 0
                    && uuid::Uuid::parse_str(execution_handle).is_ok()
                    && uuid::Uuid::parse_str(operation_id.as_str()).is_ok()
                    && !prompt.is_empty()
                    && prompt.len() <= crate::remote_worker::REMOTE_WORK_PROMPT_MAX
                    && !prompt
                        .chars()
                        .any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
            }
        }
    }
    pub fn digest(&self) -> anyhow::Result<String> {
        Ok(Sha256::digest(postcard::to_allocvec(self)?)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect())
    }
    pub(crate) fn retained_bytes(&self) -> usize {
        let i = &self.target().identity;
        let base = i.device_id.capacity()
            + i.workspace_id.0.capacity()
            + i.attempt_id.0.capacity()
            + i.operation_id.0.capacity()
            + i.incarnation.0.capacity();
        base + match self {
            Self::Observe { .. } | Self::ObserveSession { .. } => 0,
            Self::Deliver {
                execution_handle,
                operation_id,
                prompt,
                ..
            } => execution_handle.capacity() + operation_id.0.capacity() + prompt.capacity(),
            Self::CloseSession {
                execution_handle,
                operation_id,
                ..
            } => execution_handle.capacity() + operation_id.0.capacity(),
        }
    }
    pub(crate) fn canonicalize(&mut self) {
        let t = match self {
            Self::Observe { target, .. }
            | Self::ObserveSession { target, .. }
            | Self::Deliver { target, .. }
            | Self::CloseSession { target, .. } => target,
        };
        t.identity.device_id.shrink_to_fit();
        t.identity.workspace_id.0.shrink_to_fit();
        t.identity.attempt_id.0.shrink_to_fit();
        t.identity.operation_id.0.shrink_to_fit();
        t.identity.incarnation.0.shrink_to_fit();
        if let Self::CloseSession {
            execution_handle,
            operation_id,
            ..
        } = self
        {
            execution_handle.shrink_to_fit();
            operation_id.0.shrink_to_fit();
        }
        if let Self::Deliver {
            execution_handle,
            operation_id,
            prompt,
            ..
        } = self
        {
            execution_handle.shrink_to_fit();
            operation_id.0.shrink_to_fit();
            prompt.shrink_to_fit();
        }
    }
}
pub fn unix_millis() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis()
        .try_into()
        .ok()
}
pub(crate) struct ExecutionRecord {
    pub target: RemoteTaskTarget,
    pub owner: Option<ProcessIdentity>,
    pub wrapped: bool,
    pub profile: AgentSelection,
    pub proof: Option<RemoteExecutionProof>,
    pub inputs: Vec<(OperationId, String, RemoteTaskInputResult)>,
    pub closes: Vec<(OperationId, String, RemoteSessionCloseResult)>,
}

impl ExecutionRecord {
    pub(crate) fn close_pending(&self) -> bool {
        self.closes
            .iter()
            .any(|(_, _, r)| *r == RemoteSessionCloseResult::ObservationUnknown)
            && (self.owner.is_some_and(|o| o.is_current())
                || self
                    .proof
                    .as_ref()
                    .is_some_and(|p| p.execution.is_current() || p.foreground.is_current()))
    }
}
/// Bounded kernel identity checks only; no process enumeration or executable-name guessing.
pub(crate) fn owns_foreground(owner: ProcessIdentity, foreground: ProcessIdentity) -> bool {
    if !owner.is_current() || !foreground.is_current() {
        return false;
    }
    let mut pid = foreground.pid;
    for _ in 0..32 {
        if pid == owner.pid {
            return owner.is_current() && foreground.is_current();
        }
        let Some(parent) = deppy_core::process_identity::pid_parent(pid) else {
            return false;
        };
        if parent == pid || parent == 0 {
            return false;
        }
        pid = parent;
    }
    false
}

/// The known launcher wrapper has exactly one foreground CLI child. Ambiguity fails closed.
/// Apple proc_listchildpids returns PID count (libsyscall/wrappers/libproc/libproc.c).
pub(crate) fn inner_execution(
    owner: ProcessIdentity,
    foreground: ProcessIdentity,
    wrapped: bool,
) -> Option<ProcessIdentity> {
    if !owns_foreground(owner, foreground) {
        return None;
    }
    let execution = if wrapped { only_child(owner)? } else { owner };
    (execution.is_current() && process_group(execution.pid) == Some(foreground.pid))
        .then_some(execution)
}
pub(crate) fn execution_is_current(
    owner: ProcessIdentity,
    foreground: ProcessIdentity,
    execution: ProcessIdentity,
) -> bool {
    owns_foreground(owner, foreground)
        && owns_foreground(owner, execution)
        && execution.is_current()
        && process_group(execution.pid) == Some(foreground.pid)
}
fn process_group(pid: u32) -> Option<u32> {
    #[cfg(unix)]
    {
        let pid = i32::try_from(pid).ok()?;
        u32::try_from(unsafe { libc::getpgid(pid) })
            .ok()
            .filter(|g| *g > 0)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        None
    }
}
fn only_child(owner: ProcessIdentity) -> Option<ProcessIdentity> {
    #[cfg(target_os = "macos")]
    let pid = {
        let mut pids = [0i32; 3];
        // Fixed capacity: no allocation, global process scan, retry, or subprocess.
        let count = unsafe {
            libc::proc_listchildpids(
                owner.pid.try_into().ok()?,
                pids.as_mut_ptr().cast(),
                std::mem::size_of_val(&pids) as i32,
            )
        };
        if count != 1 {
            return None;
        }
        u32::try_from(pids[0]).ok()?
    };
    #[cfg(target_os = "linux")]
    let pid = {
        use std::io::Read as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(format!("/proc/{}/task/{}/children", owner.pid, owner.pid))
            .ok()?;
        let mut bytes = [0u8; 256];
        let count = file.read(&mut bytes).ok()?;
        if count == bytes.len() {
            return None;
        }
        let text = std::str::from_utf8(&bytes[..count]).ok()?;
        let mut pids = text.split_whitespace();
        let pid = pids.next()?.parse::<u32>().ok()?;
        if pids.next().is_some() {
            return None;
        }
        pid
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = owner;
        None
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let child = ProcessIdentity::capture(pid)?;
        (deppy_core::process_identity::pid_parent(pid) == Some(owner.pid) && owner.is_current())
            .then_some(child)
    }
}

/// At most 160x80 cells plus 64KiB sparse graphemes; no full scrollback retention.
pub fn valid_session_snapshot(snapshot: &terminal::TerminalViewportSnapshot) -> bool {
    let cells = usize::from(snapshot.cols) * usize::from(snapshot.rows);
    snapshot.cols > 0
        && snapshot.cols <= 160
        && snapshot.rows > 0
        && snapshot.rows <= 80
        && snapshot.visible_cells.len() == cells
        && snapshot.history.is_some_and(|h| {
            h.generation > 0
                && h.total_lines >= u32::from(snapshot.rows)
                && snapshot.scroll_offset >= 0
                && snapshot.scroll_offset as u32 <= h.total_lines - u32::from(snapshot.rows)
                && h.first_line
                    .checked_add(snapshot.scroll_offset as u64)
                    .is_some()
        })
        && snapshot.title.as_ref().is_none_or(|t| t.len() <= 4096)
        && snapshot.dirty_ranges.len() <= 80
        && snapshot
            .dirty_ranges
            .iter()
            .all(|r| r.start <= r.end && r.end <= cells)
        && snapshot
            .graphemes
            .iter()
            .try_fold(0usize, |n, g| n.checked_add(g.text.len()))
            .is_some_and(|n| n <= 64 * 1024)
        && terminal::validate_cell_graphemes(&snapshot.visible_cells, &snapshot.graphemes).is_ok()
}
