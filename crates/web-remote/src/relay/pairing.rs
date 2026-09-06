use std::collections::HashMap;
use std::fmt;

use subtle::ConstantTimeEq;

use super::contract::{ConnectionId, PairingId};

pub const PAIRING_TTL_SECS: u64 = 5 * 60;
pub const MAX_SECRET_ATTEMPTS: u8 = 5;
pub const PAIRING_SECRET_BYTES: usize = 32;
pub const MAX_PAIRING_RECORDS: usize = 256;
const MAX_PAIRING_ISSUE_ATTEMPTS: usize = 8;

pub struct PairingSecret([u8; PAIRING_SECRET_BYTES]);

impl PairingSecret {
    pub fn take_from_bytes(bytes: &mut [u8; PAIRING_SECRET_BYTES]) -> Self {
        let secret = Self(*bytes);
        erase_secret_bytes(bytes);
        secret
    }

    pub fn generate() -> Result<Self, getrandom::Error> {
        let mut bytes = [0u8; PAIRING_SECRET_BYTES];
        if let Err(error) = getrandom::fill(&mut bytes) {
            erase_secret_bytes(&mut bytes);
            return Err(error);
        }
        Ok(Self::take_from_bytes(&mut bytes))
    }

    fn clear(&mut self) {
        erase_secret_bytes(&mut self.0);
    }
}

impl fmt::Debug for PairingSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PairingSecret([REDACTED])")
    }
}

impl Drop for PairingSecret {
    fn drop(&mut self) {
        self.clear();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PairingError {
    NotVerified,
    AlreadyVerified,
    InvalidSecret,
    AttemptsExhausted,
    Expired,
    Rejected,
    Consumed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PairingRegistryError {
    NotFound,
    DuplicateId,
    CapacityExhausted,
    ClockRollback,
    EntropyUnavailable,
    IdGenerationExhausted,
    Pairing(PairingError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PairingBinding {
    connection_id: ConnectionId,
    peer_identity_fingerprint: [u8; 32],
    transcript_hash: [u8; 32],
}

impl PairingBinding {
    pub(crate) const fn new(
        connection_id: ConnectionId,
        peer_identity_fingerprint: [u8; 32],
        transcript_hash: [u8; 32],
    ) -> Self {
        Self {
            connection_id,
            peer_identity_fingerprint,
            transcript_hash,
        }
    }

    pub const fn connection_id(&self) -> ConnectionId {
        self.connection_id
    }

    pub const fn peer_identity_fingerprint(&self) -> [u8; 32] {
        self.peer_identity_fingerprint
    }

    pub const fn transcript_hash(&self) -> [u8; 32] {
        self.transcript_hash
    }
}

pub struct PairingApproval {
    pairing_id: PairingId,
    binding: PairingBinding,
}

pub struct IssuedPairing {
    id: PairingId,
    secret: PairingSecret,
}

impl IssuedPairing {
    pub const fn id(&self) -> PairingId {
        self.id
    }

    pub const fn secret(&self) -> &PairingSecret {
        &self.secret
    }
}

impl fmt::Debug for IssuedPairing {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IssuedPairing")
            .field("id", &self.id)
            .field("secret", &"[REDACTED]")
            .finish()
    }
}

#[allow(dead_code)] // Consumed by the crypto binding added in the next Task 1 slice.
impl PairingApproval {
    pub(crate) const fn pairing_id(&self) -> PairingId {
        self.pairing_id
    }

    pub(crate) const fn connection_id(&self) -> ConnectionId {
        self.binding.connection_id()
    }

    pub(crate) const fn peer_identity_fingerprint(&self) -> [u8; 32] {
        self.binding.peer_identity_fingerprint()
    }

    pub(crate) const fn transcript_hash(&self) -> [u8; 32] {
        self.binding.transcript_hash()
    }

    pub(crate) const fn binding(&self) -> PairingBinding {
        self.binding
    }
}

impl fmt::Debug for PairingApproval {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PairingApproval([REDACTED])")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PairingState {
    Pending,
    Verified,
    AttemptsExhausted,
    Expired,
    Rejected,
    Consumed,
}

pub struct PendingPairing {
    id: PairingId,
    secret: PairingSecret,
    issued_at: u64,
    last_checked_at: u64,
    failed_attempts: u8,
    state: PairingState,
    verified_binding: Option<PairingBinding>,
}

impl PendingPairing {
    const fn new(id: PairingId, secret: PairingSecret, unix_secs: u64) -> Self {
        Self {
            id,
            secret,
            issued_at: unix_secs,
            last_checked_at: unix_secs,
            failed_attempts: 0,
            state: PairingState::Pending,
            verified_binding: None,
        }
    }

    pub const fn id(&self) -> PairingId {
        self.id
    }

    fn verify_secret_for_binding(
        &mut self,
        candidate: &PairingSecret,
        unix_secs: u64,
        binding: PairingBinding,
    ) -> Result<(), PairingError> {
        self.require_active(unix_secs)?;

        if self.state == PairingState::Verified {
            return Err(PairingError::AlreadyVerified);
        }

        if constant_time_secret_eq(&self.secret.0, &candidate.0) {
            self.state = PairingState::Verified;
            self.verified_binding = Some(binding);
            self.secret.clear();
            return Ok(());
        }

        self.failed_attempts = self.failed_attempts.saturating_add(1);
        if self.failed_attempts >= MAX_SECRET_ATTEMPTS {
            self.state = PairingState::AttemptsExhausted;
            self.secret.clear();
            Err(PairingError::AttemptsExhausted)
        } else {
            Err(PairingError::InvalidSecret)
        }
    }

    #[cfg(test)]
    fn verify_secret(
        &mut self,
        candidate: &PairingSecret,
        unix_secs: u64,
    ) -> Result<(), PairingError> {
        self.verify_secret_for_binding(
            candidate,
            unix_secs,
            PairingBinding::new(ConnectionId::from_bytes([0x71; 16]), [0x72; 32], [0x73; 32]),
        )
    }

    pub fn consume(&mut self, unix_secs: u64) -> Result<PairingApproval, PairingError> {
        self.require_active(unix_secs)?;

        if self.state != PairingState::Verified {
            return Err(PairingError::NotVerified);
        }
        let binding = self
            .verified_binding
            .take()
            .ok_or(PairingError::NotVerified)?;
        self.state = PairingState::Consumed;
        self.secret.clear();
        Ok(PairingApproval {
            pairing_id: self.id,
            binding,
        })
    }

    pub fn reject(&mut self) -> Result<(), PairingError> {
        if let Some(error) = self.terminal_error() {
            return Err(error);
        }
        self.state = PairingState::Rejected;
        self.secret.clear();
        Ok(())
    }

    fn require_active(&mut self, unix_secs: u64) -> Result<(), PairingError> {
        if let Some(error) = self.terminal_error() {
            return Err(error);
        }

        if unix_secs < self.last_checked_at {
            self.state = PairingState::Expired;
            self.secret.clear();
            return Err(PairingError::Expired);
        }
        self.last_checked_at = unix_secs;

        let deadline = self.issued_at.saturating_add(PAIRING_TTL_SECS);
        if unix_secs >= deadline {
            self.state = PairingState::Expired;
            self.secret.clear();
            return Err(PairingError::Expired);
        }
        Ok(())
    }

    const fn terminal_error(&self) -> Option<PairingError> {
        match self.state {
            PairingState::Pending | PairingState::Verified => None,
            PairingState::AttemptsExhausted => Some(PairingError::AttemptsExhausted),
            PairingState::Expired => Some(PairingError::Expired),
            PairingState::Rejected => Some(PairingError::Rejected),
            PairingState::Consumed => Some(PairingError::Consumed),
        }
    }

    const fn deadline(&self) -> u64 {
        self.issued_at.saturating_add(PAIRING_TTL_SECS)
    }
}

impl fmt::Debug for PendingPairing {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PendingPairing")
            .field("id", &self.id)
            .field("secret", &"[REDACTED]")
            .field("issued_at", &self.issued_at)
            .field("last_checked_at", &self.last_checked_at)
            .field("failed_attempts", &self.failed_attempts)
            .field("state", &self.state)
            .finish()
    }
}

#[derive(Clone, Copy)]
struct PairingTombstone {
    error: PairingError,
    expires_at: u64,
}

pub struct PairingRegistry {
    pending: HashMap<PairingId, PendingPairing>,
    tombstones: HashMap<PairingId, PairingTombstone>,
    last_checked_at: Option<u64>,
}

impl PairingRegistry {
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
            tombstones: HashMap::new(),
            last_checked_at: None,
        }
    }

    pub fn issue(&mut self, unix_secs: u64) -> Result<IssuedPairing, PairingRegistryError> {
        if self.prepare(unix_secs) {
            return Err(PairingRegistryError::ClockRollback);
        }
        if self.pending.len() + self.tombstones.len() >= MAX_PAIRING_RECORDS {
            return Err(PairingRegistryError::CapacityExhausted);
        }

        for _ in 0..MAX_PAIRING_ISSUE_ATTEMPTS {
            let mut id_bytes = [0u8; super::contract::RELAY_ID_BYTES];
            getrandom::fill(&mut id_bytes).map_err(|_| PairingRegistryError::EntropyUnavailable)?;
            let id = PairingId::from_bytes(id_bytes);
            if self.pending.contains_key(&id) || self.tombstones.contains_key(&id) {
                continue;
            }

            let issued_secret =
                PairingSecret::generate().map_err(|_| PairingRegistryError::EntropyUnavailable)?;
            let mut stored_bytes = issued_secret.0;
            let stored_secret = PairingSecret::take_from_bytes(&mut stored_bytes);
            self.insert(id, stored_secret, unix_secs)?;
            return Ok(IssuedPairing {
                id,
                secret: issued_secret,
            });
        }
        Err(PairingRegistryError::IdGenerationExhausted)
    }

    #[cfg(test)]
    fn create_for_test(
        &mut self,
        id: PairingId,
        secret: PairingSecret,
        unix_secs: u64,
    ) -> Result<(), PairingRegistryError> {
        if self.prepare(unix_secs) {
            return Err(PairingRegistryError::ClockRollback);
        }
        self.insert(id, secret, unix_secs)
    }

    fn insert(
        &mut self,
        id: PairingId,
        secret: PairingSecret,
        unix_secs: u64,
    ) -> Result<(), PairingRegistryError> {
        if self.pending.contains_key(&id) || self.tombstones.contains_key(&id) {
            return Err(PairingRegistryError::DuplicateId);
        }
        if self.pending.len() + self.tombstones.len() >= MAX_PAIRING_RECORDS {
            return Err(PairingRegistryError::CapacityExhausted);
        }
        self.pending
            .insert(id, PendingPairing::new(id, secret, unix_secs));
        Ok(())
    }

    pub fn verify_secret_for_binding(
        &mut self,
        id: PairingId,
        candidate: &PairingSecret,
        unix_secs: u64,
        binding: PairingBinding,
    ) -> Result<(), PairingRegistryError> {
        if self.prepare(unix_secs) {
            return Err(self.expire_target_after_clock_rollback(id, unix_secs));
        }
        if let Some(tombstone) = self.tombstones.get(&id) {
            return Err(PairingRegistryError::Pairing(tombstone.error));
        }
        let (deadline, result) = {
            let pairing = self
                .pending
                .get_mut(&id)
                .ok_or(PairingRegistryError::NotFound)?;
            (
                pairing.deadline(),
                pairing.verify_secret_for_binding(candidate, unix_secs, binding),
            )
        };
        if let Err(error) = result {
            if is_terminal(error) {
                self.finish_terminal(id, error, deadline, unix_secs);
            }
            return Err(PairingRegistryError::Pairing(error));
        }
        Ok(())
    }

    #[cfg(test)]
    fn verify_secret(
        &mut self,
        id: PairingId,
        candidate: &PairingSecret,
        unix_secs: u64,
    ) -> Result<(), PairingRegistryError> {
        self.verify_secret_for_binding(
            id,
            candidate,
            unix_secs,
            PairingBinding::new(ConnectionId::from_bytes([0x71; 16]), [0x72; 32], [0x73; 32]),
        )
    }

    pub fn consume(
        &mut self,
        id: PairingId,
        unix_secs: u64,
    ) -> Result<PairingApproval, PairingRegistryError> {
        if self.prepare(unix_secs) {
            return Err(self.expire_target_after_clock_rollback(id, unix_secs));
        }
        if let Some(tombstone) = self.tombstones.get(&id) {
            return Err(PairingRegistryError::Pairing(tombstone.error));
        }
        let (deadline, result) = {
            let pairing = self
                .pending
                .get_mut(&id)
                .ok_or(PairingRegistryError::NotFound)?;
            (pairing.deadline(), pairing.consume(unix_secs))
        };
        match result {
            Ok(approval) => {
                self.finish_terminal(id, PairingError::Consumed, deadline, unix_secs);
                Ok(approval)
            }
            Err(error) => {
                if is_terminal(error) {
                    self.finish_terminal(id, error, deadline, unix_secs);
                }
                Err(PairingRegistryError::Pairing(error))
            }
        }
    }

    pub fn reject(&mut self, id: PairingId, unix_secs: u64) -> Result<(), PairingRegistryError> {
        if self.prepare(unix_secs) {
            return Err(self.expire_target_after_clock_rollback(id, unix_secs));
        }
        if let Some(tombstone) = self.tombstones.get(&id) {
            return Err(PairingRegistryError::Pairing(tombstone.error));
        }
        let (deadline, result) = {
            let pairing = self
                .pending
                .get_mut(&id)
                .ok_or(PairingRegistryError::NotFound)?;
            (pairing.deadline(), pairing.reject())
        };
        match result {
            Ok(()) => {
                self.finish_terminal(id, PairingError::Rejected, deadline, unix_secs);
                Ok(())
            }
            Err(error) => {
                if is_terminal(error) {
                    self.finish_terminal(id, error, deadline, unix_secs);
                }
                Err(PairingRegistryError::Pairing(error))
            }
        }
    }

    fn prepare(&mut self, unix_secs: u64) -> bool {
        if self
            .last_checked_at
            .is_some_and(|last_checked_at| unix_secs < last_checked_at)
        {
            return true;
        }
        self.last_checked_at = Some(unix_secs);
        // Tombstones are needed only through the original ticket deadline. At and after that
        // deadline the old secret is invalid, so releasing the bounded record and permitting a
        // newly issued ticket to reuse the random id cannot revive the old pairing.
        self.tombstones
            .retain(|_, tombstone| unix_secs < tombstone.expires_at);
        self.pending
            .retain(|_, pairing| unix_secs < pairing.deadline());
        false
    }

    fn expire_target_after_clock_rollback(
        &mut self,
        id: PairingId,
        unix_secs: u64,
    ) -> PairingRegistryError {
        if let Some(tombstone) = self.tombstones.get(&id) {
            return PairingRegistryError::Pairing(tombstone.error);
        }
        let Some(deadline) = self.pending.get(&id).map(PendingPairing::deadline) else {
            return PairingRegistryError::ClockRollback;
        };
        self.finish_terminal(id, PairingError::Expired, deadline, unix_secs);
        PairingRegistryError::Pairing(PairingError::Expired)
    }

    fn finish_terminal(
        &mut self,
        id: PairingId,
        error: PairingError,
        expires_at: u64,
        unix_secs: u64,
    ) {
        self.pending.remove(&id);
        if unix_secs < expires_at {
            self.tombstones
                .insert(id, PairingTombstone { error, expires_at });
        }
    }
}

impl Default for PairingRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn is_terminal(error: PairingError) -> bool {
    matches!(
        error,
        PairingError::AttemptsExhausted
            | PairingError::Expired
            | PairingError::Rejected
            | PairingError::Consumed
    )
}

fn constant_time_secret_eq(
    expected: &[u8; PAIRING_SECRET_BYTES],
    candidate: &[u8; PAIRING_SECRET_BYTES],
) -> bool {
    bool::from(expected.ct_eq(candidate))
}

fn erase_secret_bytes(bytes: &mut [u8]) {
    for byte in bytes {
        // SAFETY: `byte` is exclusively borrowed from an owned secret buffer.
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay::contract::{ConnectionId, PairingId, RELAY_ID_BYTES};

    const ISSUED_AT: u64 = 1_800_000_000;
    const GOOD_SECRET_BYTE: u8 = b'Q';
    const CONNECTION: ConnectionId = ConnectionId::from_bytes([0x71; RELAY_ID_BYTES]);
    const PEER_IDENTITY_FINGERPRINT: [u8; 32] = [0x72; 32];
    const TRANSCRIPT_HASH: [u8; 32] = [0x73; 32];

    fn secret(byte: u8) -> PairingSecret {
        PairingSecret([byte; PAIRING_SECRET_BYTES])
    }

    fn pending() -> PendingPairing {
        PendingPairing::new(
            PairingId::from_bytes([0x41; RELAY_ID_BYTES]),
            secret(GOOD_SECRET_BYTE),
            ISSUED_AT,
        )
    }

    fn pairing_id(value: u16) -> PairingId {
        let mut bytes = [0u8; RELAY_ID_BYTES];
        bytes[..2].copy_from_slice(&value.to_be_bytes());
        PairingId::from_bytes(bytes)
    }

    fn consume_pending(
        pairing: &mut PendingPairing,
        unix_secs: u64,
    ) -> Result<PairingApproval, PairingError> {
        pairing.consume(unix_secs)
    }

    #[test]
    fn pairing_resource_limits_are_explicit() {
        assert_eq!(PAIRING_TTL_SECS, 5 * 60);
        assert_eq!(MAX_SECRET_ATTEMPTS, 5);
        assert_eq!(PAIRING_SECRET_BYTES, 32);
    }

    #[test]
    fn pairing_requires_verification_before_single_consume() {
        let mut pairing = pending();

        assert!(matches!(
            consume_pending(&mut pairing, ISSUED_AT),
            Err(PairingError::NotVerified)
        ));
        assert_eq!(
            pairing.verify_secret(&secret(GOOD_SECRET_BYTE), ISSUED_AT),
            Ok(())
        );
        let approval = consume_pending(&mut pairing, ISSUED_AT).unwrap();
        assert_eq!(approval.connection_id(), CONNECTION);
        assert_eq!(
            approval.peer_identity_fingerprint(),
            PEER_IDENTITY_FINGERPRINT
        );
        assert_eq!(approval.transcript_hash(), TRANSCRIPT_HASH);
        assert!(matches!(
            consume_pending(&mut pairing, ISSUED_AT),
            Err(PairingError::Consumed)
        ));
        assert_eq!(
            pairing.verify_secret(&secret(GOOD_SECRET_BYTE), ISSUED_AT),
            Err(PairingError::Consumed)
        );
        assert_eq!(pairing.reject(), Err(PairingError::Consumed));
    }

    #[test]
    fn verified_pairing_rejects_every_second_secret_verification() {
        let mut pairing = pending();
        let correct = secret(GOOD_SECRET_BYTE);
        let wrong = secret(b'X');

        assert_eq!(pairing.verify_secret(&correct, ISSUED_AT), Ok(()));
        assert_eq!(
            pairing.verify_secret(&correct, ISSUED_AT),
            Err(PairingError::AlreadyVerified)
        );
        assert_eq!(
            pairing.verify_secret(&wrong, ISSUED_AT),
            Err(PairingError::AlreadyVerified)
        );
        assert_eq!(wrong.0, [b'X'; PAIRING_SECRET_BYTES]);
    }

    #[test]
    fn pairing_accepts_only_before_the_five_minute_deadline() {
        let mut just_before = pending();
        assert_eq!(
            just_before.verify_secret(&secret(GOOD_SECRET_BYTE), ISSUED_AT + PAIRING_TTL_SECS - 1,),
            Ok(())
        );
        assert!(consume_pending(&mut just_before, ISSUED_AT + PAIRING_TTL_SECS - 1).is_ok());

        let mut at_deadline = pending();
        assert_eq!(
            at_deadline.verify_secret(&secret(GOOD_SECRET_BYTE), ISSUED_AT + PAIRING_TTL_SECS,),
            Err(PairingError::Expired)
        );
    }

    #[test]
    fn verified_pairing_still_expires_before_consume() {
        let mut pairing = pending();
        assert_eq!(
            pairing.verify_secret(&secret(GOOD_SECRET_BYTE), ISSUED_AT + PAIRING_TTL_SECS - 1,),
            Ok(())
        );
        assert!(matches!(
            consume_pending(&mut pairing, ISSUED_AT + PAIRING_TTL_SECS),
            Err(PairingError::Expired)
        ));
    }

    #[test]
    fn secret_verification_is_limited_to_five_attempts() {
        let mut pairing = pending();

        for attempt in 1..MAX_SECRET_ATTEMPTS {
            assert_eq!(
                pairing.verify_secret(&secret(attempt), ISSUED_AT),
                Err(PairingError::InvalidSecret),
                "attempt {attempt} must fail without exhausting the ticket"
            );
        }
        assert_eq!(
            pairing.verify_secret(&secret(MAX_SECRET_ATTEMPTS), ISSUED_AT),
            Err(PairingError::AttemptsExhausted)
        );
        assert_eq!(
            pairing.verify_secret(&secret(GOOD_SECRET_BYTE), ISSUED_AT),
            Err(PairingError::AttemptsExhausted)
        );
        assert!(matches!(
            consume_pending(&mut pairing, ISSUED_AT),
            Err(PairingError::AttemptsExhausted)
        ));
        assert_eq!(pairing.reject(), Err(PairingError::AttemptsExhausted));
    }

    #[test]
    fn rejected_pairing_cannot_be_revived() {
        let mut pairing = pending();

        assert_eq!(pairing.reject(), Ok(()));
        assert_eq!(
            pairing.verify_secret(&secret(GOOD_SECRET_BYTE), ISSUED_AT),
            Err(PairingError::Rejected)
        );
        assert!(matches!(
            consume_pending(&mut pairing, ISSUED_AT),
            Err(PairingError::Rejected)
        ));
        assert_eq!(pairing.reject(), Err(PairingError::Rejected));
    }

    #[test]
    fn expired_pairing_cannot_be_revived_by_clock_rollback() {
        let mut pairing = pending();

        assert_eq!(
            pairing.verify_secret(&secret(GOOD_SECRET_BYTE), ISSUED_AT + PAIRING_TTL_SECS,),
            Err(PairingError::Expired)
        );
        assert_eq!(
            pairing.verify_secret(&secret(GOOD_SECRET_BYTE), ISSUED_AT),
            Err(PairingError::Expired)
        );
        assert!(matches!(
            consume_pending(&mut pairing, ISSUED_AT),
            Err(PairingError::Expired)
        ));
        assert_eq!(pairing.reject(), Err(PairingError::Expired));
    }

    #[test]
    fn pairing_fails_closed_when_clock_precedes_issue_or_last_observation() {
        let mut before_issue = pending();
        assert_eq!(
            before_issue.verify_secret(&secret(GOOD_SECRET_BYTE), ISSUED_AT - 1),
            Err(PairingError::Expired)
        );
        assert_eq!(
            before_issue.verify_secret(&secret(GOOD_SECRET_BYTE), ISSUED_AT),
            Err(PairingError::Expired)
        );

        let mut rolls_back_while_pending = pending();
        assert_eq!(
            rolls_back_while_pending.verify_secret(&secret(b'X'), ISSUED_AT + 200),
            Err(PairingError::InvalidSecret)
        );
        assert_eq!(
            rolls_back_while_pending.verify_secret(&secret(GOOD_SECRET_BYTE), ISSUED_AT + 199),
            Err(PairingError::Expired)
        );
        assert!(matches!(
            consume_pending(&mut rolls_back_while_pending, ISSUED_AT + 201),
            Err(PairingError::Expired)
        ));
    }

    #[test]
    fn successful_verification_clears_stored_secret_but_not_borrowed_candidate() {
        let mut pairing = pending();
        let candidate = secret(GOOD_SECRET_BYTE);

        assert_eq!(pairing.verify_secret(&candidate, ISSUED_AT), Ok(()));
        assert_eq!(pairing.secret.0, [0; PAIRING_SECRET_BYTES]);
        assert_eq!(candidate.0, [GOOD_SECRET_BYTE; PAIRING_SECRET_BYTES]);
    }

    #[test]
    fn every_terminal_transition_clears_the_stored_secret() {
        let mut exhausted = pending();
        for byte in 0..MAX_SECRET_ATTEMPTS {
            let _ = exhausted.verify_secret(&secret(byte), ISSUED_AT);
        }
        assert_eq!(exhausted.secret.0, [0; PAIRING_SECRET_BYTES]);

        let mut expired = pending();
        assert_eq!(
            expired.verify_secret(&secret(GOOD_SECRET_BYTE), ISSUED_AT + PAIRING_TTL_SECS),
            Err(PairingError::Expired)
        );
        assert_eq!(expired.secret.0, [0; PAIRING_SECRET_BYTES]);

        let mut rejected = pending();
        assert_eq!(rejected.reject(), Ok(()));
        assert_eq!(rejected.secret.0, [0; PAIRING_SECRET_BYTES]);

        let mut consumed = pending();
        assert_eq!(
            consumed.verify_secret(&secret(GOOD_SECRET_BYTE), ISSUED_AT),
            Ok(())
        );
        assert!(consume_pending(&mut consumed, ISSUED_AT).is_ok());
        assert_eq!(consumed.secret.0, [0; PAIRING_SECRET_BYTES]);
    }

    #[test]
    fn pairing_secret_zeroizes_on_drop() {
        assert!(std::mem::needs_drop::<PairingSecret>());
    }

    #[test]
    fn production_pairing_secret_takes_and_erases_the_source_buffer() {
        let mut source = [GOOD_SECRET_BYTE; PAIRING_SECRET_BYTES];
        let owned = PairingSecret::take_from_bytes(&mut source);

        assert_eq!(source, [0; PAIRING_SECRET_BYTES]);
        assert_eq!(owned.0, [GOOD_SECRET_BYTE; PAIRING_SECRET_BYTES]);
        let production = include_str!("pairing.rs")
            .split("#[cfg(test)]\nmod tests")
            .next()
            .expect("production pairing source");
        assert!(!production.contains("pub const fn from_bytes"));
    }

    #[test]
    fn registry_owns_creation_and_tombstone_blocks_pairing_id_reconstruction() {
        let id = pairing_id(7);
        let mut registry = PairingRegistry::new();

        assert_eq!(
            registry.create_for_test(id, secret(GOOD_SECRET_BYTE), ISSUED_AT),
            Ok(())
        );
        assert_eq!(
            registry.create_for_test(id, secret(b'X'), ISSUED_AT),
            Err(PairingRegistryError::DuplicateId)
        );
        assert_eq!(
            registry.verify_secret(id, &secret(GOOD_SECRET_BYTE), ISSUED_AT),
            Ok(())
        );
        let approval = registry.consume(id, ISSUED_AT).unwrap();
        assert_eq!(approval.pairing_id(), id);
        assert_eq!(approval.connection_id(), CONNECTION);
        assert_eq!(
            approval.peer_identity_fingerprint(),
            PEER_IDENTITY_FINGERPRINT
        );
        assert_eq!(approval.transcript_hash(), TRANSCRIPT_HASH);
        assert!(format!("{approval:?}").contains("REDACTED"));
        assert_eq!(
            registry.create_for_test(id, secret(b'X'), ISSUED_AT + 1),
            Err(PairingRegistryError::DuplicateId)
        );
        assert_eq!(
            registry.create_for_test(id, secret(b'X'), ISSUED_AT + PAIRING_TTL_SECS),
            Ok(()),
            "the bounded tombstone may expire only after the original ticket deadline"
        );
    }

    #[test]
    fn verification_atomically_claims_the_exact_handshake_binding() {
        let id = pairing_id(12);
        let mut registry = PairingRegistry::new();
        registry
            .create_for_test(id, secret(GOOD_SECRET_BYTE), ISSUED_AT)
            .unwrap();
        let claimed = PairingBinding::new(CONNECTION, [0x81; 32], [0x82; 32]);

        assert_eq!(
            registry.verify_secret_for_binding(id, &secret(GOOD_SECRET_BYTE), ISSUED_AT, claimed,),
            Ok(())
        );
        let approval = registry.consume(id, ISSUED_AT).unwrap();
        assert_eq!(approval.binding(), claimed);
    }

    #[test]
    fn production_registry_issues_uncontrolled_id_and_secret_together() {
        let mut registry = PairingRegistry::new();
        let issued = registry.issue(ISSUED_AT).unwrap();
        let id = issued.id();
        let binding = PairingBinding::new(CONNECTION, [0x83; 32], [0x84; 32]);

        assert_eq!(
            registry.verify_secret_for_binding(id, issued.secret(), ISSUED_AT, binding),
            Ok(())
        );
        assert_eq!(registry.consume(id, ISSUED_AT).unwrap().binding(), binding);

        let production = include_str!("pairing.rs")
            .split("#[cfg(test)]\nmod tests")
            .next()
            .expect("production pairing source");
        assert!(!production.contains("pub fn create("));
    }

    #[test]
    fn registry_preserves_attempts_exhausted_as_a_tombstoned_terminal_state() {
        let id = pairing_id(8);
        let mut registry = PairingRegistry::new();
        registry
            .create_for_test(id, secret(GOOD_SECRET_BYTE), ISSUED_AT)
            .unwrap();

        for attempt in 1..MAX_SECRET_ATTEMPTS {
            assert_eq!(
                registry.verify_secret(id, &secret(attempt), ISSUED_AT),
                Err(PairingRegistryError::Pairing(PairingError::InvalidSecret))
            );
        }
        assert_eq!(
            registry.verify_secret(id, &secret(MAX_SECRET_ATTEMPTS), ISSUED_AT),
            Err(PairingRegistryError::Pairing(
                PairingError::AttemptsExhausted
            ))
        );
        assert_eq!(
            registry.verify_secret(id, &secret(GOOD_SECRET_BYTE), ISSUED_AT),
            Err(PairingRegistryError::Pairing(
                PairingError::AttemptsExhausted
            ))
        );
        assert_eq!(
            registry.create_for_test(id, secret(GOOD_SECRET_BYTE), ISSUED_AT),
            Err(PairingRegistryError::DuplicateId)
        );
    }

    #[test]
    fn registry_clock_rollback_terminally_expires_the_target_pairing() {
        let id = pairing_id(9);
        let mut registry = PairingRegistry::new();
        registry
            .create_for_test(id, secret(GOOD_SECRET_BYTE), ISSUED_AT)
            .unwrap();
        assert_eq!(
            registry.verify_secret(id, &secret(b'X'), ISSUED_AT + 200),
            Err(PairingRegistryError::Pairing(PairingError::InvalidSecret))
        );
        assert_eq!(
            registry.verify_secret(id, &secret(GOOD_SECRET_BYTE), ISSUED_AT + 199),
            Err(PairingRegistryError::Pairing(PairingError::Expired))
        );
        assert_eq!(
            registry.verify_secret(id, &secret(GOOD_SECRET_BYTE), ISSUED_AT + 201),
            Err(PairingRegistryError::Pairing(PairingError::Expired))
        );
    }

    #[test]
    fn registry_clock_rollback_rejects_new_ticket_creation() {
        let mut registry = PairingRegistry::new();
        registry
            .create_for_test(pairing_id(10), secret(GOOD_SECRET_BYTE), ISSUED_AT + 100)
            .unwrap();

        assert_eq!(
            registry.create_for_test(pairing_id(11), secret(b'X'), ISSUED_AT + 99),
            Err(PairingRegistryError::ClockRollback)
        );
        assert_eq!(
            registry.verify_secret(pairing_id(11), &secret(b'X'), ISSUED_AT + 101),
            Err(PairingRegistryError::NotFound)
        );
    }

    #[test]
    fn registry_bounds_active_and_tombstoned_pairing_records() {
        let mut registry = PairingRegistry::new();
        for value in 0..MAX_PAIRING_RECORDS as u16 {
            let id = pairing_id(value);
            let candidate = secret(value as u8);
            registry
                .create_for_test(id, secret(value as u8), ISSUED_AT)
                .unwrap();
            registry.verify_secret(id, &candidate, ISSUED_AT).unwrap();
            registry.consume(id, ISSUED_AT).unwrap();
        }
        assert_eq!(registry.pending.len(), 0);
        assert_eq!(registry.tombstones.len(), MAX_PAIRING_RECORDS);
        assert_eq!(
            registry.create_for_test(
                pairing_id(MAX_PAIRING_RECORDS as u16),
                secret(b'X'),
                ISSUED_AT
            ),
            Err(PairingRegistryError::CapacityExhausted)
        );
        assert_eq!(
            registry.create_for_test(
                pairing_id(MAX_PAIRING_RECORDS as u16),
                secret(b'X'),
                ISSUED_AT + PAIRING_TTL_SECS
            ),
            Ok(()),
            "expired records must release bounded registry capacity"
        );
    }

    #[test]
    fn pending_pairing_constructor_and_approval_are_not_publicly_reconstructible() {
        let production = include_str!("pairing.rs")
            .split("#[cfg(test)]\nmod tests")
            .next()
            .expect("production pairing source");

        assert!(!production.contains("pub const fn new(id: PairingId"));
        assert!(production.contains("pub struct PairingApproval {"));
        assert!(!production.contains("impl Clone for PairingApproval"));
        assert!(!production.contains("impl Copy for PairingApproval"));
        assert!(!production.contains("impl serde::Serialize for PairingApproval"));
    }

    #[test]
    fn pairing_secret_comparison_uses_constant_time_helper() {
        let production = include_str!("pairing.rs")
            .split("#[cfg(test)]\nmod tests")
            .next()
            .expect("production pairing source");

        assert!(production.contains("fn constant_time_secret_eq("));
        assert!(
            production.matches("constant_time_secret_eq(").count() >= 2,
            "verification must call the constant-time helper"
        );
        assert!(
            production.contains(".ct_eq("),
            "the helper must use a constant-time equality primitive"
        );
        assert!(!production.contains("SystemTime::now"));
    }

    #[test]
    fn pairing_debug_output_redacts_rendezvous_secret() {
        let pairing_secret = secret(GOOD_SECRET_BYTE);
        let secret_debug = format!("{pairing_secret:?}");
        assert!(!secret_debug.contains("QQQQQQQQ"));
        assert!(!secret_debug.contains("81, 81, 81"));

        let pairing_debug = format!("{:?}", pending());
        assert!(!pairing_debug.contains("QQQQQQQQ"));
        assert!(!pairing_debug.contains("81, 81, 81"));
    }
}
