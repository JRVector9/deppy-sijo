//! Storage-neutral persistence boundary for Relay devices and one-shot approvals.
//!
//! The application owns the concrete database and secret store. This module contains no SQLite
//! connection and never accepts terminal payloads, Tailscale credentials, or private keys as a
//! repository value.

use anyhow::Context as _;
use secret::{SecretStore, SecretString};
use sha2::{Digest as _, Sha256};
use zeroize::{Zeroize as _, Zeroizing};

use super::contract::{DeviceId, PairingId, RelayPermissions};
use super::crypto::{AuthenticatedHandshake, RelayIdentity, SecureChannel};
use super::pairing::{PAIRING_TTL_SECS, PairingApproval};

pub const MAX_RELAY_DEVICES: usize = 64;
pub const MAX_PENDING_RELAY_DEVICES: usize = 256;
pub const MAX_RELAY_DISPLAY_NAME_BYTES: usize = 128;
pub const RELAY_IDENTITY_SECRET_ID: &str = "deppy-relay-identity-p256-1";
const PRIVATE_SCALAR_BYTES: usize = 32;
const PRIVATE_SCALAR_HEX_BYTES: usize = PRIVATE_SCALAR_BYTES * 2;
const IDENTITY_GENERATION_ATTEMPTS: usize = 8;

/// Two independent lifetimes. `pairing_expires_at` is the five-minute one-shot ceremony deadline
/// after which approval must fail; `device_expires_at` is the separately selected authorization
/// window the admitted device receives once approval commits. Conflating them either stretches the
/// ceremony to the device window or kills the device at the ticket deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelayPairingLifetime {
    issued_at: u64,
    pairing_expires_at: u64,
    device_expires_at: u64,
}

impl RelayPairingLifetime {
    pub fn new(
        issued_at: u64,
        pairing_expires_at: u64,
        device_expires_at: u64,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            issued_at <= i64::MAX as u64
                && pairing_expires_at <= i64::MAX as u64
                && device_expires_at <= i64::MAX as u64,
            "Relay pairing lifetime is out of range"
        );
        anyhow::ensure!(
            pairing_expires_at > issued_at
                && pairing_expires_at - issued_at <= PAIRING_TTL_SECS
                && device_expires_at >= pairing_expires_at,
            "Relay pairing lifetime is invalid"
        );
        Ok(Self {
            issued_at,
            pairing_expires_at,
            device_expires_at,
        })
    }

    pub const fn issued_at(&self) -> u64 {
        self.issued_at
    }

    pub const fn pairing_expires_at(&self) -> u64 {
        self.pairing_expires_at
    }

    pub const fn device_expires_at(&self) -> u64 {
        self.device_expires_at
    }
}

/// What the Mac proposes to persist for one device. Grouping these keeps the SEC1 key, the name,
/// and the two timestamps from being passed positionally, where a swap would silently persist the
/// wrong lifetime or the wrong key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayDeviceProposal {
    pub device_id: DeviceId,
    pub identity_public_sec1: [u8; 65],
    pub display_name: String,
    pub permissions: RelayPermissions,
    pub lifetime: RelayPairingLifetime,
}

#[derive(Clone, PartialEq, Eq)]
pub struct PendingRelayDevice {
    pairing_id: PairingId,
    device_id: DeviceId,
    identity_public_sec1: [u8; 65],
    display_name: String,
    permissions: RelayPermissions,
    lifetime: RelayPairingLifetime,
}

impl PendingRelayDevice {
    /// The only production constructor. A persisted pending row may exist only for the peer whose
    /// public key the verified, non-cloneable pairing approval is bound to.
    pub fn from_pairing_approval(
        approval: &PairingApproval,
        proposal: RelayDeviceProposal,
    ) -> anyhow::Result<Self> {
        let fingerprint: [u8; 32] = Sha256::digest(proposal.identity_public_sec1).into();
        anyhow::ensure!(
            fingerprint == approval.peer_identity_fingerprint(),
            "Relay pairing peer identity does not match the persisted device"
        );
        Self::build(approval.pairing_id(), proposal)
    }

    #[cfg(test)]
    fn new_for_test(pairing_id: PairingId, proposal: RelayDeviceProposal) -> anyhow::Result<Self> {
        Self::build(pairing_id, proposal)
    }

    fn build(pairing_id: PairingId, proposal: RelayDeviceProposal) -> anyhow::Result<Self> {
        validate_public_identity(&proposal.identity_public_sec1, &proposal.display_name)?;
        Ok(Self {
            pairing_id,
            device_id: proposal.device_id,
            identity_public_sec1: proposal.identity_public_sec1,
            display_name: proposal.display_name,
            permissions: proposal.permissions,
            lifetime: proposal.lifetime,
        })
    }

    pub const fn pairing_id(&self) -> PairingId {
        self.pairing_id
    }

    pub const fn device_id(&self) -> DeviceId {
        self.device_id
    }

    pub const fn identity_public_sec1(&self) -> &[u8; 65] {
        &self.identity_public_sec1
    }

    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    pub const fn permissions(&self) -> RelayPermissions {
        self.permissions
    }

    pub const fn lifetime(&self) -> RelayPairingLifetime {
        self.lifetime
    }
}

impl std::fmt::Debug for PendingRelayDevice {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PendingRelayDevice")
            .field("pairing_id", &self.pairing_id)
            .field("device_id", &self.device_id)
            .field("display_name_bytes", &self.display_name.len())
            .field("lifetime", &self.lifetime)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct RelayDeviceRecord {
    device_id: DeviceId,
    identity_public_sec1: [u8; 65],
    display_name: String,
    permissions: RelayPermissions,
    issued_at: u64,
    device_expires_at: u64,
    last_seen_at: Option<u64>,
    revoked_at: Option<u64>,
}

impl RelayDeviceRecord {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        device_id: DeviceId,
        identity_public_sec1: [u8; 65],
        display_name: String,
        permissions: RelayPermissions,
        issued_at: u64,
        device_expires_at: u64,
        last_seen_at: Option<u64>,
        revoked_at: Option<u64>,
    ) -> anyhow::Result<Self> {
        validate_public_identity(&identity_public_sec1, &display_name)?;
        anyhow::ensure!(
            issued_at <= i64::MAX as u64
                && device_expires_at <= i64::MAX as u64
                && device_expires_at > issued_at,
            "Relay device lifetime is invalid"
        );
        for timestamp in [last_seen_at, revoked_at].into_iter().flatten() {
            anyhow::ensure!(
                timestamp >= issued_at && timestamp <= i64::MAX as u64,
                "Relay device timestamp is invalid"
            );
        }
        Ok(Self {
            device_id,
            identity_public_sec1,
            display_name,
            permissions,
            issued_at,
            device_expires_at,
            last_seen_at,
            revoked_at,
        })
    }

    pub const fn device_id(&self) -> DeviceId {
        self.device_id
    }

    pub const fn identity_public_sec1(&self) -> &[u8; 65] {
        &self.identity_public_sec1
    }

    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    pub const fn permissions(&self) -> RelayPermissions {
        self.permissions
    }

    pub const fn issued_at(&self) -> u64 {
        self.issued_at
    }

    pub const fn device_expires_at(&self) -> u64 {
        self.device_expires_at
    }

    pub const fn last_seen_at(&self) -> Option<u64> {
        self.last_seen_at
    }

    pub const fn revoked_at(&self) -> Option<u64> {
        self.revoked_at
    }

    pub fn is_admitted(&self, identity_public_sec1: &[u8; 65], unix_secs: u64) -> bool {
        self.revoked_at.is_none()
            && unix_secs >= self.issued_at
            && unix_secs < self.device_expires_at
            && self.identity_public_sec1 == *identity_public_sec1
    }
}

impl std::fmt::Debug for RelayDeviceRecord {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RelayDeviceRecord")
            .field("device_id", &self.device_id)
            .field("display_name_bytes", &self.display_name.len())
            .field("permissions", &self.permissions)
            .field("issued_at", &self.issued_at)
            .field("device_expires_at", &self.device_expires_at)
            .field("last_seen_at", &self.last_seen_at)
            .field("revoked_at", &self.revoked_at)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingInsert {
    Stored,
    PendingLimitReached,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ApprovalResult {
    Approved(RelayDeviceRecord),
    NotFound,
    Expired,
    DeviceLimitReached,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevocationResult {
    Revoked,
    NotFound,
}

pub trait RelayRepository: Send + Sync {
    fn insert_pending(
        &self,
        pending: PendingRelayDevice,
        trusted_now: u64,
    ) -> anyhow::Result<PendingInsert>;
    fn approve_pending(
        &self,
        pairing_id: PairingId,
        approved_at: u64,
    ) -> anyhow::Result<ApprovalResult>;
    fn pending_count(&self) -> anyhow::Result<usize>;
    fn device(&self, device_id: DeviceId) -> anyhow::Result<Option<RelayDeviceRecord>>;
    fn list_devices(&self, limit: usize) -> anyhow::Result<Vec<RelayDeviceRecord>>;
    fn revoke_device(
        &self,
        device_id: DeviceId,
        revoked_at: u64,
    ) -> anyhow::Result<RevocationResult>;
    fn touch_device(&self, device_id: DeviceId, seen_at: u64) -> anyhow::Result<bool>;
}

/// Why the pairing ceremony did not admit a device. Every variant is a deterministic decision, not
/// an error, and none of them leaves an admissible device behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionRejection {
    PendingLimitReached,
    PendingNotFound,
    PairingDeadlinePassed,
    DeviceLimitReached,
}

#[derive(Debug)]
pub enum AdmissionStart {
    Pending(Box<PendingAdmission>),
    Rejected(AdmissionRejection),
}

// `SecureChannel` carries live AES-256-GCM keys that zeroize on drop. Boxing the admitted variant
// to even the sizes would copy that key material onto the heap and leave the original stack bytes
// behind unzeroized, so the size difference is accepted here deliberately. The value is produced
// at most once per pairing ceremony.
#[allow(clippy::large_enum_variant)]
pub enum AdmissionOutcome {
    Admitted {
        channel: SecureChannel,
        device: RelayDeviceRecord,
    },
    Rejected(AdmissionRejection),
}

impl std::fmt::Debug for AdmissionOutcome {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Admitted { device, .. } => formatter
                .debug_struct("Admitted")
                .field("device", device)
                .finish(),
            Self::Rejected(rejection) => {
                formatter.debug_tuple("Rejected").field(rejection).finish()
            }
        }
    }
}

/// The single coordinator-owned ordering for one pairing ceremony:
///
/// 1. `PairingRegistry::consume` produces the verified, non-cloneable [`PairingApproval`].
/// 2. [`PendingAdmission::begin`] binds that approval to the peer public key and writes one
///    pending row.
/// 3. The user approves on the Mac.
/// 4. [`PendingAdmission::approve`] commits the database approval and only then consumes the
///    approval through `AuthenticatedHandshake::confirm`.
///
/// The approval lives in this value, never in the database, so a durable row alone is never proof
/// of pairing. A process restart drops every `PendingAdmission`, which is why the application
/// purges leftover pending rows when it opens the repository. If confirmation fails after the
/// approval commit, [`PendingAdmission::approve`] revokes the freshly published device before
/// returning the error, so a half-finished ceremony cannot leave an admissible device behind.
#[derive(Debug)]
pub struct PendingAdmission {
    approval: PairingApproval,
    pending: PendingRelayDevice,
}

impl PendingAdmission {
    pub fn begin(
        repository: &dyn RelayRepository,
        approval: PairingApproval,
        proposal: RelayDeviceProposal,
        trusted_now: u64,
    ) -> anyhow::Result<AdmissionStart> {
        let pending = PendingRelayDevice::from_pairing_approval(&approval, proposal)?;
        match repository.insert_pending(pending.clone(), trusted_now)? {
            PendingInsert::Stored => Ok(AdmissionStart::Pending(Box::new(Self {
                approval,
                pending,
            }))),
            PendingInsert::PendingLimitReached => Ok(AdmissionStart::Rejected(
                AdmissionRejection::PendingLimitReached,
            )),
        }
    }

    pub const fn pending(&self) -> &PendingRelayDevice {
        &self.pending
    }

    pub fn approve(
        self,
        repository: &dyn RelayRepository,
        handshake: AuthenticatedHandshake,
        approved_at: u64,
    ) -> anyhow::Result<AdmissionOutcome> {
        let device = match repository.approve_pending(self.pending.pairing_id(), approved_at)? {
            ApprovalResult::Approved(device) => device,
            ApprovalResult::NotFound => {
                return Ok(AdmissionOutcome::Rejected(
                    AdmissionRejection::PendingNotFound,
                ));
            }
            ApprovalResult::Expired => {
                return Ok(AdmissionOutcome::Rejected(
                    AdmissionRejection::PairingDeadlinePassed,
                ));
            }
            ApprovalResult::DeviceLimitReached => {
                return Ok(AdmissionOutcome::Rejected(
                    AdmissionRejection::DeviceLimitReached,
                ));
            }
        };
        if !device.is_admitted(self.pending.identity_public_sec1(), approved_at) {
            return Err(self.compensate(
                repository,
                device.device_id(),
                approved_at,
                anyhow::anyhow!("Relay approval published a device that is not admissible"),
            ));
        }
        match handshake.confirm(self.approval) {
            Ok(channel) => Ok(AdmissionOutcome::Admitted { channel, device }),
            Err(error) => Err(self.pending.compensate_confirmation_failure(
                repository,
                device.device_id(),
                approved_at,
                error,
            )),
        }
    }

    fn compensate(
        &self,
        repository: &dyn RelayRepository,
        device_id: DeviceId,
        revoked_at: u64,
        cause: anyhow::Error,
    ) -> anyhow::Error {
        self.pending
            .compensate_confirmation_failure(repository, device_id, revoked_at, cause)
    }
}

impl PendingRelayDevice {
    /// Fail-closed compensation for a failure after the approval commit. The published device is
    /// revoked so that no later connection can be admitted from the durable row alone. A failed
    /// revocation is folded into the returned error rather than swallowed.
    fn compensate_confirmation_failure(
        &self,
        repository: &dyn RelayRepository,
        device_id: DeviceId,
        revoked_at: u64,
        cause: anyhow::Error,
    ) -> anyhow::Error {
        match repository.revoke_device(device_id, revoked_at) {
            Ok(_) => cause.context("Relay admission failed after approval; device revoked"),
            Err(revocation_error) => cause.context(format!(
                "Relay admission failed after approval and the compensating revocation also \
                 failed: {revocation_error:#}"
            )),
        }
    }
}

pub fn get_or_create_relay_identity(store: &dyn SecretStore) -> anyhow::Result<RelayIdentity> {
    let _serial = RELAY_IDENTITY_SERIAL
        .lock()
        .map_err(|_| anyhow::anyhow!("Relay identity lock poisoned"))?;
    if store
        .has_secret(RELAY_IDENTITY_SECRET_ID)
        .context("Relay identity secret presence check failed")?
    {
        let encoded = store
            .get_secret(RELAY_IDENTITY_SECRET_ID)
            .context("Relay identity secret read failed")?;
        let mut scalar = decode_private_scalar(encoded.expose())?;
        return RelayIdentity::take_from_private_scalar(&mut scalar)
            .context("Relay identity secret is invalid");
    }

    for _ in 0..IDENTITY_GENERATION_ATTEMPTS {
        let mut scalar = Zeroizing::new([0u8; PRIVATE_SCALAR_BYTES]);
        getrandom::fill(&mut *scalar).context("Relay identity entropy unavailable")?;
        if p256::SecretKey::from_slice(&scalar[..]).is_err() {
            scalar.zeroize();
            continue;
        }
        let encoded = encode_private_scalar(&scalar);
        store
            .set_secret(RELAY_IDENTITY_SECRET_ID, &SecretString::new(encoded))
            .context("Relay identity secret write failed")?;
        return RelayIdentity::take_from_private_scalar(&mut scalar)
            .context("Relay identity generation failed");
    }
    anyhow::bail!("Relay identity generation attempts exhausted")
}

static RELAY_IDENTITY_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The one validation every persisted Relay row must pass before it becomes a record. The
/// application adapter calls this on raw database rows before any approval mutation, because a
/// storage engine can only check the length and `0x04` prefix, never that the value is a point on
/// the P-256 curve.
pub fn validate_public_identity(
    identity_public_sec1: &[u8; 65],
    display_name: &str,
) -> anyhow::Result<()> {
    p256::PublicKey::from_sec1_bytes(identity_public_sec1)
        .context("Relay device identity public key is invalid")?;
    anyhow::ensure!(
        !display_name.trim().is_empty()
            && display_name.len() <= MAX_RELAY_DISPLAY_NAME_BYTES
            && !display_name.contains('\0'),
        "Relay device display name is invalid"
    );
    Ok(())
}

fn encode_private_scalar(scalar: &[u8; PRIVATE_SCALAR_BYTES]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(PRIVATE_SCALAR_HEX_BYTES);
    for byte in scalar {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn decode_private_scalar(encoded: &str) -> anyhow::Result<[u8; PRIVATE_SCALAR_BYTES]> {
    anyhow::ensure!(
        encoded.len() == PRIVATE_SCALAR_HEX_BYTES,
        "Relay identity secret has invalid length"
    );
    let mut decoded = Zeroizing::new([0u8; PRIVATE_SCALAR_BYTES]);
    for (index, pair) in encoded.as_bytes().chunks_exact(2).enumerate() {
        decoded[index] = (decode_hex_nibble(pair[0])? << 4) | decode_hex_nibble(pair[1])?;
    }
    Ok(std::mem::take(&mut *decoded))
}

fn decode_hex_nibble(byte: u8) -> anyhow::Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => anyhow::bail!("Relay identity secret is not hexadecimal"),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex;

    use anyhow::Context as _;
    use p256::elliptic_curve::sec1::ToSec1Point as _;
    use secret::{SecretStore, SecretString};
    use sha2::{Digest as _, Sha256};

    use super::*;
    use crate::relay::contract::{ConnectionId, RelayAction};
    use crate::relay::crypto::{PendingHandshake, RELAY_PROTOCOL_VERSION, RelayRole};
    use crate::relay::pairing::{PairingBinding, PairingRegistry};

    const ISSUED_AT: u64 = 1_800_000_000;
    const PAIRING_EXPIRES_AT: u64 = ISSUED_AT + PAIRING_TTL_SECS;
    const DEVICE_EXPIRES_AT: u64 = ISSUED_AT + 86_400;

    fn lifetime() -> RelayPairingLifetime {
        RelayPairingLifetime::new(ISSUED_AT, PAIRING_EXPIRES_AT, DEVICE_EXPIRES_AT).unwrap()
    }

    /// Production text only — everything before the test module. Splitting on the first
    /// `#[cfg(test)]` would stop at the first test-only constructor and make every source law
    /// pass vacuously.
    fn production_source() -> &'static str {
        include_str!("repository.rs")
            .split("\n#[cfg(test)]\nmod tests {")
            .next()
            .unwrap()
    }

    fn opaque_id(value: u16) -> [u8; 16] {
        let mut bytes = [0u8; 16];
        bytes[..2].copy_from_slice(&value.to_be_bytes());
        bytes
    }

    fn pairing_id(value: u16) -> PairingId {
        PairingId::from_bytes(opaque_id(value))
    }

    fn device_id(value: u16) -> DeviceId {
        DeviceId::from_bytes(opaque_id(value))
    }

    fn public_key(value: u16) -> [u8; 65] {
        let mut scalar = [0u8; 32];
        scalar[30..].copy_from_slice(&value.saturating_add(1).to_be_bytes());
        let secret = p256::SecretKey::from_slice(&scalar).expect("valid test scalar");
        let encoded = secret.public_key().to_sec1_point(false);
        encoded.as_bytes().try_into().expect("uncompressed SEC1")
    }

    /// 65 bytes with the required `0x04` prefix that is nevertheless not on the P-256 curve —
    /// the shape a tampered database row takes when length checks alone are not enough.
    fn off_curve_public_key() -> [u8; 65] {
        let mut point = [0u8; 65];
        point[0] = 0x04;
        point[1..].fill(0x01);
        assert!(p256::PublicKey::from_sec1_bytes(&point).is_err());
        point
    }

    fn pending(value: u16) -> PendingRelayDevice {
        PendingRelayDevice::new_for_test(
            pairing_id(value),
            RelayDeviceProposal {
                device_id: device_id(value),
                identity_public_sec1: public_key(value),
                display_name: format!("phone-{value}"),
                permissions: RelayPermissions::default(),
                lifetime: lifetime(),
            },
        )
        .unwrap()
    }

    fn pairing_approval(identity_public_sec1: &[u8; 65]) -> PairingApproval {
        let fingerprint: [u8; 32] = Sha256::digest(identity_public_sec1).into();
        approval_for_binding(PairingBinding::new(
            ConnectionId::from_bytes([0x81; 16]),
            fingerprint,
            [0x82; 32],
        ))
    }

    fn approval_for_binding(binding: PairingBinding) -> PairingApproval {
        let mut registry = PairingRegistry::new();
        let issued = registry.issue(ISSUED_AT).unwrap();
        registry
            .verify_secret_for_binding(issued.id(), issued.secret(), ISSUED_AT, binding)
            .unwrap();
        registry.consume(issued.id(), ISSUED_AT).unwrap()
    }

    /// One real signed handshake pair. The desktop side is returned together with the device
    /// identity public key the approval must be bound to.
    fn desktop_handshake(connection: ConnectionId) -> (AuthenticatedHandshake, [u8; 65]) {
        let desktop_identity = RelayIdentity::generate().unwrap();
        let device_identity = RelayIdentity::generate().unwrap();
        let device_public = *device_identity.public_key_sec1();
        let desktop = PendingHandshake::begin(
            desktop_identity,
            device_public.to_vec(),
            RelayRole::Desktop,
            RELAY_PROTOCOL_VERSION,
            connection,
        )
        .unwrap();
        let device_peer_expectation = desktop.identity().public_key_sec1().to_vec();
        let device = PendingHandshake::begin(
            device_identity,
            device_peer_expectation,
            RelayRole::Device,
            RELAY_PROTOCOL_VERSION,
            connection,
        )
        .unwrap();
        let device_hello = device.sign_peer_offer(desktop.offer()).unwrap();
        (desktop.finish(device_hello).unwrap(), device_public)
    }

    #[derive(Default)]
    struct FakeRepository {
        state: Mutex<FakeState>,
    }

    #[derive(Default)]
    struct FakeState {
        pending: Vec<PendingRelayDevice>,
        devices: HashMap<[u8; 16], RelayDeviceRecord>,
        calls: Vec<&'static str>,
        fail_revocation: bool,
    }

    impl FakeRepository {
        fn calls(&self) -> Vec<&'static str> {
            self.state.lock().unwrap().calls.clone()
        }

        fn device_record(&self, device_id: DeviceId) -> Option<RelayDeviceRecord> {
            self.state
                .lock()
                .unwrap()
                .devices
                .get(device_id.as_bytes())
                .cloned()
        }
    }

    impl RelayRepository for FakeRepository {
        fn insert_pending(
            &self,
            pending: PendingRelayDevice,
            trusted_now: u64,
        ) -> anyhow::Result<PendingInsert> {
            anyhow::ensure!(
                pending.lifetime().issued_at() <= trusted_now
                    && trusted_now < pending.lifetime().pairing_expires_at(),
                "trusted clock outside the pairing window"
            );
            let mut state = self.state.lock().unwrap();
            state.calls.push("insert_pending");
            if state.pending.len() >= MAX_PENDING_RELAY_DEVICES {
                return Ok(PendingInsert::PendingLimitReached);
            }
            state.pending.push(pending);
            Ok(PendingInsert::Stored)
        }

        fn approve_pending(
            &self,
            pairing_id: PairingId,
            approved_at: u64,
        ) -> anyhow::Result<ApprovalResult> {
            let mut state = self.state.lock().unwrap();
            state.calls.push("approve_pending");
            let Some(index) = state
                .pending
                .iter()
                .position(|row| row.pairing_id() == pairing_id)
            else {
                return Ok(ApprovalResult::NotFound);
            };
            let row = state.pending.remove(index);
            if approved_at >= row.lifetime().pairing_expires_at() {
                return Ok(ApprovalResult::Expired);
            }
            if !state.devices.contains_key(row.device_id().as_bytes())
                && state.devices.len() >= MAX_RELAY_DEVICES
            {
                return Ok(ApprovalResult::DeviceLimitReached);
            }
            let device = RelayDeviceRecord::new(
                row.device_id(),
                *row.identity_public_sec1(),
                row.display_name().to_owned(),
                row.permissions(),
                approved_at,
                row.lifetime().device_expires_at(),
                None,
                None,
            )?;
            state
                .devices
                .insert(*row.device_id().as_bytes(), device.clone());
            Ok(ApprovalResult::Approved(device))
        }

        fn pending_count(&self) -> anyhow::Result<usize> {
            Ok(self.state.lock().unwrap().pending.len())
        }

        fn device(&self, device_id: DeviceId) -> anyhow::Result<Option<RelayDeviceRecord>> {
            Ok(self.device_record(device_id))
        }

        fn list_devices(&self, limit: usize) -> anyhow::Result<Vec<RelayDeviceRecord>> {
            let state = self.state.lock().unwrap();
            anyhow::ensure!(state.devices.len() <= limit, "limit exceeded");
            Ok(state.devices.values().cloned().collect())
        }

        fn revoke_device(
            &self,
            device_id: DeviceId,
            revoked_at: u64,
        ) -> anyhow::Result<RevocationResult> {
            let mut state = self.state.lock().unwrap();
            state.calls.push("revoke_device");
            anyhow::ensure!(!state.fail_revocation, "revocation unavailable");
            let Some(existing) = state.devices.get(device_id.as_bytes()).cloned() else {
                return Ok(RevocationResult::NotFound);
            };
            let revoked = RelayDeviceRecord::new(
                existing.device_id(),
                *existing.identity_public_sec1(),
                existing.display_name().to_owned(),
                existing.permissions(),
                existing.issued_at(),
                existing.device_expires_at(),
                existing.last_seen_at(),
                Some(existing.revoked_at().unwrap_or(revoked_at)),
            )?;
            state.devices.insert(*device_id.as_bytes(), revoked);
            Ok(RevocationResult::Revoked)
        }

        fn touch_device(&self, device_id: DeviceId, seen_at: u64) -> anyhow::Result<bool> {
            let mut state = self.state.lock().unwrap();
            let Some(existing) = state.devices.get(device_id.as_bytes()).cloned() else {
                return Ok(false);
            };
            if !existing.is_admitted(existing.identity_public_sec1(), seen_at) {
                return Ok(false);
            }
            let touched = RelayDeviceRecord::new(
                existing.device_id(),
                *existing.identity_public_sec1(),
                existing.display_name().to_owned(),
                existing.permissions(),
                existing.issued_at(),
                existing.device_expires_at(),
                Some(seen_at),
                existing.revoked_at(),
            )?;
            state.devices.insert(*device_id.as_bytes(), touched);
            Ok(true)
        }
    }

    #[derive(Default)]
    struct MemStore(Mutex<HashMap<String, String>>);

    impl SecretStore for MemStore {
        fn set_secret(&self, id: &str, secret: &SecretString) -> anyhow::Result<()> {
            self.0
                .lock()
                .unwrap()
                .insert(id.to_owned(), secret.expose().to_owned());
            Ok(())
        }

        fn get_secret(&self, id: &str) -> anyhow::Result<SecretString> {
            self.0
                .lock()
                .unwrap()
                .get(id)
                .cloned()
                .map(SecretString::new)
                .context("missing")
        }

        fn delete_secret(&self, id: &str) -> anyhow::Result<()> {
            self.0.lock().unwrap().remove(id);
            Ok(())
        }

        fn has_secret(&self, id: &str) -> anyhow::Result<bool> {
            Ok(self.0.lock().unwrap().contains_key(id))
        }
    }

    struct DeniedStore;

    impl SecretStore for DeniedStore {
        fn set_secret(&self, _: &str, _: &SecretString) -> anyhow::Result<()> {
            anyhow::bail!("denied")
        }

        fn get_secret(&self, _: &str) -> anyhow::Result<SecretString> {
            anyhow::bail!("denied")
        }

        fn delete_secret(&self, _: &str) -> anyhow::Result<()> {
            anyhow::bail!("denied")
        }

        fn has_secret(&self, _: &str) -> anyhow::Result<bool> {
            anyhow::bail!("denied")
        }
    }

    #[test]
    fn records_are_bounded_and_default_to_view_only() {
        assert_eq!(MAX_RELAY_DEVICES, 64);
        assert_eq!(MAX_PENDING_RELAY_DEVICES, 256);
        assert_eq!(MAX_RELAY_DISPLAY_NAME_BYTES, 128);

        let record = pending(1);
        assert!(record.permissions().allows(RelayAction::View));
        assert!(!record.permissions().allows(RelayAction::Input));
        assert!(
            PendingRelayDevice::new_for_test(
                pairing_id(2),
                RelayDeviceProposal {
                    device_id: device_id(2),
                    identity_public_sec1: public_key(2),
                    display_name: "x".repeat(MAX_RELAY_DISPLAY_NAME_BYTES + 1),
                    permissions: RelayPermissions::default(),
                    lifetime: lifetime(),
                },
            )
            .is_err()
        );
    }

    /// The five-minute ceremony deadline and the admitted-device window are two different
    /// lifetimes. One shared `expires_at` either stretches the ceremony or kills the device.
    #[test]
    fn pairing_deadline_and_device_expiry_are_independent_typed_fields() {
        let lifetime = lifetime();
        assert_eq!(lifetime.pairing_expires_at(), ISSUED_AT + PAIRING_TTL_SECS);
        assert_eq!(lifetime.device_expires_at(), DEVICE_EXPIRES_AT);
        assert_ne!(lifetime.pairing_expires_at(), lifetime.device_expires_at());

        assert!(
            RelayPairingLifetime::new(ISSUED_AT, ISSUED_AT + PAIRING_TTL_SECS, DEVICE_EXPIRES_AT)
                .is_ok()
        );
        assert!(
            RelayPairingLifetime::new(
                ISSUED_AT,
                ISSUED_AT + PAIRING_TTL_SECS + 1,
                DEVICE_EXPIRES_AT,
            )
            .is_err(),
            "the pairing window must never exceed the five-minute ticket"
        );
        assert!(
            RelayPairingLifetime::new(ISSUED_AT, ISSUED_AT, DEVICE_EXPIRES_AT).is_err(),
            "an empty pairing window is not a ceremony"
        );
        assert!(
            RelayPairingLifetime::new(
                ISSUED_AT,
                ISSUED_AT + PAIRING_TTL_SECS,
                ISSUED_AT + PAIRING_TTL_SECS - 1,
            )
            .is_err(),
            "a device must not expire before the pairing deadline"
        );
        assert!(
            RelayPairingLifetime::new(
                ISSUED_AT,
                ISSUED_AT + PAIRING_TTL_SECS,
                ISSUED_AT + PAIRING_TTL_SECS,
            )
            .is_ok(),
            "a device window equal to the pairing deadline is the shortest legal one"
        );

        let device = RelayDeviceRecord::new(
            device_id(1),
            public_key(1),
            "phone".to_owned(),
            RelayPermissions::default(),
            ISSUED_AT,
            DEVICE_EXPIRES_AT,
            None,
            None,
        )
        .unwrap();
        assert!(device.is_admitted(&public_key(1), ISSUED_AT + PAIRING_TTL_SECS + 1));
        assert!(device.is_admitted(&public_key(1), DEVICE_EXPIRES_AT - 1));
        assert!(!device.is_admitted(&public_key(1), DEVICE_EXPIRES_AT));
    }

    #[test]
    fn pending_record_requires_verified_pairing_peer_identity() {
        let approved_key = public_key(70);
        let approval = pairing_approval(&approved_key);
        assert!(
            PendingRelayDevice::from_pairing_approval(
                &approval,
                RelayDeviceProposal {
                    device_id: device_id(70),
                    identity_public_sec1: approved_key,
                    display_name: "approved phone".to_owned(),
                    permissions: RelayPermissions::default(),
                    lifetime: lifetime(),
                },
            )
            .is_ok()
        );
        assert!(
            PendingRelayDevice::from_pairing_approval(
                &approval,
                RelayDeviceProposal {
                    device_id: device_id(71),
                    identity_public_sec1: public_key(71),
                    display_name: "wrong phone".to_owned(),
                    permissions: RelayPermissions::default(),
                    lifetime: lifetime(),
                },
            )
            .is_err()
        );

        assert!(!production_source().contains("pub fn new(\n        pairing_id"));
    }

    /// A tampered row can carry an exact-length `0x04`-prefixed value that is not a curve point.
    /// Both record constructors reject it, so it can never reach admission or publication.
    #[test]
    fn off_curve_identity_keys_are_rejected_by_every_record_constructor() {
        let corrupt = off_curve_public_key();
        assert_eq!(corrupt.len(), 65);
        assert_eq!(corrupt[0], 0x04);

        assert!(
            RelayDeviceRecord::new(
                device_id(60),
                corrupt,
                "phone".to_owned(),
                RelayPermissions::default(),
                ISSUED_AT,
                DEVICE_EXPIRES_AT,
                None,
                None,
            )
            .is_err()
        );
        assert!(
            PendingRelayDevice::new_for_test(
                pairing_id(60),
                RelayDeviceProposal {
                    device_id: device_id(60),
                    identity_public_sec1: corrupt,
                    display_name: "phone".to_owned(),
                    permissions: RelayPermissions::default(),
                    lifetime: lifetime(),
                },
            )
            .is_err()
        );
        let approval = pairing_approval(&corrupt);
        assert!(
            PendingRelayDevice::from_pairing_approval(
                &approval,
                RelayDeviceProposal {
                    device_id: device_id(60),
                    identity_public_sec1: corrupt,
                    display_name: "phone".to_owned(),
                    permissions: RelayPermissions::default(),
                    lifetime: lifetime(),
                },
            )
            .is_err(),
            "a matching fingerprint over an invalid point is still an invalid point"
        );
    }

    #[test]
    fn admission_requires_the_verified_approval_not_a_durable_row() {
        let repository = FakeRepository::default();
        let connection = ConnectionId::from_bytes([0x33; 16]);
        let (handshake, device_public) = desktop_handshake(connection);
        let approval = approval_for_binding(handshake.pairing_binding());
        let AdmissionStart::Pending(admission) = PendingAdmission::begin(
            &repository,
            approval,
            RelayDeviceProposal {
                device_id: device_id(10),
                identity_public_sec1: device_public,
                display_name: "paired phone".to_owned(),
                permissions: RelayPermissions::default(),
                lifetime: lifetime(),
            },
            ISSUED_AT,
        )
        .unwrap() else {
            panic!("a fresh ceremony must start pending");
        };
        assert_eq!(repository.pending_count().unwrap(), 1);

        let outcome = admission
            .approve(&repository, handshake, ISSUED_AT + 1)
            .unwrap();
        let AdmissionOutcome::Admitted { channel, device } = outcome else {
            panic!("a verified ceremony must admit exactly one device");
        };
        drop(channel);
        assert_eq!(device.device_id(), device_id(10));
        assert_eq!(device.issued_at(), ISSUED_AT + 1);
        assert_eq!(device.device_expires_at(), DEVICE_EXPIRES_AT);
        assert_eq!(repository.pending_count().unwrap(), 0);
        assert_eq!(
            repository.calls(),
            vec!["insert_pending", "approve_pending"],
            "the database approval must precede channel confirmation and nothing else runs"
        );

        // A second connection with the same durable device row but no verified approval has no
        // way to reach `approve`: the coordinator consumed the only `PendingAdmission`.
        let (replay_handshake, _) = desktop_handshake(connection);
        let wrong_approval = pairing_approval(&device_public);
        assert!(
            replay_handshake.confirm(wrong_approval).is_err(),
            "a durable row must never substitute for the verified transcript binding"
        );
    }

    #[test]
    fn confirmation_failure_after_approval_revokes_the_published_device() {
        let repository = FakeRepository::default();
        let (handshake, device_public) = desktop_handshake(ConnectionId::from_bytes([0x34; 16]));
        // The approval is verified but bound to a different connection than the handshake, so the
        // database approval commits and `confirm` then fails.
        let mismatched = pairing_approval(&device_public);
        let AdmissionStart::Pending(admission) = PendingAdmission::begin(
            &repository,
            mismatched,
            RelayDeviceProposal {
                device_id: device_id(11),
                identity_public_sec1: device_public,
                display_name: "phone".to_owned(),
                permissions: RelayPermissions::default(),
                lifetime: lifetime(),
            },
            ISSUED_AT,
        )
        .unwrap() else {
            panic!("a fresh ceremony must start pending");
        };

        let error = admission
            .approve(&repository, handshake, ISSUED_AT + 1)
            .unwrap_err();
        assert!(format!("{error:#}").contains("device revoked"));
        assert_eq!(
            repository.calls(),
            vec!["insert_pending", "approve_pending", "revoke_device"],
            "compensation must run in the same ordering, after the commit"
        );
        let device = repository.device_record(device_id(11)).unwrap();
        assert_eq!(device.revoked_at(), Some(ISSUED_AT + 1));
        assert!(!device.is_admitted(&device_public, ISSUED_AT + 1));
    }

    #[test]
    fn a_failed_compensating_revocation_is_reported_not_swallowed() {
        let repository = FakeRepository::default();
        let (handshake, device_public) = desktop_handshake(ConnectionId::from_bytes([0x35; 16]));
        let mismatched = pairing_approval(&device_public);
        let AdmissionStart::Pending(admission) = PendingAdmission::begin(
            &repository,
            mismatched,
            RelayDeviceProposal {
                device_id: device_id(12),
                identity_public_sec1: device_public,
                display_name: "phone".to_owned(),
                permissions: RelayPermissions::default(),
                lifetime: lifetime(),
            },
            ISSUED_AT,
        )
        .unwrap() else {
            panic!("a fresh ceremony must start pending");
        };
        repository.state.lock().unwrap().fail_revocation = true;

        let error = admission
            .approve(&repository, handshake, ISSUED_AT + 1)
            .unwrap_err();
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("compensating revocation also failed"),
            "{rendered}"
        );
    }

    #[test]
    fn approval_after_the_pairing_deadline_admits_nothing() {
        let repository = FakeRepository::default();
        let (handshake, device_public) = desktop_handshake(ConnectionId::from_bytes([0x36; 16]));
        let approval = approval_for_binding(handshake.pairing_binding());
        let AdmissionStart::Pending(admission) = PendingAdmission::begin(
            &repository,
            approval,
            RelayDeviceProposal {
                device_id: device_id(13),
                identity_public_sec1: device_public,
                display_name: "phone".to_owned(),
                permissions: RelayPermissions::default(),
                lifetime: lifetime(),
            },
            ISSUED_AT,
        )
        .unwrap() else {
            panic!("a fresh ceremony must start pending");
        };

        let outcome = admission
            .approve(&repository, handshake, PAIRING_EXPIRES_AT)
            .unwrap();
        assert!(matches!(
            outcome,
            AdmissionOutcome::Rejected(AdmissionRejection::PairingDeadlinePassed)
        ));
        assert!(repository.device_record(device_id(13)).is_none());
    }

    #[test]
    fn a_full_pending_table_rejects_the_ceremony_before_any_approval() {
        let repository = FakeRepository::default();
        for value in 1..=MAX_PENDING_RELAY_DEVICES as u16 {
            assert_eq!(
                repository
                    .insert_pending(pending(value), ISSUED_AT)
                    .unwrap(),
                PendingInsert::Stored
            );
        }
        let (_, device_public) = desktop_handshake(ConnectionId::from_bytes([0x37; 16]));
        let approval = pairing_approval(&device_public);
        let start = PendingAdmission::begin(
            &repository,
            approval,
            RelayDeviceProposal {
                device_id: device_id(14),
                identity_public_sec1: device_public,
                display_name: "phone".to_owned(),
                permissions: RelayPermissions::default(),
                lifetime: lifetime(),
            },
            ISSUED_AT,
        )
        .unwrap();
        assert!(matches!(
            start,
            AdmissionStart::Rejected(AdmissionRejection::PendingLimitReached)
        ));
        assert_eq!(
            repository.pending_count().unwrap(),
            MAX_PENDING_RELAY_DEVICES
        );
    }

    #[test]
    fn relay_identity_uses_a_distinct_versioned_secret_and_survives_restart() {
        let store = MemStore::default();
        let first = get_or_create_relay_identity(&store).unwrap();
        let second = get_or_create_relay_identity(&store).unwrap();

        assert_eq!(first.public_key_sec1(), second.public_key_sec1());
        let entries = store.0.lock().unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries.contains_key(RELAY_IDENTITY_SECRET_ID));
        for reused in [
            "web-remote-token-1",
            "web-push-vapid-key-1",
            "audit-encryption-key-1",
        ] {
            assert_ne!(RELAY_IDENTITY_SECRET_ID, reused);
        }
    }

    #[test]
    fn concurrent_identity_creation_returns_one_persisted_identity() {
        struct SlowStore(MemStore);

        impl SecretStore for SlowStore {
            fn set_secret(&self, id: &str, secret: &SecretString) -> anyhow::Result<()> {
                std::thread::sleep(std::time::Duration::from_millis(30));
                self.0.set_secret(id, secret)
            }

            fn get_secret(&self, id: &str) -> anyhow::Result<SecretString> {
                self.0.get_secret(id)
            }

            fn delete_secret(&self, id: &str) -> anyhow::Result<()> {
                self.0.delete_secret(id)
            }

            fn has_secret(&self, id: &str) -> anyhow::Result<bool> {
                self.0.has_secret(id)
            }
        }

        let store = std::sync::Arc::new(SlowStore(MemStore::default()));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let mut threads = Vec::new();
        for _ in 0..8 {
            let store = store.clone();
            let barrier = barrier.clone();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                *get_or_create_relay_identity(store.as_ref())
                    .unwrap()
                    .public_key_sec1()
            }));
        }
        let identities = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();
        assert!(identities.iter().all(|identity| *identity == identities[0]));
        let persisted = get_or_create_relay_identity(store.as_ref()).unwrap();
        assert_eq!(persisted.public_key_sec1(), &identities[0]);
    }

    #[test]
    fn relay_identity_keychain_denial_is_a_relay_only_error() {
        let error = get_or_create_relay_identity(&DeniedStore).unwrap_err();
        assert!(format!("{error:#}").contains("Relay identity"));
    }

    #[test]
    fn corrupt_relay_identity_fails_closed_without_replacement() {
        let store = MemStore::default();
        store.0.lock().unwrap().insert(
            RELAY_IDENTITY_SECRET_ID.to_owned(),
            "not-a-valid-private-scalar".to_owned(),
        );

        assert!(get_or_create_relay_identity(&store).is_err());
        assert_eq!(
            store
                .0
                .lock()
                .unwrap()
                .get(RELAY_IDENTITY_SECRET_ID)
                .unwrap(),
            "not-a-valid-private-scalar"
        );
    }

    #[test]
    fn revocation_and_permission_downgrade_are_visible_to_admission_immediately() {
        let repository = FakeRepository::default();
        let granted = RelayPermissions::new(true, true, true).with_upload(true);
        let elevated = PendingRelayDevice::new_for_test(
            pairing_id(50),
            RelayDeviceProposal {
                device_id: device_id(50),
                identity_public_sec1: public_key(50),
                display_name: "phone".to_owned(),
                permissions: granted,
                lifetime: lifetime(),
            },
        )
        .unwrap();
        repository.insert_pending(elevated, ISSUED_AT).unwrap();
        let ApprovalResult::Approved(device) = repository
            .approve_pending(pairing_id(50), ISSUED_AT + 1)
            .unwrap()
        else {
            panic!("approval must publish the device");
        };
        for action in [
            RelayAction::View,
            RelayAction::Input,
            RelayAction::Upload,
            RelayAction::Approval,
        ] {
            assert!(device.permissions().allows(action));
        }

        // Re-approval with the first-release view-only grant downgrades in place.
        let downgraded = PendingRelayDevice::new_for_test(
            pairing_id(51),
            RelayDeviceProposal {
                device_id: device_id(50),
                identity_public_sec1: public_key(50),
                display_name: "phone".to_owned(),
                permissions: RelayPermissions::default(),
                lifetime: lifetime(),
            },
        )
        .unwrap();
        repository.insert_pending(downgraded, ISSUED_AT).unwrap();
        repository
            .approve_pending(pairing_id(51), ISSUED_AT + 2)
            .unwrap();
        let current = repository.device_record(device_id(50)).unwrap();
        assert!(current.permissions().allows(RelayAction::View));
        for action in [
            RelayAction::Input,
            RelayAction::Upload,
            RelayAction::Approval,
        ] {
            assert!(!current.permissions().allows(action));
        }

        assert_eq!(
            repository
                .revoke_device(device_id(50), ISSUED_AT + 3)
                .unwrap(),
            RevocationResult::Revoked
        );
        let revoked = repository.device_record(device_id(50)).unwrap();
        assert!(!revoked.is_admitted(&public_key(50), ISSUED_AT + 3));
        assert!(
            !repository
                .touch_device(device_id(50), ISSUED_AT + 4)
                .unwrap(),
            "a revoked device must not refresh its last-seen marker"
        );
    }

    /// The repository port stays storage-neutral: no SQLite handle, no secret material, and no
    /// terminal payload may appear in production code.
    #[test]
    fn the_repository_port_never_names_storage_or_secret_payloads() {
        let production = production_source();
        for forbidden in [
            "terminal_payload",
            "private_key",
            "tailscale_token",
            "rusqlite",
            "storage::",
            "PairingSecret",
        ] {
            assert!(!production.contains(forbidden), "{forbidden}");
        }

        let mut seen = HashSet::new();
        for method in [
            "insert_pending",
            "approve_pending",
            "pending_count",
            "device",
            "list_devices",
            "revoke_device",
            "touch_device",
        ] {
            assert!(seen.insert(method));
            assert!(production.contains(&format!("fn {method}(")), "{method}");
        }
    }
}
