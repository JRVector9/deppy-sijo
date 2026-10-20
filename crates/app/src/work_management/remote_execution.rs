//! A persisted send claim is never automatic permission to replay after reconnect.
use super::devices::ExecutionLease;
use deppy_core::work_management::{OperationId, RuntimeBinding};

pub const REMOTE_DELIVERY_MAX: usize = 256;
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryStatus {
    Durable,
    Sent,
    Running(RuntimeBinding),
    Rejected,
    ObservationUnknown,
    Finished,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteDelivery {
    pub lease: ExecutionLease,
    pub prompt: String,
    pub status: DeliveryStatus,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryError {
    Invalid,
    Full,
    Conflict,
    NotSendable,
    Missing,
}
#[derive(Default)]
pub struct RemoteExecutionLedger {
    records: Vec<RemoteDelivery>,
}
impl RemoteExecutionLedger {
    pub fn restore(records: Vec<RemoteDelivery>) -> Result<Self, DeliveryError> {
        if records.len() > REMOTE_DELIVERY_MAX {
            return Err(DeliveryError::Full);
        }
        let mut ledger = Self::default();
        for mut record in records {
            if ledger.records.iter().any(|r| {
                r.lease.request.operation_id == record.lease.request.operation_id
                    || r.lease.request.attempt_id == record.lease.request.attempt_id
            }) {
                return Err(DeliveryError::Conflict);
            }
            ledger.record_durable(record.lease.clone(), record.prompt.clone())?;
            if matches!(
                record.status,
                DeliveryStatus::Durable | DeliveryStatus::Sent | DeliveryStatus::Running(_)
            ) {
                record.status = DeliveryStatus::ObservationUnknown;
            }
            *ledger.records.last_mut().ok_or(DeliveryError::Missing)? = record;
        }
        Ok(ledger)
    }
    pub fn record_durable(
        &mut self,
        lease: ExecutionLease,
        prompt: String,
    ) -> Result<(), DeliveryError> {
        if prompt.is_empty()
            || prompt.len() > deppy_core::work_management::WORK_INPUT_MAX_BYTES
            || prompt.contains('\0')
            || lease.grant_epoch == 0
            || lease.lease_epoch == 0
            || lease.connection_epoch == 0
        {
            return Err(DeliveryError::Invalid);
        }
        if let Some(existing) = self.records.iter().find(|r| {
            r.lease.request.operation_id == lease.request.operation_id
                || r.lease.request.attempt_id == lease.request.attempt_id
        }) {
            return if existing.lease == lease && existing.prompt == prompt {
                Ok(())
            } else {
                Err(DeliveryError::Conflict)
            };
        }
        if self.records.len() == REMOTE_DELIVERY_MAX {
            return Err(DeliveryError::Full);
        }
        self.records.push(RemoteDelivery {
            lease,
            prompt,
            status: DeliveryStatus::Durable,
        });
        Ok(())
    }
    /// Persist the returned Sent state before invoking any transport.
    pub fn claim_send(&mut self, operation: &OperationId) -> Result<RemoteDelivery, DeliveryError> {
        let record = self
            .records
            .iter_mut()
            .find(|r| r.lease.request.operation_id == *operation)
            .ok_or(DeliveryError::Missing)?;
        if record.status != DeliveryStatus::Durable {
            return Err(DeliveryError::NotSendable);
        }
        record.status = DeliveryStatus::Sent;
        Ok(record.clone())
    }
    pub fn admit(
        &mut self,
        lease: &ExecutionLease,
        binding: RuntimeBinding,
    ) -> Result<(), DeliveryError> {
        let record = self
            .records
            .iter_mut()
            .find(|r| r.lease.request.operation_id == lease.request.operation_id)
            .ok_or(DeliveryError::Missing)?;
        if record.lease != *lease
            || binding.incarnation != lease.incarnation
            || binding.execution_generation != lease.lease_epoch
            || binding.session_id.0 == 0
        {
            return Err(DeliveryError::Conflict);
        }
        match &record.status {
            DeliveryStatus::Sent | DeliveryStatus::ObservationUnknown => {
                record.status = DeliveryStatus::Running(binding);
                Ok(())
            }
            DeliveryStatus::Running(existing) if *existing == binding => Ok(()),
            _ => Err(DeliveryError::Conflict),
        }
    }
    pub fn disconnected(&mut self, device: &str, connection_epoch: u64) {
        for record in &mut self.records {
            if record.lease.request.device_id == device
                && record.lease.connection_epoch == connection_epoch
                && matches!(
                    record.status,
                    DeliveryStatus::Sent | DeliveryStatus::Running(_)
                )
            {
                record.status = DeliveryStatus::ObservationUnknown;
            }
        }
    }
    pub fn records(&self) -> &[RemoteDelivery] {
        &self.records
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work_management::devices::LeaseRequest;
    use deppy_core::work_management::{AgentSelection, AttemptId, RuntimeIncarnationId};
    use deppy_core::{SessionId, WorkspaceId};
    fn lease() -> ExecutionLease {
        ExecutionLease {
            request: LeaseRequest {
                device_id: "worker-a".into(),
                workspace_id: WorkspaceId("workspace-a".into()),
                attempt_id: AttemptId("attempt-a".into()),
                operation_id: OperationId("operation-a".into()),
                folder: "/fixture/a".into(),
                agent: AgentSelection {
                    provider: "codex".into(),
                    model: None,
                    effort: None,
                },
            },
            folder_anchor: "anchor-a".into(),
            profile_id: "profile-a".into(),
            grant_epoch: 1,
            lease_epoch: 1,
            connection_epoch: 1,
            incarnation: RuntimeIncarnationId("runtime-a".into()),
        }
    }
    fn binding() -> RuntimeBinding {
        RuntimeBinding {
            incarnation: RuntimeIncarnationId("runtime-a".into()),
            session_id: SessionId(1),
            execution_generation: 1,
        }
    }
    #[test]
    fn lost_ack_restore_and_reconnect_never_resend_or_accept_an_alias() {
        let lease = lease();
        let mut ledger = RemoteExecutionLedger::default();
        ledger
            .record_durable(lease.clone(), "do bounded work".into())
            .unwrap();
        assert_eq!(
            ledger
                .claim_send(&lease.request.operation_id)
                .unwrap()
                .status,
            DeliveryStatus::Sent
        );
        assert_eq!(
            ledger.claim_send(&lease.request.operation_id),
            Err(DeliveryError::NotSendable)
        );
        ledger.disconnected("worker-a", 1);
        assert_eq!(
            ledger.records()[0].status,
            DeliveryStatus::ObservationUnknown
        );
        let mut restored = RemoteExecutionLedger::restore(ledger.records().to_vec()).unwrap();
        assert_eq!(
            restored.claim_send(&lease.request.operation_id),
            Err(DeliveryError::NotSendable)
        );
        let mut wrong = lease.clone();
        wrong.connection_epoch = 2;
        assert_eq!(
            restored.admit(&wrong, binding()),
            Err(DeliveryError::Conflict)
        );
        restored.admit(&lease, binding()).unwrap();
        restored.admit(&lease, binding()).unwrap();
        let mut other = binding();
        other.session_id = SessionId(2);
        assert_eq!(restored.admit(&lease, other), Err(DeliveryError::Conflict));
    }
    #[test]
    fn restore_disarms_even_durable_work_and_checks_prompt_identity() {
        let lease = lease();
        let mut ledger = RemoteExecutionLedger::default();
        ledger
            .record_durable(lease.clone(), "first".into())
            .unwrap();
        assert_eq!(
            ledger.record_durable(lease.clone(), "different".into()),
            Err(DeliveryError::Conflict)
        );
        let mut restored = RemoteExecutionLedger::restore(ledger.records().to_vec()).unwrap();
        assert_eq!(
            restored.claim_send(&lease.request.operation_id),
            Err(DeliveryError::NotSendable)
        );
        assert!(
            RemoteExecutionLedger::restore(vec![
                ledger.records()[0].clone();
                REMOTE_DELIVERY_MAX + 1
            ])
            .is_err()
        );
    }
}
