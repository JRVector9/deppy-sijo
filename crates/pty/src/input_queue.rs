use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PtyInputQueuePolicy {
    pub max_bytes: usize,
    pub max_messages: usize,
    pub large_paste_threshold: usize,
}

impl Default for PtyInputQueuePolicy {
    fn default() -> Self {
        Self {
            max_bytes: 4 * 1024 * 1024,
            max_messages: 256,
            large_paste_threshold: 64 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PtyInputEnqueueResult {
    Accepted,
    Backpressured { pressure: PtyInputPressure },
    Rejected { pressure: PtyInputPressure },
}

impl PtyInputEnqueueResult {
    pub fn is_accepted(&self) -> bool {
        matches!(self, Self::Accepted)
    }

    pub fn pressure(&self) -> Option<&PtyInputPressure> {
        match self {
            Self::Accepted => None,
            Self::Backpressured { pressure } | Self::Rejected { pressure } => Some(pressure),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PtyInputPressure {
    pub attempted_bytes: usize,
    pub queued_bytes: usize,
    pub queued_messages: usize,
    pub max_bytes: usize,
    pub max_messages: usize,
    pub reason: PtyInputRejectReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PtyInputRejectReason {
    QueueFull,
    SessionClosed,
    WriterUnavailable,
    PayloadTooLarge,
}

#[derive(Clone)]
pub(crate) struct PtyInputQueueState {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Debug)]
struct Inner {
    policy: PtyInputQueuePolicy,
    queued_bytes: usize,
    queued_messages: usize,
    closed: bool,
}

impl PtyInputQueueState {
    pub(crate) fn new(policy: PtyInputQueuePolicy) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                policy,
                queued_bytes: 0,
                queued_messages: 0,
                closed: false,
            })),
        }
    }

    pub(crate) fn policy(&self) -> PtyInputQueuePolicy {
        self.inner.lock().expect("PTY input queue mutex").policy
    }

    pub(crate) fn complete(&self, bytes: usize) {
        let mut inner = self.inner.lock().expect("PTY input queue mutex");
        inner.queued_bytes = inner.queued_bytes.saturating_sub(bytes);
        inner.queued_messages = inner.queued_messages.saturating_sub(1);
    }

    pub(crate) fn close(&self) {
        self.inner.lock().expect("PTY input queue mutex").closed = true;
    }

    fn pressure(
        inner: &Inner,
        attempted_bytes: usize,
        reason: PtyInputRejectReason,
    ) -> PtyInputPressure {
        PtyInputPressure {
            attempted_bytes,
            queued_bytes: inner.queued_bytes,
            queued_messages: inner.queued_messages,
            max_bytes: inner.policy.max_bytes,
            max_messages: inner.policy.max_messages,
            reason,
        }
    }
}

pub(crate) fn enqueue_input(
    tx: &SyncSender<Vec<u8>>,
    queue: &PtyInputQueueState,
    bytes: &[u8],
) -> anyhow::Result<PtyInputEnqueueResult> {
    let attempted_bytes = bytes.len();
    {
        let mut inner = queue.inner.lock().expect("PTY input queue mutex");
        if inner.closed {
            return Ok(PtyInputEnqueueResult::Rejected {
                pressure: PtyInputQueueState::pressure(
                    &inner,
                    attempted_bytes,
                    PtyInputRejectReason::SessionClosed,
                ),
            });
        }
        if attempted_bytes > inner.policy.max_bytes {
            return Ok(PtyInputEnqueueResult::Rejected {
                pressure: PtyInputQueueState::pressure(
                    &inner,
                    attempted_bytes,
                    PtyInputRejectReason::PayloadTooLarge,
                ),
            });
        }
        if inner.queued_messages >= inner.policy.max_messages
            || inner.queued_bytes.saturating_add(attempted_bytes) > inner.policy.max_bytes
        {
            return Ok(PtyInputEnqueueResult::Backpressured {
                pressure: PtyInputQueueState::pressure(
                    &inner,
                    attempted_bytes,
                    PtyInputRejectReason::QueueFull,
                ),
            });
        }
        inner.queued_messages += 1;
        inner.queued_bytes = inner.queued_bytes.saturating_add(attempted_bytes);
    }

    match tx.try_send(bytes.to_vec()) {
        Ok(()) => Ok(PtyInputEnqueueResult::Accepted),
        Err(TrySendError::Full(_)) => {
            queue.complete(attempted_bytes);
            let inner = queue.inner.lock().expect("PTY input queue mutex");
            Ok(PtyInputEnqueueResult::Backpressured {
                pressure: PtyInputQueueState::pressure(
                    &inner,
                    attempted_bytes,
                    PtyInputRejectReason::QueueFull,
                ),
            })
        }
        Err(TrySendError::Disconnected(_)) => {
            queue.complete(attempted_bytes);
            queue.close();
            let inner = queue.inner.lock().expect("PTY input queue mutex");
            Ok(PtyInputEnqueueResult::Rejected {
                pressure: PtyInputQueueState::pressure(
                    &inner,
                    attempted_bytes,
                    PtyInputRejectReason::WriterUnavailable,
                ),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::sync_channel;

    #[test]
    fn queue_accepts_within_budget() {
        let (tx, _rx) = sync_channel(2);
        let queue = PtyInputQueueState::new(PtyInputQueuePolicy {
            max_bytes: 10,
            max_messages: 2,
            large_paste_threshold: 4,
        });
        assert_eq!(
            enqueue_input(&tx, &queue, b"abc").unwrap(),
            PtyInputEnqueueResult::Accepted
        );
    }

    #[test]
    fn queue_reports_backpressure_without_blocking() {
        let (tx, _rx) = sync_channel(1);
        let queue = PtyInputQueueState::new(PtyInputQueuePolicy {
            max_bytes: 10,
            max_messages: 1,
            large_paste_threshold: 4,
        });
        assert!(enqueue_input(&tx, &queue, b"abc").unwrap().is_accepted());
        let result = enqueue_input(&tx, &queue, b"d").unwrap();
        assert!(matches!(
            result,
            PtyInputEnqueueResult::Backpressured {
                pressure: PtyInputPressure {
                    reason: PtyInputRejectReason::QueueFull,
                    ..
                }
            }
        ));
    }

    #[test]
    fn oversized_payload_is_rejected() {
        let (tx, _rx) = sync_channel(1);
        let queue = PtyInputQueueState::new(PtyInputQueuePolicy {
            max_bytes: 3,
            max_messages: 1,
            large_paste_threshold: 4,
        });
        let result = enqueue_input(&tx, &queue, b"abcd").unwrap();
        assert!(matches!(
            result,
            PtyInputEnqueueResult::Rejected {
                pressure: PtyInputPressure {
                    reason: PtyInputRejectReason::PayloadTooLarge,
                    ..
                }
            }
        ));
    }
}
