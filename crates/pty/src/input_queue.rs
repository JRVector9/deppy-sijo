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
        self.release(bytes, 1);
    }

    /// 예약 반환 — writer 완료(complete) 및 전송 실패 롤백 공용.
    fn release(&self, bytes: usize, messages: usize) {
        let mut inner = self.inner.lock().expect("PTY input queue mutex");
        inner.queued_bytes = inner.queued_bytes.saturating_sub(bytes);
        inner.queued_messages = inner.queued_messages.saturating_sub(messages);
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

/// 입력을 큐에 넣는다 — try_send만 사용하고 즉시 반환한다 (blocking write는
/// writer thread 소유 — full-duplex deadlock 방지 불변식).
///
/// 대량 paste는 `large_paste_threshold` 단위 chunk로 쪼갠다 — writer가 chunk마다
/// complete()하므로 pressure가 점진적으로 빠지고, 한 메시지가 budget을 독점하지
/// 않는다. 수용은 all-or-nothing: 부분 전송(paste 반토막)을 만들지 않는다.
///
/// 결과 경계:
/// - Backpressured(QueueFull): 지금은 가득 찼지만 큐가 빠지면 수용 가능 — 재시도 대상
/// - Rejected(PayloadTooLarge): 빈 큐라도 정책상 영원히 수용 불가 — 재시도 무의미
pub(crate) fn enqueue_input(
    tx: &SyncSender<Vec<u8>>,
    queue: &PtyInputQueueState,
    bytes: &[u8],
) -> anyhow::Result<PtyInputEnqueueResult> {
    let attempted_bytes = bytes.len();
    let (chunk_size, chunk_count) = {
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
        let chunk_size = inner.policy.large_paste_threshold.max(1);
        let chunk_count = attempted_bytes.div_ceil(chunk_size);
        // 정책상 영원히 수용 불가능한 payload — 큐가 비어도 못 들어간다
        if attempted_bytes > inner.policy.max_bytes || chunk_count > inner.policy.max_messages {
            return Ok(PtyInputEnqueueResult::Rejected {
                pressure: PtyInputQueueState::pressure(
                    &inner,
                    attempted_bytes,
                    PtyInputRejectReason::PayloadTooLarge,
                ),
            });
        }
        // 빈 입력 — 쓸 것이 없다. 슬롯을 소모하지 않는다.
        if chunk_count == 0 {
            return Ok(PtyInputEnqueueResult::Accepted);
        }
        if inner.queued_messages.saturating_add(chunk_count) > inner.policy.max_messages
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
        // all-or-nothing 예약 — 아래 try_send가 전부 성공함을 보장한다:
        // queued_messages는 채널 점유의 상한(sent-but-not-received ⊆
        // reserved-not-completed)이고 채널 capacity == max_messages,
        // enqueuer는 세션당 단일(&mut self)이므로 예약 후 Full은 나올 수 없다.
        inner.queued_messages += chunk_count;
        inner.queued_bytes = inner.queued_bytes.saturating_add(attempted_bytes);
        (chunk_size, chunk_count)
    };

    let mut sent_bytes = 0usize;
    let mut sent_chunks = 0usize;
    for chunk in bytes.chunks(chunk_size) {
        match tx.try_send(chunk.to_vec()) {
            Ok(()) => {
                sent_bytes += chunk.len();
                sent_chunks += 1;
            }
            // 예약 산술상 도달 불가 경로 — 그래도 조용한 유실 대신 방어적으로
            // 미전송분 예약을 되돌리고 명시적 결과를 반환한다 (테스트에서 임의
            // capacity 채널로는 도달 가능).
            Err(TrySendError::Full(_)) => {
                queue.release(attempted_bytes - sent_bytes, chunk_count - sent_chunks);
                let inner = queue.inner.lock().expect("PTY input queue mutex");
                return Ok(PtyInputEnqueueResult::Backpressured {
                    pressure: PtyInputQueueState::pressure(
                        &inner,
                        attempted_bytes,
                        PtyInputRejectReason::QueueFull,
                    ),
                });
            }
            Err(TrySendError::Disconnected(_)) => {
                queue.release(attempted_bytes - sent_bytes, chunk_count - sent_chunks);
                queue.close();
                let inner = queue.inner.lock().expect("PTY input queue mutex");
                return Ok(PtyInputEnqueueResult::Rejected {
                    pressure: PtyInputQueueState::pressure(
                        &inner,
                        attempted_bytes,
                        PtyInputRejectReason::WriterUnavailable,
                    ),
                });
            }
        }
    }
    Ok(PtyInputEnqueueResult::Accepted)
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

    /// threshold 초과 paste는 chunk로 쪼개져 전부(all-or-nothing) 수용되고,
    /// 순서/내용이 보존된다.
    #[test]
    fn large_paste_is_chunked_and_accepted() {
        let (tx, rx) = sync_channel(3);
        let queue = PtyInputQueueState::new(PtyInputQueuePolicy {
            max_bytes: 100,
            max_messages: 3,
            large_paste_threshold: 2,
        });
        assert!(enqueue_input(&tx, &queue, b"abcdef").unwrap().is_accepted());
        let chunks: Vec<Vec<u8>> = rx.try_iter().collect();
        assert_eq!(chunks, vec![b"ab".to_vec(), b"cd".to_vec(), b"ef".to_vec()]);
        // 예약 회계: 3 chunk / 6 byte가 큐에 잡혀 있어야 한다 (complete 전).
        let result = enqueue_input(&tx, &queue, b"z").unwrap();
        match result {
            PtyInputEnqueueResult::Backpressured { pressure } => {
                assert_eq!(pressure.reason, PtyInputRejectReason::QueueFull);
                assert_eq!(pressure.queued_bytes, 6);
                assert_eq!(pressure.queued_messages, 3);
            }
            other => panic!("QueueFull backpressure 기대, 실제: {other:?}"),
        }
    }

    /// byte budget에는 들어가지만 chunk 수가 message budget을 넘으면 —
    /// 큐가 비어도 영원히 수용 불가 → 재시도 무의미한 Rejected.
    #[test]
    fn chunk_count_over_message_budget_is_rejected_not_backpressured() {
        let (tx, _rx) = sync_channel(2);
        let queue = PtyInputQueueState::new(PtyInputQueuePolicy {
            max_bytes: 100,
            max_messages: 2,
            large_paste_threshold: 1,
        });
        let result = enqueue_input(&tx, &queue, b"abc").unwrap();
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

    /// 경계: byte budget에 정확히 맞으면 수용, 1 byte 초과분은 backpressure.
    /// writer가 complete하면 다시 수용된다 (backpressure = 재시도 가능).
    #[test]
    fn backpressure_boundary_recovers_after_complete() {
        let (tx, rx) = sync_channel(2);
        let queue = PtyInputQueueState::new(PtyInputQueuePolicy {
            max_bytes: 4,
            max_messages: 2,
            large_paste_threshold: 4,
        });
        assert!(enqueue_input(&tx, &queue, b"abcd").unwrap().is_accepted());
        let result = enqueue_input(&tx, &queue, b"e").unwrap();
        assert!(matches!(
            result,
            PtyInputEnqueueResult::Backpressured {
                pressure: PtyInputPressure {
                    reason: PtyInputRejectReason::QueueFull,
                    ..
                }
            }
        ));
        // writer thread 역할 재현: 메시지 소비 + complete → budget 반환
        let written = rx.try_recv().unwrap();
        queue.complete(written.len());
        assert!(enqueue_input(&tx, &queue, b"e").unwrap().is_accepted());
    }

    /// 빈 입력은 슬롯/바이트를 소모하지 않고 수용된다.
    #[test]
    fn empty_input_is_accepted_without_reserving_budget() {
        let (tx, _rx) = sync_channel(1);
        let queue = PtyInputQueueState::new(PtyInputQueuePolicy {
            max_bytes: 4,
            max_messages: 1,
            large_paste_threshold: 4,
        });
        assert!(enqueue_input(&tx, &queue, b"").unwrap().is_accepted());
        // 빈 입력이 슬롯을 잡았다면 이 full-budget 입력은 backpressure였을 것
        assert!(enqueue_input(&tx, &queue, b"abcd").unwrap().is_accepted());
    }

    #[test]
    fn closed_queue_rejects_with_session_closed() {
        let (tx, _rx) = sync_channel(1);
        let queue = PtyInputQueueState::new(PtyInputQueuePolicy::default());
        queue.close();
        let result = enqueue_input(&tx, &queue, b"a").unwrap();
        assert!(matches!(
            result,
            PtyInputEnqueueResult::Rejected {
                pressure: PtyInputPressure {
                    reason: PtyInputRejectReason::SessionClosed,
                    ..
                }
            }
        ));
    }

    /// writer 소멸(수신측 drop) → WriterUnavailable + 큐 close, 예약 롤백.
    #[test]
    fn disconnected_writer_rejects_and_closes_queue() {
        let (tx, rx) = sync_channel(1);
        drop(rx);
        let queue = PtyInputQueueState::new(PtyInputQueuePolicy::default());
        let result = enqueue_input(&tx, &queue, b"a").unwrap();
        match result {
            PtyInputEnqueueResult::Rejected { pressure } => {
                assert_eq!(pressure.reason, PtyInputRejectReason::WriterUnavailable);
                // 롤백 확인: 예약이 남아 있으면 안 된다
                assert_eq!(pressure.queued_bytes, 0);
                assert_eq!(pressure.queued_messages, 0);
            }
            other => panic!("WriterUnavailable 기대, 실제: {other:?}"),
        }
        // 이후 시도는 SessionClosed로 즉시 거절
        let result = enqueue_input(&tx, &queue, b"a").unwrap();
        assert!(matches!(
            result,
            PtyInputEnqueueResult::Rejected {
                pressure: PtyInputPressure {
                    reason: PtyInputRejectReason::SessionClosed,
                    ..
                }
            }
        ));
    }
}
