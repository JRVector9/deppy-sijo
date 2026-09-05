use aes_gcm::aead::{Aead as _, Payload};
use aes_gcm::{Aes256Gcm, KeyInit as _, Nonce};
use anyhow::Context as _;
use hkdf::Hkdf;
use p256::elliptic_curve::sec1::ToSec1Point as _;
use sha2::{Digest as _, Sha256};

use super::contract::ConnectionId;
use super::pairing::{PairingApproval, PairingBinding};
use super::repository::RelayDeviceRecord;

/// First independently authenticated Relay wire contract.
pub const RELAY_PROTOCOL_VERSION: u32 = 1;
/// One encrypted Relay record may retain at most one MiB of plaintext.
pub const MAX_RELAY_PLAINTEXT_BYTES: usize = 1024 * 1024;
/// WebCrypto AES-GCM appends a 128-bit authentication tag to the ciphertext.
pub const AES_GCM_TAG_BYTES: usize = 16;
pub const MAX_RELAY_CIPHERTEXT_BYTES: usize = MAX_RELAY_PLAINTEXT_BYTES + AES_GCM_TAG_BYTES;

const HANDSHAKE_DOMAIN: &[u8] = b"deppy-relay-handshake-v1\0";
const HKDF_SALT_DOMAIN: &[u8] = b"deppy-relay-hkdf-salt-v1\0";
const DESKTOP_TO_DEVICE_INFO: &[u8] = b"deppy-relay-desktop-to-device-v1\0";
const DEVICE_TO_DESKTOP_INFO: &[u8] = b"deppy-relay-device-to-desktop-v1\0";
const SAS_INFO: &[u8] = b"deppy-relay-sas-v1\0";
const ENVELOPE_AAD_DOMAIN: &[u8] = b"deppy-relay-envelope-aad-v1\0";
const DESKTOP_TO_DEVICE_NONCE_DOMAIN: [u8; 4] = *b"D2DV";
const DEVICE_TO_DESKTOP_NONCE_DOMAIN: [u8; 4] = *b"V2DS";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayRole {
    Desktop,
    Device,
}

impl RelayRole {
    fn code(self) -> u8 {
        match self {
            Self::Desktop => 1,
            Self::Device => 2,
        }
    }

    fn opposite(self) -> Self {
        match self {
            Self::Desktop => Self::Device,
            Self::Device => Self::Desktop,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayDirection {
    DesktopToDevice,
    DeviceToDesktop,
}

impl RelayDirection {
    fn code(self) -> u8 {
        match self {
            Self::DesktopToDevice => 1,
            Self::DeviceToDesktop => 2,
        }
    }

    fn nonce_domain(self) -> [u8; 4] {
        match self {
            Self::DesktopToDevice => DESKTOP_TO_DEVICE_NONCE_DOMAIN,
            Self::DeviceToDesktop => DEVICE_TO_DESKTOP_NONCE_DOMAIN,
        }
    }
}

/// Long-term P-256 ECDSA identity. It is deliberately neither `Clone` nor serializable.
pub struct RelayIdentity {
    signing: p256::ecdsa::SigningKey,
    public_sec1: [u8; 65],
    fingerprint: [u8; 32],
}

impl RelayIdentity {
    pub fn take_from_private_scalar(private_scalar: &mut [u8; 32]) -> anyhow::Result<Self> {
        let signing = signing_key_from_scalar_and_erase(private_scalar)?;
        let encoded = signing.verifying_key().to_sec1_point(false);
        let public_sec1 = sec1_array(encoded.as_bytes(), "relay identity public key")?;
        let fingerprint = sha256(&public_sec1);
        Ok(Self {
            signing,
            public_sec1,
            fingerprint,
        })
    }

    pub fn generate() -> anyhow::Result<Self> {
        let mut private_scalar = random_p256_scalar()?;
        Self::take_from_private_scalar(&mut private_scalar)
    }

    /// Test-only deterministic identity. Production callers must go through the keychain-backed
    /// `get_or_create_relay_identity`, which never lets a caller choose the scalar.
    #[cfg(test)]
    pub(crate) fn from_private_scalar(mut private_scalar: [u8; 32]) -> anyhow::Result<Self> {
        Self::take_from_private_scalar(&mut private_scalar)
    }

    pub fn public_key_sec1(&self) -> &[u8; 65] {
        &self.public_sec1
    }

    pub fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }
}

impl std::fmt::Debug for RelayIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RelayIdentity(REDACTED)")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayOffer {
    version: u32,
    role: RelayRole,
    connection_id: ConnectionId,
    identity_public_sec1: [u8; 65],
    ephemeral_public_sec1: [u8; 65],
}

impl RelayOffer {
    /// 와이어에서 받은 서명 없는 제시. 신원키·임시키는 곡선 위의 서로 다른 점이어야 한다 —
    /// 서명된 hello와 같은 검증을 받는다.
    pub fn from_webcrypto_parts(
        version: u32,
        role: RelayRole,
        connection_id: ConnectionId,
        identity_public_sec1: &[u8],
        ephemeral_public_sec1: &[u8],
    ) -> anyhow::Result<Self> {
        let identity_public_sec1 =
            validated_public_sec1(identity_public_sec1, "relay offer identity key")?;
        let ephemeral_public_sec1 =
            validated_public_sec1(ephemeral_public_sec1, "relay offer ephemeral key")?;
        ensure_distinct_handshake_keys(
            &identity_public_sec1,
            &ephemeral_public_sec1,
            "relay offer",
        )?;
        Ok(Self {
            version,
            role,
            connection_id,
            identity_public_sec1,
            ephemeral_public_sec1,
        })
    }

    pub fn version(&self) -> u32 {
        self.version
    }

    pub fn role(&self) -> RelayRole {
        self.role
    }

    pub fn connection_id(&self) -> ConnectionId {
        self.connection_id
    }

    pub fn identity_public_sec1(&self) -> &[u8; 65] {
        &self.identity_public_sec1
    }

    pub fn ephemeral_public_sec1(&self) -> &[u8; 65] {
        &self.ephemeral_public_sec1
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayHello {
    offer: RelayOffer,
    signature_raw: [u8; 64],
}

impl RelayHello {
    /// The offer half of this hello. The peer's signature is over the canonical transcript of
    /// both offers, so the local side needs this value to sign its own hello.
    pub fn offer(&self) -> &RelayOffer {
        &self.offer
    }

    pub fn from_webcrypto_parts(
        version: u32,
        role: RelayRole,
        connection_id: ConnectionId,
        identity_public_sec1: &[u8],
        ephemeral_public_sec1: &[u8],
        signature_raw: &[u8],
    ) -> anyhow::Result<Self> {
        let identity_public_sec1 =
            validated_public_sec1(identity_public_sec1, "relay hello identity key")?;
        let ephemeral_public_sec1 =
            validated_public_sec1(ephemeral_public_sec1, "relay hello ephemeral key")?;
        ensure_distinct_handshake_keys(
            &identity_public_sec1,
            &ephemeral_public_sec1,
            "relay hello",
        )?;
        anyhow::ensure!(
            signature_raw.len() == 64,
            "relay hello ECDSA signature must be raw 64-byte r||s"
        );
        p256::ecdsa::Signature::from_slice(signature_raw)
            .context("relay hello ECDSA signature is invalid")?;
        let signature_raw: [u8; 64] = signature_raw
            .try_into()
            .map_err(|_| anyhow::anyhow!("relay hello ECDSA signature length changed"))?;
        Ok(Self {
            offer: RelayOffer {
                version,
                role,
                connection_id,
                identity_public_sec1,
                ephemeral_public_sec1,
            },
            signature_raw,
        })
    }

    pub fn version(&self) -> u32 {
        self.offer.version()
    }

    pub fn role(&self) -> RelayRole {
        self.offer.role()
    }

    pub fn connection_id(&self) -> ConnectionId {
        self.offer.connection_id()
    }

    pub fn identity_public_sec1(&self) -> &[u8; 65] {
        self.offer.identity_public_sec1()
    }

    pub fn ephemeral_public_sec1(&self) -> &[u8; 65] {
        self.offer.ephemeral_public_sec1()
    }

    pub fn signature_raw(&self) -> &[u8; 64] {
        &self.signature_raw
    }
}

/// Owns the one-connection ECDH scalar and the distinct long-term ECDSA identity.
pub struct PendingHandshake {
    identity: RelayIdentity,
    expected_peer_identity_sec1: [u8; 65],
    ephemeral: p256::SecretKey,
    offer: RelayOffer,
}

impl PendingHandshake {
    pub fn begin(
        identity: RelayIdentity,
        expected_peer_identity_sec1: Vec<u8>,
        role: RelayRole,
        version: u32,
        connection_id: ConnectionId,
    ) -> anyhow::Result<Self> {
        Self::begin_with_ephemeral(
            identity,
            expected_peer_identity_sec1,
            role,
            version,
            connection_id,
            random_p256_scalar()?,
        )
    }

    fn begin_with_ephemeral(
        identity: RelayIdentity,
        expected_peer_identity_sec1: Vec<u8>,
        role: RelayRole,
        version: u32,
        connection_id: ConnectionId,
        mut ephemeral_scalar: [u8; 32],
    ) -> anyhow::Result<Self> {
        Self::begin_with_ephemeral_scalar(
            identity,
            expected_peer_identity_sec1,
            role,
            version,
            connection_id,
            &mut ephemeral_scalar,
        )
    }

    fn begin_with_ephemeral_scalar(
        identity: RelayIdentity,
        expected_peer_identity_sec1: Vec<u8>,
        role: RelayRole,
        version: u32,
        connection_id: ConnectionId,
        ephemeral_scalar: &mut [u8; 32],
    ) -> anyhow::Result<Self> {
        let ephemeral = secret_key_from_scalar_and_erase(ephemeral_scalar)?;
        anyhow::ensure!(
            version == RELAY_PROTOCOL_VERSION,
            "unsupported relay protocol version"
        );
        let expected_peer_identity_sec1 = validated_public_sec1(
            &expected_peer_identity_sec1,
            "expected relay peer identity key",
        )?;
        let encoded = ephemeral.public_key().to_sec1_point(false);
        let ephemeral_public_sec1 = sec1_array(encoded.as_bytes(), "relay ephemeral public key")?;
        ensure_distinct_handshake_keys(
            identity.public_key_sec1(),
            &ephemeral_public_sec1,
            "local relay offer",
        )?;
        let offer = RelayOffer {
            version,
            role,
            connection_id,
            identity_public_sec1: *identity.public_key_sec1(),
            ephemeral_public_sec1,
        };
        Ok(Self {
            identity,
            expected_peer_identity_sec1,
            ephemeral,
            offer,
        })
    }

    /// Test-only deterministic ephemeral. Production callers get a fresh OS-random scalar; a
    /// caller-chosen ephemeral would destroy forward secrecy, so this never leaves `cfg(test)`.
    #[cfg(test)]
    pub(crate) fn begin_with_ephemeral_for_test(
        identity: RelayIdentity,
        expected_peer_identity_sec1: Vec<u8>,
        role: RelayRole,
        version: u32,
        connection_id: ConnectionId,
        ephemeral_scalar: [u8; 32],
    ) -> anyhow::Result<Self> {
        Self::begin_with_ephemeral(
            identity,
            expected_peer_identity_sec1,
            role,
            version,
            connection_id,
            ephemeral_scalar,
        )
    }

    pub fn identity(&self) -> &RelayIdentity {
        &self.identity
    }

    pub fn offer(&self) -> &RelayOffer {
        &self.offer
    }

    pub fn sign_peer_offer(&self, peer_offer: &RelayOffer) -> anyhow::Result<RelayHello> {
        let transcript = self.validated_transcript(peer_offer)?;
        use p256::ecdsa::signature::Signer as _;
        let signature: p256::ecdsa::Signature = self.identity.signing.sign(&transcript);
        Ok(RelayHello {
            offer: self.offer.clone(),
            signature_raw: signature.to_bytes().into(),
        })
    }

    pub fn finish(self, peer_hello: RelayHello) -> anyhow::Result<AuthenticatedHandshake> {
        let transcript = self.validated_transcript(&peer_hello.offer)?;
        let verifying =
            p256::ecdsa::VerifyingKey::from_sec1_bytes(&self.expected_peer_identity_sec1)
                .context("expected relay peer identity key is invalid")?;
        let signature = p256::ecdsa::Signature::from_slice(&peer_hello.signature_raw)
            .context("relay peer signature is invalid")?;
        use p256::ecdsa::signature::Verifier as _;
        verifying
            .verify(&transcript, &signature)
            .context("relay signed transcript authentication failed")?;

        let peer_ephemeral =
            p256::PublicKey::from_sec1_bytes(peer_hello.offer.ephemeral_public_sec1())
                .context("relay peer ephemeral key is invalid")?;
        let shared = p256::ecdh::diffie_hellman(
            self.ephemeral.to_nonzero_scalar(),
            peer_ephemeral.as_affine(),
        );
        let material = derive_session_material(shared.raw_secret_bytes(), &transcript)?;
        let transcript_hash = sha256(&transcript);
        Ok(AuthenticatedHandshake {
            role: self.offer.role,
            version: self.offer.version,
            connection_id: self.offer.connection_id,
            own_identity_fingerprint: self.identity.fingerprint(),
            peer_identity_fingerprint: sha256(&self.expected_peer_identity_sec1),
            transcript_hash,
            desktop_to_device_key: material.desktop_to_device_key,
            device_to_desktop_key: material.device_to_desktop_key,
            confirmation_code: material.confirmation_code,
        })
    }

    fn validated_transcript(&self, peer_offer: &RelayOffer) -> anyhow::Result<Vec<u8>> {
        ensure_distinct_handshake_keys(
            self.offer.identity_public_sec1(),
            self.offer.ephemeral_public_sec1(),
            "local relay offer",
        )?;
        ensure_distinct_handshake_keys(
            peer_offer.identity_public_sec1(),
            peer_offer.ephemeral_public_sec1(),
            "peer relay offer",
        )?;
        anyhow::ensure!(
            peer_offer.version == self.offer.version,
            "relay protocol version mismatch"
        );
        anyhow::ensure!(
            peer_offer.role == self.offer.role.opposite(),
            "relay peer role mismatch"
        );
        anyhow::ensure!(
            peer_offer.connection_id == self.offer.connection_id,
            "relay connection id mismatch"
        );
        anyhow::ensure!(
            peer_offer.identity_public_sec1 == self.expected_peer_identity_sec1,
            "relay peer identity mismatch"
        );
        canonical_transcript(&self.offer, peer_offer)
    }
}

impl std::fmt::Debug for PendingHandshake {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PendingHandshake(REDACTED)")
    }
}

struct SessionMaterial {
    desktop_to_device_key: ChannelKey,
    device_to_desktop_key: ChannelKey,
    confirmation_code: String,
}

pub struct AuthenticatedHandshake {
    role: RelayRole,
    version: u32,
    connection_id: ConnectionId,
    own_identity_fingerprint: [u8; 32],
    peer_identity_fingerprint: [u8; 32],
    transcript_hash: [u8; 32],
    desktop_to_device_key: ChannelKey,
    device_to_desktop_key: ChannelKey,
    confirmation_code: String,
}

impl AuthenticatedHandshake {
    pub fn confirmation_code(&self) -> &str {
        &self.confirmation_code
    }

    pub fn pairing_binding(&self) -> PairingBinding {
        PairingBinding::new(
            self.connection_id,
            self.peer_identity_fingerprint,
            self.transcript_hash,
        )
    }

    pub fn confirm(self, approval: PairingApproval) -> anyhow::Result<SecureChannel> {
        anyhow::ensure!(
            self.role == RelayRole::Desktop,
            "only the desktop Relay role consumes local pairing approval"
        );
        anyhow::ensure!(
            approval.binding() == self.pairing_binding(),
            "pairing approval does not match the authenticated Relay transcript"
        );
        Ok(self.activate())
    }

    /// Activate the channel for a device that is already paired, without a pairing ticket.
    ///
    /// This is the revocation boundary. A stored row is never proof on its own: the record's
    /// identity key must hash to the fingerprint this handshake authenticated, and the record
    /// must still be admitted at `now` — not revoked, inside its authorization window. Both are
    /// re-checked here, on every reconnection, because a device that was admissible yesterday
    /// may have been revoked since.
    pub fn confirm_admitted(
        self,
        device: &RelayDeviceRecord,
        now: u64,
    ) -> anyhow::Result<SecureChannel> {
        anyhow::ensure!(
            self.role == RelayRole::Desktop,
            "only the desktop Relay role admits a stored device"
        );
        anyhow::ensure!(
            sha256(device.identity_public_sec1()) == self.peer_identity_fingerprint,
            "stored Relay device identity does not match the authenticated peer"
        );
        anyhow::ensure!(
            device.is_admitted(device.identity_public_sec1(), now),
            "stored Relay device is revoked or outside its authorization window"
        );
        Ok(self.activate())
    }

    #[cfg(test)]
    pub(crate) fn confirm_device_for_test(self) -> SecureChannel {
        assert_eq!(self.role, RelayRole::Device);
        self.activate()
    }

    fn activate(mut self) -> SecureChannel {
        let desktop_to_device_key = std::mem::take(&mut self.desktop_to_device_key);
        let device_to_desktop_key = std::mem::take(&mut self.device_to_desktop_key);
        let (send_key, receive_key) = match self.role {
            RelayRole::Desktop => (desktop_to_device_key, device_to_desktop_key),
            RelayRole::Device => (device_to_desktop_key, desktop_to_device_key),
        };
        SecureChannel {
            role: self.role,
            version: self.version,
            connection_id: self.connection_id,
            own_identity_fingerprint: self.own_identity_fingerprint,
            peer_identity_fingerprint: self.peer_identity_fingerprint,
            send_key,
            receive_key,
            send_sequence: 0,
            receive_sequence: 0,
            closed: false,
        }
    }
}

impl std::fmt::Debug for AuthenticatedHandshake {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AuthenticatedHandshake(REDACTED)")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvelopeHeader {
    pub version: u32,
    pub connection_id: ConnectionId,
    pub direction: RelayDirection,
    pub sequence: u64,
}

pub struct EncryptedEnvelope {
    header: EnvelopeHeader,
    ciphertext_and_tag: Vec<u8>,
}

impl EncryptedEnvelope {
    pub fn from_webcrypto_parts(
        header: EnvelopeHeader,
        ciphertext_and_tag: &[u8],
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            (AES_GCM_TAG_BYTES..=MAX_RELAY_CIPHERTEXT_BYTES).contains(&ciphertext_and_tag.len()),
            "relay ciphertext size is invalid"
        );
        Ok(Self {
            header,
            ciphertext_and_tag: ciphertext_and_tag.to_vec(),
        })
    }

    pub fn header(&self) -> &EnvelopeHeader {
        &self.header
    }

    pub fn ciphertext_and_tag(&self) -> &[u8] {
        &self.ciphertext_and_tag
    }
}

impl std::fmt::Debug for EncryptedEnvelope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncryptedEnvelope")
            .field("header", &self.header)
            .field("ciphertext_bytes", &self.ciphertext_and_tag.len())
            .finish()
    }
}

#[derive(Default)]
struct ChannelKey([u8; 32]);

impl ChannelKey {
    fn clear(&mut self) {
        erase(&mut self.0);
    }
}

impl Drop for ChannelKey {
    fn drop(&mut self) {
        self.clear();
    }
}

impl zeroize::ZeroizeOnDrop for ChannelKey {}

pub struct SecureChannel {
    role: RelayRole,
    version: u32,
    connection_id: ConnectionId,
    own_identity_fingerprint: [u8; 32],
    peer_identity_fingerprint: [u8; 32],
    send_key: ChannelKey,
    receive_key: ChannelKey,
    send_sequence: u64,
    receive_sequence: u64,
    closed: bool,
}

impl SecureChannel {
    /// The connection this channel was derived for. The session gate reads it from the channel
    /// itself rather than accepting it as a separate argument, so a channel can never be hung on
    /// a connection id other than the one its handshake was bound to.
    pub const fn connection_id(&self) -> ConnectionId {
        self.connection_id
    }

    pub fn seal(&mut self, plaintext: &[u8]) -> anyhow::Result<EncryptedEnvelope> {
        anyhow::ensure!(!self.closed, "relay channel is closed");
        anyhow::ensure!(
            plaintext.len() <= MAX_RELAY_PLAINTEXT_BYTES,
            "relay plaintext exceeds fixed limit"
        );
        anyhow::ensure!(
            self.send_sequence < u64::MAX,
            "relay send sequence is exhausted"
        );
        let sequence = self.send_sequence;
        let direction = self.send_direction();
        let header = EnvelopeHeader {
            version: self.version,
            connection_id: self.connection_id,
            direction,
            sequence,
        };
        let expected_ciphertext_len = plaintext
            .len()
            .checked_add(AES_GCM_TAG_BYTES)
            .context("relay ciphertext length overflow")?;
        let aad = envelope_aad(
            &header,
            &self.own_identity_fingerprint,
            &self.peer_identity_fingerprint,
            expected_ciphertext_len,
        )?;
        let nonce = envelope_nonce(direction, sequence);
        let cipher = Aes256Gcm::new_from_slice(&self.send_key.0)
            .map_err(|_| anyhow::anyhow!("relay AES-256 key length is invalid"))?;
        let ciphertext_and_tag = cipher
            .encrypt(
                &Nonce::from(nonce),
                Payload {
                    msg: plaintext,
                    aad: &aad,
                },
            )
            .map_err(|_| anyhow::anyhow!("relay AES-256-GCM encryption failed"))?;
        anyhow::ensure!(
            ciphertext_and_tag.len() == expected_ciphertext_len,
            "relay AES-GCM ciphertext length mismatch"
        );
        self.send_sequence = sequence + 1;
        Ok(EncryptedEnvelope {
            header,
            ciphertext_and_tag,
        })
    }

    pub fn open(&mut self, envelope: &EncryptedEnvelope) -> anyhow::Result<Vec<u8>> {
        anyhow::ensure!(!self.closed, "relay channel is closed");
        anyhow::ensure!(
            self.receive_sequence < u64::MAX,
            "relay receive sequence is exhausted"
        );
        anyhow::ensure!(
            envelope.header.sequence == self.receive_sequence,
            "relay receive sequence is not exactly monotonic"
        );
        anyhow::ensure!(
            envelope.header.version == self.version,
            "relay envelope version mismatch"
        );
        anyhow::ensure!(
            envelope.header.connection_id == self.connection_id,
            "relay envelope connection mismatch"
        );
        anyhow::ensure!(
            envelope.header.direction == self.receive_direction(),
            "relay envelope direction mismatch"
        );
        anyhow::ensure!(
            (AES_GCM_TAG_BYTES..=MAX_RELAY_CIPHERTEXT_BYTES)
                .contains(&envelope.ciphertext_and_tag.len()),
            "relay ciphertext size is invalid"
        );
        let aad = envelope_aad(
            &envelope.header,
            &self.peer_identity_fingerprint,
            &self.own_identity_fingerprint,
            envelope.ciphertext_and_tag.len(),
        )?;
        let nonce = envelope_nonce(envelope.header.direction, envelope.header.sequence);
        let cipher = Aes256Gcm::new_from_slice(&self.receive_key.0)
            .map_err(|_| anyhow::anyhow!("relay AES-256 key length is invalid"))?;
        let plaintext = cipher
            .decrypt(
                &Nonce::from(nonce),
                Payload {
                    msg: &envelope.ciphertext_and_tag,
                    aad: &aad,
                },
            )
            .map_err(|_| anyhow::anyhow!("relay AES-256-GCM authentication failed"))?;
        anyhow::ensure!(
            plaintext.len() <= MAX_RELAY_PLAINTEXT_BYTES,
            "relay decrypted plaintext exceeds fixed limit"
        );
        self.receive_sequence += 1;
        Ok(plaintext)
    }

    pub fn close(&mut self) {
        if !self.closed {
            self.closed = true;
            self.send_key.clear();
            self.receive_key.clear();
        }
    }

    #[cfg(test)]
    fn set_send_sequence_for_test(&mut self, sequence: u64) {
        self.send_sequence = sequence;
    }

    fn send_direction(&self) -> RelayDirection {
        match self.role {
            RelayRole::Desktop => RelayDirection::DesktopToDevice,
            RelayRole::Device => RelayDirection::DeviceToDesktop,
        }
    }

    fn receive_direction(&self) -> RelayDirection {
        match self.role {
            RelayRole::Desktop => RelayDirection::DeviceToDesktop,
            RelayRole::Device => RelayDirection::DesktopToDevice,
        }
    }
}

impl std::fmt::Debug for SecureChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecureChannel")
            .field("role", &self.role)
            .field("version", &self.version)
            .field("connection_id", &self.connection_id)
            .field("send_sequence", &self.send_sequence)
            .field("receive_sequence", &self.receive_sequence)
            .field("closed", &self.closed)
            .field("keys", &"REDACTED")
            .finish()
    }
}

impl Drop for SecureChannel {
    fn drop(&mut self) {
        self.close();
    }
}

fn canonical_transcript(own: &RelayOffer, peer: &RelayOffer) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(own.version == peer.version, "relay version mismatch");
    anyhow::ensure!(
        own.connection_id == peer.connection_id,
        "relay connection mismatch"
    );
    anyhow::ensure!(own.role != peer.role, "relay roles must be opposite");
    let (desktop, device) = match (own.role, peer.role) {
        (RelayRole::Desktop, RelayRole::Device) => (own, peer),
        (RelayRole::Device, RelayRole::Desktop) => (peer, own),
        _ => anyhow::bail!("relay roles must contain one desktop and one device"),
    };
    let mut transcript = Vec::with_capacity(HANDSHAKE_DOMAIN.len() + 4 + 16 + 2 + 65 * 4);
    transcript.extend_from_slice(HANDSHAKE_DOMAIN);
    transcript.extend_from_slice(&desktop.version.to_be_bytes());
    transcript.extend_from_slice(desktop.connection_id.as_bytes());
    transcript.push(desktop.role.code());
    transcript.extend_from_slice(&desktop.identity_public_sec1);
    transcript.extend_from_slice(&desktop.ephemeral_public_sec1);
    transcript.push(device.role.code());
    transcript.extend_from_slice(&device.identity_public_sec1);
    transcript.extend_from_slice(&device.ephemeral_public_sec1);
    Ok(transcript)
}

fn derive_session_material(
    shared_secret: &[u8],
    transcript: &[u8],
) -> anyhow::Result<SessionMaterial> {
    let mut salt_input = Vec::with_capacity(HKDF_SALT_DOMAIN.len() + transcript.len());
    salt_input.extend_from_slice(HKDF_SALT_DOMAIN);
    salt_input.extend_from_slice(transcript);
    let salt = sha256(&salt_input);
    let hkdf = Hkdf::<Sha256>::new(Some(&salt), shared_secret);
    let mut desktop_to_device_key = ChannelKey::default();
    hkdf.expand(DESKTOP_TO_DEVICE_INFO, &mut desktop_to_device_key.0)
        .map_err(|_| anyhow::anyhow!("relay desktop-to-device HKDF failed"))?;
    let mut device_to_desktop_key = ChannelKey::default();
    hkdf.expand(DEVICE_TO_DESKTOP_INFO, &mut device_to_desktop_key.0)
        .map_err(|_| anyhow::anyhow!("relay device-to-desktop HKDF failed"))?;
    let mut sas_bytes = [0u8; 4];
    hkdf.expand(SAS_INFO, &mut sas_bytes)
        .map_err(|_| anyhow::anyhow!("relay SAS HKDF failed"))?;
    let sas = u32::from_be_bytes(sas_bytes) % 1_000_000;
    Ok(SessionMaterial {
        desktop_to_device_key,
        device_to_desktop_key,
        confirmation_code: format!("{sas:06}"),
    })
}

fn envelope_aad(
    header: &EnvelopeHeader,
    sender_identity_fingerprint: &[u8; 32],
    recipient_identity_fingerprint: &[u8; 32],
    ciphertext_len: usize,
) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(
        ciphertext_len <= MAX_RELAY_CIPHERTEXT_BYTES,
        "relay AAD ciphertext length exceeds fixed limit"
    );
    let ciphertext_len = u32::try_from(ciphertext_len)
        .context("relay AAD ciphertext length does not fit the wire")?;
    let mut aad = Vec::with_capacity(ENVELOPE_AAD_DOMAIN.len() + 4 + 16 + 32 + 32 + 1 + 8 + 4);
    aad.extend_from_slice(ENVELOPE_AAD_DOMAIN);
    aad.extend_from_slice(&header.version.to_be_bytes());
    aad.extend_from_slice(header.connection_id.as_bytes());
    aad.extend_from_slice(sender_identity_fingerprint);
    aad.extend_from_slice(recipient_identity_fingerprint);
    aad.push(header.direction.code());
    aad.extend_from_slice(&header.sequence.to_be_bytes());
    aad.extend_from_slice(&ciphertext_len.to_be_bytes());
    Ok(aad)
}

fn envelope_nonce(direction: RelayDirection, sequence: u64) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[..4].copy_from_slice(&direction.nonce_domain());
    nonce[4..].copy_from_slice(&sequence.to_be_bytes());
    nonce
}

fn validated_public_sec1(bytes: &[u8], label: &str) -> anyhow::Result<[u8; 65]> {
    anyhow::ensure!(
        bytes.len() == 65 && bytes.first() == Some(&0x04),
        "{label} must use WebCrypto uncompressed 65-byte SEC1"
    );
    p256::PublicKey::from_sec1_bytes(bytes).with_context(|| format!("{label} is invalid"))?;
    sec1_array(bytes, label)
}

fn ensure_distinct_handshake_keys(
    identity_public_sec1: &[u8; 65],
    ephemeral_public_sec1: &[u8; 65],
    label: &str,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        identity_public_sec1 != ephemeral_public_sec1,
        "{label} identity and ephemeral keys must be distinct"
    );
    Ok(())
}

fn sec1_array(bytes: &[u8], label: &str) -> anyhow::Result<[u8; 65]> {
    bytes
        .try_into()
        .with_context(|| format!("{label} is not 65 bytes"))
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn signing_key_from_scalar_and_erase(
    private_scalar: &mut [u8; 32],
) -> anyhow::Result<p256::ecdsa::SigningKey> {
    let signing = p256::ecdsa::SigningKey::from_slice(private_scalar)
        .context("relay identity scalar is not a valid P-256 key");
    erase(private_scalar);
    signing
}

fn secret_key_from_scalar_and_erase(
    private_scalar: &mut [u8; 32],
) -> anyhow::Result<p256::SecretKey> {
    let secret = p256::SecretKey::from_slice(private_scalar)
        .context("relay ephemeral scalar is not a valid P-256 key");
    erase(private_scalar);
    secret
}

fn random_p256_scalar() -> anyhow::Result<[u8; 32]> {
    loop {
        let mut bytes = [0u8; 32];
        if let Err(error) = getrandom::fill(&mut bytes) {
            erase(&mut bytes);
            return Err(error).context("OS entropy unavailable for relay P-256 key");
        }
        if p256::SecretKey::from_slice(&bytes).is_ok() {
            return Ok(bytes);
        }
        erase(&mut bytes);
    }
}

fn erase(bytes: &mut [u8]) {
    for byte in bytes {
        // SAFETY: `byte` is an exclusively borrowed byte in an owned key buffer.
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay::contract::ConnectionId;

    const VERSION: u32 = 1;
    const CONNECTION_A: ConnectionId = ConnectionId::from_bytes(*b"connection-a-001");
    const CONNECTION_B: ConnectionId = ConnectionId::from_bytes(*b"connection-b-002");
    const DESKTOP_IDENTITY: [u8; 32] = [0x11; 32];
    const DEVICE_IDENTITY: [u8; 32] = [0x22; 32];
    const ROGUE_IDENTITY: [u8; 32] = [0x33; 32];
    const DESKTOP_EPHEMERAL: [u8; 32] = [0x44; 32];
    const DEVICE_EPHEMERAL: [u8; 32] = [0x55; 32];
    const OTHER_EPHEMERAL: [u8; 32] = [0x66; 32];

    struct SignedPair {
        desktop: PendingHandshake,
        device: PendingHandshake,
        desktop_hello: RelayHello,
        device_hello: RelayHello,
    }

    fn pending_pair(
        version: u32,
        connection_id: ConnectionId,
        desktop_role: RelayRole,
        device_role: RelayRole,
        desktop_ephemeral: [u8; 32],
        device_ephemeral: [u8; 32],
    ) -> (PendingHandshake, PendingHandshake) {
        pending_pair_with_identities(
            version,
            connection_id,
            desktop_role,
            device_role,
            DESKTOP_IDENTITY,
            DEVICE_IDENTITY,
            desktop_ephemeral,
            device_ephemeral,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn pending_pair_with_identities(
        version: u32,
        connection_id: ConnectionId,
        desktop_role: RelayRole,
        device_role: RelayRole,
        desktop_identity: [u8; 32],
        device_identity: [u8; 32],
        desktop_ephemeral: [u8; 32],
        device_ephemeral: [u8; 32],
    ) -> (PendingHandshake, PendingHandshake) {
        let desktop = RelayIdentity::from_private_scalar(desktop_identity).unwrap();
        let device = RelayIdentity::from_private_scalar(device_identity).unwrap();
        let desktop_public = desktop.public_key_sec1().to_vec();
        let device_public = device.public_key_sec1().to_vec();

        let desktop = PendingHandshake::begin_with_ephemeral_for_test(
            desktop,
            device_public,
            desktop_role,
            version,
            connection_id,
            desktop_ephemeral,
        )
        .unwrap();
        let device = PendingHandshake::begin_with_ephemeral_for_test(
            device,
            desktop_public,
            device_role,
            version,
            connection_id,
            device_ephemeral,
        )
        .unwrap();
        (desktop, device)
    }

    fn signed_pair_for(
        version: u32,
        connection_id: ConnectionId,
        desktop_ephemeral: [u8; 32],
        device_ephemeral: [u8; 32],
    ) -> SignedPair {
        signed_pair_for_identities(
            version,
            connection_id,
            DESKTOP_IDENTITY,
            DEVICE_IDENTITY,
            desktop_ephemeral,
            device_ephemeral,
        )
    }

    fn signed_pair_for_identities(
        version: u32,
        connection_id: ConnectionId,
        desktop_identity: [u8; 32],
        device_identity: [u8; 32],
        desktop_ephemeral: [u8; 32],
        device_ephemeral: [u8; 32],
    ) -> SignedPair {
        let (desktop, device) = pending_pair_with_identities(
            version,
            connection_id,
            RelayRole::Desktop,
            RelayRole::Device,
            desktop_identity,
            device_identity,
            desktop_ephemeral,
            device_ephemeral,
        );
        let desktop_hello = desktop.sign_peer_offer(device.offer()).unwrap();
        let device_hello = device.sign_peer_offer(desktop.offer()).unwrap();
        SignedPair {
            desktop,
            device,
            desktop_hello,
            device_hello,
        }
    }

    fn signed_pair() -> SignedPair {
        signed_pair_for(VERSION, CONNECTION_A, DESKTOP_EPHEMERAL, DEVICE_EPHEMERAL)
    }

    fn channels_for(
        version: u32,
        connection_id: ConnectionId,
        desktop_ephemeral: [u8; 32],
        device_ephemeral: [u8; 32],
    ) -> (SecureChannel, SecureChannel, String) {
        channels_for_identities(
            version,
            connection_id,
            DESKTOP_IDENTITY,
            DEVICE_IDENTITY,
            desktop_ephemeral,
            device_ephemeral,
        )
    }

    fn channels_for_identities(
        version: u32,
        connection_id: ConnectionId,
        desktop_identity: [u8; 32],
        device_identity: [u8; 32],
        desktop_ephemeral: [u8; 32],
        device_ephemeral: [u8; 32],
    ) -> (SecureChannel, SecureChannel, String) {
        let pair = signed_pair_for_identities(
            version,
            connection_id,
            desktop_identity,
            device_identity,
            desktop_ephemeral,
            device_ephemeral,
        );
        let desktop = pair.desktop.finish(pair.device_hello).unwrap();
        let device = pair.device.finish(pair.desktop_hello).unwrap();
        assert_eq!(desktop.confirmation_code(), device.confirmation_code());
        let code = desktop.confirmation_code().to_owned();
        let approval = approval_for(&desktop);
        (
            desktop.confirm(approval).unwrap(),
            device.confirm_device_for_test(),
            code,
        )
    }

    fn approval_for(handshake: &AuthenticatedHandshake) -> PairingApproval {
        use crate::relay::pairing::PairingRegistry;

        const APPROVAL_TIME: u64 = 1_800_000_000;
        let mut registry = PairingRegistry::new();
        let issued = registry.issue(APPROVAL_TIME).unwrap();
        registry
            .verify_secret_for_binding(
                issued.id(),
                issued.secret(),
                APPROVAL_TIME,
                handshake.pairing_binding(),
            )
            .unwrap();
        registry.consume(issued.id(), APPROVAL_TIME).unwrap()
    }

    fn channels() -> (SecureChannel, SecureChannel) {
        let (desktop, device, _) =
            channels_for(VERSION, CONNECTION_A, DESKTOP_EPHEMERAL, DEVICE_EPHEMERAL);
        (desktop, device)
    }

    fn hello_with(
        original: &RelayHello,
        version: u32,
        role: RelayRole,
        connection_id: ConnectionId,
        identity_public_sec1: Vec<u8>,
        ephemeral_public_sec1: Vec<u8>,
        signature_raw: Vec<u8>,
    ) -> RelayHello {
        RelayHello::from_webcrypto_parts(
            version,
            role,
            connection_id,
            &identity_public_sec1,
            &ephemeral_public_sec1,
            &signature_raw,
        )
        .unwrap_or_else(|error| {
            panic!("valid WebCrypto wire parts rejected: {error:#}; {original:?}")
        })
    }

    #[test]
    fn identity와_ephemeral_scalar입력은_성공과_오류에서_항상_erase된다() {
        let mut valid_identity = DESKTOP_IDENTITY;
        assert!(signing_key_from_scalar_and_erase(&mut valid_identity).is_ok());
        assert_eq!(valid_identity, [0; 32]);

        let mut invalid_identity = [0; 32];
        assert!(signing_key_from_scalar_and_erase(&mut invalid_identity).is_err());
        assert_eq!(invalid_identity, [0; 32]);

        let mut valid_ephemeral = DESKTOP_EPHEMERAL;
        assert!(secret_key_from_scalar_and_erase(&mut valid_ephemeral).is_ok());
        assert_eq!(valid_ephemeral, [0; 32]);

        let mut invalid_ephemeral = [0; 32];
        assert!(secret_key_from_scalar_and_erase(&mut invalid_ephemeral).is_err());
        assert_eq!(invalid_ephemeral, [0; 32]);
    }

    #[test]
    fn crypto_key_schedule_dependencies_zeroize_on_drop() {
        fn require_zeroize_on_drop<T: zeroize::ZeroizeOnDrop>() {}

        require_zeroize_on_drop::<aes::Aes256>();
        require_zeroize_on_drop::<Sha256>();

        // GHash/Hmac contain zeroizing Drop components but do not forward the
        // marker trait from those components through their public wrappers.
        assert!(std::mem::needs_drop::<ghash::GHash>());
        assert!(std::mem::needs_drop::<hmac::Hmac<Sha256>>());

        let manifest = include_str!("../../Cargo.toml");
        assert!(manifest.contains("ghash = { version = \"0.6\", features = [\"zeroize\"] }"));
        assert!(manifest.contains("hmac = { version = \"0.13\", features = [\"zeroize\"] }"));
        assert!(manifest.contains("sha2 = { workspace = true, features = [\"zeroize\"] }"));
    }

    #[test]
    fn prechannel_session_keys는_non_copy_zeroize_on_drop_wrapper로_소유된다() {
        fn require_zeroize_on_drop<T: zeroize::ZeroizeOnDrop>() {}
        fn require_channel_key(_: &ChannelKey) {}

        require_zeroize_on_drop::<ChannelKey>();
        assert!(std::mem::needs_drop::<ChannelKey>());

        let pair = signed_pair();
        let transcript = canonical_transcript(pair.desktop.offer(), pair.device.offer()).unwrap();
        let peer_ephemeral =
            p256::PublicKey::from_sec1_bytes(pair.device.offer().ephemeral_public_sec1()).unwrap();
        let shared = p256::ecdh::diffie_hellman(
            pair.desktop.ephemeral.to_nonzero_scalar(),
            peer_ephemeral.as_affine(),
        );
        let material = derive_session_material(shared.raw_secret_bytes(), &transcript).unwrap();
        require_channel_key(&material.desktop_to_device_key);
        require_channel_key(&material.device_to_desktop_key);

        let handshake = pair.desktop.finish(pair.device_hello).unwrap();
        require_channel_key(&handshake.desktop_to_device_key);
        require_channel_key(&handshake.device_to_desktop_key);
    }

    #[test]
    fn production_identity_constructor는_호출자_scalar_buffer까지_erase한다() {
        let mut source = DESKTOP_IDENTITY;
        let identity = RelayIdentity::take_from_private_scalar(&mut source).unwrap();

        assert_eq!(source, [0; 32]);
        assert_eq!(identity.public_key_sec1()[0], 0x04);
        let production = include_str!("crypto.rs")
            .split("#[cfg(test)]\nmod tests")
            .next()
            .expect("production crypto source");
        assert!(!production.contains("pub fn from_private_scalar"));
    }

    #[test]
    fn handshake의_선행_validation오류도_ephemeral_scalar를_erase한다() {
        let device = RelayIdentity::from_private_scalar(DEVICE_IDENTITY).unwrap();
        let mut unsupported_version_scalar = DESKTOP_EPHEMERAL;
        assert!(
            PendingHandshake::begin_with_ephemeral_scalar(
                RelayIdentity::from_private_scalar(DESKTOP_IDENTITY).unwrap(),
                device.public_key_sec1().to_vec(),
                RelayRole::Desktop,
                VERSION + 1,
                CONNECTION_A,
                &mut unsupported_version_scalar,
            )
            .is_err()
        );
        assert_eq!(unsupported_version_scalar, [0; 32]);

        let mut invalid_peer_scalar = DESKTOP_EPHEMERAL;
        assert!(
            PendingHandshake::begin_with_ephemeral_scalar(
                RelayIdentity::from_private_scalar(DESKTOP_IDENTITY).unwrap(),
                vec![0; 65],
                RelayRole::Desktop,
                VERSION,
                CONNECTION_A,
                &mut invalid_peer_scalar,
            )
            .is_err()
        );
        assert_eq!(invalid_peer_scalar, [0; 32]);
    }

    #[test]
    fn relay_identity와_ephemeral은_분리되고_private_debug은_redacted다() {
        let identity = RelayIdentity::from_private_scalar(DESKTOP_IDENTITY).unwrap();
        let identity_public = identity.public_key_sec1().to_vec();
        assert_eq!(identity_public.len(), 65);
        assert_eq!(
            identity_public[0], 0x04,
            "WebCrypto raw P-256 SEC1 encoding"
        );

        let device = RelayIdentity::from_private_scalar(DEVICE_IDENTITY).unwrap();
        let pending = PendingHandshake::begin_with_ephemeral_for_test(
            identity,
            device.public_key_sec1().to_vec(),
            RelayRole::Desktop,
            VERSION,
            CONNECTION_A,
            DESKTOP_EPHEMERAL,
        )
        .unwrap();
        assert_eq!(pending.offer().ephemeral_public_sec1().len(), 65);
        assert_eq!(pending.offer().ephemeral_public_sec1()[0], 0x04);
        assert_ne!(
            pending.offer().identity_public_sec1(),
            pending.offer().ephemeral_public_sec1(),
            "long-term ECDSA identity must not double as per-connection ECDH"
        );

        let identity_debug = format!("{:?}", pending.identity());
        let pending_debug = format!("{pending:?}");
        for debug in [&identity_debug, &pending_debug] {
            assert!(debug.contains("REDACTED"), "{debug}");
            assert!(!debug.contains("11111111"), "{debug}");
            assert!(!debug.contains("44444444"), "{debug}");
        }
    }

    #[test]
    fn local_offer는_identity와_ephemeral_key_reuse를_거부한다() {
        let identity = RelayIdentity::from_private_scalar(DESKTOP_IDENTITY).unwrap();
        let peer = RelayIdentity::from_private_scalar(DEVICE_IDENTITY).unwrap();

        let result = PendingHandshake::begin_with_ephemeral_for_test(
            identity,
            peer.public_key_sec1().to_vec(),
            RelayRole::Desktop,
            VERSION,
            CONNECTION_A,
            DESKTOP_IDENTITY,
        );

        assert!(
            result.is_err(),
            "long-term identity scalar must not be reused for local ephemeral ECDH"
        );
    }

    #[test]
    fn webcrypto_hello와_finish는_identity_ephemeral_key_reuse를_거부한다() {
        let desktop_identity = RelayIdentity::from_private_scalar(DESKTOP_IDENTITY).unwrap();
        let device_identity = RelayIdentity::from_private_scalar(DEVICE_IDENTITY).unwrap();
        let device_public = *device_identity.public_key_sec1();
        let desktop = PendingHandshake::begin_with_ephemeral_for_test(
            desktop_identity,
            device_public.to_vec(),
            RelayRole::Desktop,
            VERSION,
            CONNECTION_A,
            DESKTOP_EPHEMERAL,
        )
        .unwrap();
        let reused_offer = RelayOffer {
            version: VERSION,
            role: RelayRole::Device,
            connection_id: CONNECTION_A,
            identity_public_sec1: device_public,
            ephemeral_public_sec1: device_public,
        };
        let transcript = canonical_transcript(desktop.offer(), &reused_offer).unwrap();
        use p256::ecdsa::signature::Signer as _;
        let signature: p256::ecdsa::Signature = device_identity.signing.sign(&transcript);
        let signature_raw: [u8; 64] = signature.to_bytes().into();

        let wire_result = RelayHello::from_webcrypto_parts(
            VERSION,
            RelayRole::Device,
            CONNECTION_A,
            &device_public,
            &device_public,
            &signature_raw,
        );
        let finish_result = desktop.finish(RelayHello {
            offer: reused_offer,
            signature_raw,
        });

        assert!(
            wire_result.is_err() && finish_result.is_err(),
            "identity/ephemeral reuse accepted: from_webcrypto_parts={}, finish={}",
            wire_result.is_ok(),
            finish_result.is_ok()
        );
    }

    #[test]
    fn signed_hello는_webcrypto_raw규격이고_양쪽_sas는_같은_6자리다() {
        let pair = signed_pair();
        for hello in [&pair.desktop_hello, &pair.device_hello] {
            assert_eq!(hello.identity_public_sec1().len(), 65);
            assert_eq!(hello.identity_public_sec1()[0], 0x04);
            assert_eq!(hello.ephemeral_public_sec1().len(), 65);
            assert_eq!(hello.ephemeral_public_sec1()[0], 0x04);
            assert_eq!(
                hello.signature_raw().len(),
                64,
                "WebCrypto ECDSA P-256 signature must be raw r||s, not DER"
            );
        }

        let desktop = pair.desktop.finish(pair.device_hello).unwrap();
        let device = pair.device.finish(pair.desktop_hello).unwrap();
        assert_eq!(desktop.confirmation_code(), device.confirmation_code());
        assert_eq!(desktop.confirmation_code().len(), 6);
        assert!(
            desktop
                .confirmation_code()
                .bytes()
                .all(|byte| byte.is_ascii_digit())
        );
    }

    #[test]
    fn desktop_channel은_consumed_pairing_approval과_exact_transcript_binding을_요구한다() {
        let pair = signed_pair();
        let desktop = pair.desktop.finish(pair.device_hello).unwrap();
        let wrong_pair =
            signed_pair_for(VERSION, CONNECTION_B, DESKTOP_EPHEMERAL, DEVICE_EPHEMERAL);
        let wrong_desktop = wrong_pair.desktop.finish(wrong_pair.device_hello).unwrap();
        let wrong_approval = approval_for(&wrong_desktop);

        assert!(desktop.confirm(wrong_approval).is_err());

        let production = include_str!("crypto.rs")
            .split("#[cfg(test)]\nmod tests")
            .next()
            .expect("production crypto source");
        assert!(!production.contains("pub fn confirm(mut self) -> SecureChannel"));
        assert!(production.contains("pub fn confirm(self, approval: PairingApproval)"));
    }

    /// 이미 페어링된 기기는 티켓 없이 붙는다. 그 대신 **매 재접속마다** 저장된 레코드가
    /// 이 핸드셰이크의 상대와 같은 신원인지, 그리고 아직 인가돼 있는지를 다시 확인한다.
    #[test]
    fn admitted_device_channel은_저장된_신원과_취소_상태를_매번_다시_확인한다() {
        use crate::relay::contract::{DeviceId, RelayPermissions};
        use crate::relay::repository::RelayDeviceRecord;

        const NOW: u64 = 1_800_000_000;

        fn record(identity_public_sec1: [u8; 65], revoked_at: Option<u64>) -> RelayDeviceRecord {
            RelayDeviceRecord::new(
                DeviceId::from_bytes([0x51; 16]),
                identity_public_sec1,
                "phone".to_owned(),
                RelayPermissions::default(),
                NOW - 10,
                NOW + 1_000,
                None,
                revoked_at,
            )
            .unwrap()
        }

        let device_public = *RelayIdentity::from_private_scalar(DEVICE_IDENTITY)
            .unwrap()
            .public_key_sec1();
        let rogue_public = *RelayIdentity::from_private_scalar(ROGUE_IDENTITY)
            .unwrap()
            .public_key_sec1();

        // 다른 기기의 공개키가 든 레코드로는 열리지 않는다 — 지문이 어긋난다.
        let pair = signed_pair();
        let desktop = pair.desktop.finish(pair.device_hello).unwrap();
        assert!(
            desktop
                .confirm_admitted(&record(rogue_public, None), NOW)
                .is_err()
        );

        // 취소된 기기는 저장된 행이 남아 있어도 채널을 얻지 못한다.
        let pair = signed_pair();
        let desktop = pair.desktop.finish(pair.device_hello).unwrap();
        assert!(
            desktop
                .confirm_admitted(&record(device_public, Some(NOW)), NOW)
                .is_err()
        );

        // 인가 창을 벗어난 시각도 마찬가지다.
        let pair = signed_pair();
        let desktop = pair.desktop.finish(pair.device_hello).unwrap();
        assert!(
            desktop
                .confirm_admitted(&record(device_public, None), NOW + 1_000)
                .is_err()
        );

        // 기기 역할은 이 경로를 쓸 수 없다.
        let pair = signed_pair();
        let device = pair.device.finish(pair.desktop_hello).unwrap();
        assert!(
            device
                .confirm_admitted(&record(device_public, None), NOW)
                .is_err()
        );

        // 신원이 맞고 아직 인가돼 있으면 티켓 없이 열린다.
        let pair = signed_pair();
        let desktop = pair.desktop.finish(pair.device_hello).unwrap();
        let mut desktop = desktop
            .confirm_admitted(&record(device_public, None), NOW)
            .unwrap();
        let mut device = pair
            .device
            .finish(pair.desktop_hello)
            .unwrap()
            .confirm_device_for_test();
        let sealed = device.seal(b"already paired").unwrap();
        assert_eq!(desktop.open(&sealed).unwrap(), b"already paired");
        assert_eq!(desktop.connection_id(), CONNECTION_A);
    }

    #[test]
    fn clear_envelope_header는_stable_identity_fingerprint를_노출하지_않는다() {
        let production = include_str!("crypto.rs")
            .split("#[cfg(test)]\nmod tests")
            .next()
            .expect("production crypto source");
        let header = production
            .split("pub struct EnvelopeHeader {")
            .nth(1)
            .and_then(|source| source.split('}').next())
            .expect("EnvelopeHeader source");

        assert!(!header.contains("identity_fingerprint"), "{header}");
    }

    #[test]
    fn signed_transcript는_version_role_connection_identity_ephemeral_signature를_모두_검증한다() {
        fn rejected(mutator: impl FnOnce(&RelayHello) -> RelayHello) {
            let pair = signed_pair();
            let tampered = mutator(&pair.device_hello);
            assert!(pair.desktop.finish(tampered).is_err());
        }

        rejected(|hello| {
            hello_with(
                hello,
                hello.version() + 1,
                hello.role(),
                hello.connection_id(),
                hello.identity_public_sec1().to_vec(),
                hello.ephemeral_public_sec1().to_vec(),
                hello.signature_raw().to_vec(),
            )
        });
        rejected(|hello| {
            hello_with(
                hello,
                hello.version(),
                RelayRole::Desktop,
                hello.connection_id(),
                hello.identity_public_sec1().to_vec(),
                hello.ephemeral_public_sec1().to_vec(),
                hello.signature_raw().to_vec(),
            )
        });
        rejected(|hello| {
            hello_with(
                hello,
                hello.version(),
                hello.role(),
                CONNECTION_B,
                hello.identity_public_sec1().to_vec(),
                hello.ephemeral_public_sec1().to_vec(),
                hello.signature_raw().to_vec(),
            )
        });
        rejected(|hello| {
            let rogue = RelayIdentity::from_private_scalar(ROGUE_IDENTITY).unwrap();
            hello_with(
                hello,
                hello.version(),
                hello.role(),
                hello.connection_id(),
                rogue.public_key_sec1().to_vec(),
                hello.ephemeral_public_sec1().to_vec(),
                hello.signature_raw().to_vec(),
            )
        });
        rejected(|hello| {
            let (_, other) = pending_pair(
                VERSION,
                CONNECTION_A,
                RelayRole::Desktop,
                RelayRole::Device,
                DESKTOP_EPHEMERAL,
                OTHER_EPHEMERAL,
            );
            hello_with(
                hello,
                hello.version(),
                hello.role(),
                hello.connection_id(),
                hello.identity_public_sec1().to_vec(),
                other.offer().ephemeral_public_sec1().to_vec(),
                hello.signature_raw().to_vec(),
            )
        });
        rejected(|hello| {
            let mut signature = hello.signature_raw().to_vec();
            signature[0] ^= 0x80;
            hello_with(
                hello,
                hello.version(),
                hello.role(),
                hello.connection_id(),
                hello.identity_public_sec1().to_vec(),
                hello.ephemeral_public_sec1().to_vec(),
                signature,
            )
        });
    }

    #[test]
    fn directional_aes256gcm은_양방향_roundtrip하고_ciphertext에_plaintext가_없다() {
        let (mut desktop, mut device) = channels();
        let desktop_plaintext = b"terminal-secret-marker-741206";
        let to_device = desktop.seal(desktop_plaintext).unwrap();
        assert_eq!(
            to_device.header().direction,
            RelayDirection::DesktopToDevice
        );
        assert_eq!(to_device.header().sequence, 0);
        assert_eq!(
            to_device.ciphertext_and_tag().len(),
            desktop_plaintext.len() + AES_GCM_TAG_BYTES,
            "WebCrypto AES-GCM wire bytes are ciphertext followed by a 16-byte tag"
        );
        assert!(
            !to_device
                .ciphertext_and_tag()
                .windows(desktop_plaintext.len())
                .any(|window| window == desktop_plaintext)
        );
        assert_eq!(device.open(&to_device).unwrap(), desktop_plaintext);

        let device_plaintext = "한글 입력도 방향키가 다르다".as_bytes();
        let to_desktop = device.seal(device_plaintext).unwrap();
        assert_eq!(
            to_desktop.header().direction,
            RelayDirection::DeviceToDesktop
        );
        assert_eq!(desktop.open(&to_desktop).unwrap(), device_plaintext);
    }

    #[test]
    fn envelope_aad는_version_connection_peer_direction_sequence를_모두_인증한다() {
        let (mut desktop, mut device) = channels();
        let envelope = desktop.seal(b"authenticated envelope").unwrap();

        let mut tampered_headers = Vec::new();
        let mut version = envelope.header().clone();
        version.version += 1;
        tampered_headers.push(version);
        let mut connection = envelope.header().clone();
        connection.connection_id = CONNECTION_B;
        tampered_headers.push(connection);
        let mut direction = envelope.header().clone();
        direction.direction = RelayDirection::DeviceToDesktop;
        tampered_headers.push(direction);
        let mut sequence = envelope.header().clone();
        sequence.sequence += 1;
        tampered_headers.push(sequence);

        for header in tampered_headers {
            let tampered =
                EncryptedEnvelope::from_webcrypto_parts(header, envelope.ciphertext_and_tag())
                    .unwrap();
            assert!(device.open(&tampered).is_err());
        }
        let mut tampered_ciphertext = envelope.ciphertext_and_tag().to_vec();
        tampered_ciphertext[0] ^= 0x80;
        let tampered_ciphertext = EncryptedEnvelope::from_webcrypto_parts(
            envelope.header().clone(),
            &tampered_ciphertext,
        )
        .unwrap();
        assert!(device.open(&tampered_ciphertext).is_err());
        assert_eq!(
            device.open(&envelope).unwrap(),
            b"authenticated envelope",
            "failed AAD/tag checks must not advance the receive sequence"
        );

        let own_direction = desktop.seal(b"do not accept our own direction").unwrap();
        assert!(desktop.open(&own_direction).is_err());
    }

    #[test]
    fn receive_sequence는_정확히_단조증가하며_skip_duplicate_replay를_거부한다() {
        let (mut desktop, mut device) = channels();
        let first = desktop.seal(b"zero").unwrap();
        let second = desktop.seal(b"one").unwrap();
        assert_eq!(first.header().sequence, 0);
        assert_eq!(second.header().sequence, 1);

        assert!(device.open(&second).is_err(), "skipped sequence must fail");
        assert_eq!(device.open(&first).unwrap(), b"zero");
        assert!(device.open(&first).is_err(), "duplicate/replay must fail");
        assert_eq!(device.open(&second).unwrap(), b"one");
        assert!(
            device.open(&second).is_err(),
            "replay after advance must fail"
        );
    }

    #[test]
    fn hkdf는_connection과_peer를_묶어_다른_channel의_frame을_거부한다() {
        let (mut desktop_a, _, _) =
            channels_for(VERSION, CONNECTION_A, DESKTOP_EPHEMERAL, DEVICE_EPHEMERAL);
        let (_, mut device_b, _) =
            channels_for(VERSION, CONNECTION_B, DESKTOP_EPHEMERAL, DEVICE_EPHEMERAL);
        // Relabel only after key derivation so both sides authenticate identical B metadata while
        // retaining keys derived from different connection-bound transcripts.
        desktop_a.connection_id = CONNECTION_B;
        let envelope_a = desktop_a.seal(b"connection-bound").unwrap();
        let sealed_aad = envelope_aad(
            envelope_a.header(),
            &desktop_a.own_identity_fingerprint,
            &desktop_a.peer_identity_fingerprint,
            envelope_a.ciphertext_and_tag().len(),
        )
        .unwrap();
        let receiver_aad = envelope_aad(
            envelope_a.header(),
            &device_b.peer_identity_fingerprint,
            &device_b.own_identity_fingerprint,
            envelope_a.ciphertext_and_tag().len(),
        )
        .unwrap();
        assert_eq!(sealed_aad, receiver_aad, "test must isolate HKDF keys");
        assert!(device_b.open(&envelope_a).is_err());

        let (mut desktop_for_expected_device, _, _) = channels_for_identities(
            VERSION,
            CONNECTION_A,
            DESKTOP_IDENTITY,
            DEVICE_IDENTITY,
            DESKTOP_EPHEMERAL,
            DEVICE_EPHEMERAL,
        );
        let (_, mut rogue_device, _) = channels_for_identities(
            VERSION,
            CONNECTION_A,
            DESKTOP_IDENTITY,
            ROGUE_IDENTITY,
            DESKTOP_EPHEMERAL,
            DEVICE_EPHEMERAL,
        );
        // Relabel only the post-derivation AAD identity so both sides authenticate the same
        // metadata while their keys remain bound to different peer-identity transcripts.
        desktop_for_expected_device.peer_identity_fingerprint =
            rogue_device.own_identity_fingerprint;
        let rogue_labelled = desktop_for_expected_device
            .seal(b"peer-hkdf-bound")
            .unwrap();
        assert_eq!(
            rogue_labelled.header().connection_id,
            CONNECTION_A,
            "the clear routing header must be acceptable to both same-connection peers"
        );
        let sealed_aad = envelope_aad(
            rogue_labelled.header(),
            &desktop_for_expected_device.own_identity_fingerprint,
            &desktop_for_expected_device.peer_identity_fingerprint,
            rogue_labelled.ciphertext_and_tag().len(),
        )
        .unwrap();
        let receiver_aad = envelope_aad(
            rogue_labelled.header(),
            &rogue_device.peer_identity_fingerprint,
            &rogue_device.own_identity_fingerprint,
            rogue_labelled.ciphertext_and_tag().len(),
        )
        .unwrap();
        assert_eq!(sealed_aad, receiver_aad, "test must isolate HKDF keys");
        assert!(
            rogue_device.open(&rogue_labelled).is_err(),
            "matching clear header/AAD must still fail under a rogue peer-derived key"
        );
    }

    #[test]
    fn plaintext_ciphertext상한과_send_counter_wrap은_상태를_진전시키지_않는다() {
        let (mut desktop, mut device) = channels();
        let oversized_plaintext = vec![0u8; MAX_RELAY_PLAINTEXT_BYTES + 1];
        assert!(desktop.seal(&oversized_plaintext).is_err());
        let first = desktop.seal(b"still sequence zero").unwrap();
        assert_eq!(first.header().sequence, 0);
        assert_eq!(device.open(&first).unwrap(), b"still sequence zero");

        let oversized_ciphertext = vec![0u8; MAX_RELAY_CIPHERTEXT_BYTES + 1];
        assert!(
            EncryptedEnvelope::from_webcrypto_parts(first.header().clone(), &oversized_ciphertext)
                .is_err()
        );

        desktop.set_send_sequence_for_test(u64::MAX);
        assert!(desktop.seal(b"must not wrap").is_err());
    }

    #[test]
    fn close후에는_encrypt_decrypt를_모두_fail_closed한다() {
        let (mut desktop, mut device) = channels();
        let pending = desktop.seal(b"created before close").unwrap();
        desktop.close();
        assert!(desktop.seal(b"after close").is_err());
        device.close();
        assert!(device.open(&pending).is_err());
    }

    fn relay_webcrypto_fixture() -> serde_json::Value {
        use base64::Engine as _;

        fn hex(bytes: &[u8]) -> String {
            bytes.iter().map(|byte| format!("{byte:02x}")).collect()
        }

        fn private_jwk(
            private_scalar: &[u8; 32],
            public_sec1: &[u8; 65],
            key_operation: &str,
        ) -> serde_json::Value {
            let encode =
                |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
            serde_json::json!({
                "kty": "EC",
                "crv": "P-256",
                "x": encode(&public_sec1[1..33]),
                "y": encode(&public_sec1[33..65]),
                "d": encode(private_scalar),
                "ext": true,
                "key_ops": [key_operation]
            })
        }

        let pair = signed_pair();
        let transcript = canonical_transcript(pair.desktop.offer(), pair.device.offer()).unwrap();
        let device_ephemeral =
            p256::PublicKey::from_sec1_bytes(pair.device.offer().ephemeral_public_sec1()).unwrap();
        let shared = p256::ecdh::diffie_hellman(
            pair.desktop.ephemeral.to_nonzero_scalar(),
            device_ephemeral.as_affine(),
        );
        let material = derive_session_material(shared.raw_secret_bytes(), &transcript).unwrap();
        let mut salt_input = Vec::from(HKDF_SALT_DOMAIN);
        salt_input.extend_from_slice(&transcript);

        let desktop_identity_fingerprint = pair.desktop.identity.fingerprint();
        let device_identity_fingerprint = pair.device.identity.fingerprint();
        let desktop_identity_public = *pair.desktop.offer().identity_public_sec1();
        let device_identity_public = *pair.device.offer().identity_public_sec1();
        let desktop_ephemeral_public = *pair.desktop.offer().ephemeral_public_sec1();
        let device_ephemeral_public = *pair.device.offer().ephemeral_public_sec1();
        let desktop_signature = *pair.desktop_hello.signature_raw();
        let device_signature = *pair.device_hello.signature_raw();

        let desktop = pair.desktop.finish(pair.device_hello).unwrap();
        let device = pair.device.finish(pair.desktop_hello).unwrap();
        let approval = approval_for(&desktop);
        let mut desktop = desktop.confirm(approval).unwrap();
        let mut device = device.confirm_device_for_test();
        let desktop_plaintext = b"browser-decrypts-rust-d2d";
        let device_plaintext = "브라우저와 Rust V2D".as_bytes();
        let desktop_to_device = desktop.seal(desktop_plaintext).unwrap();
        let device_to_desktop = device.seal(device_plaintext).unwrap();
        let desktop_to_device_aad = envelope_aad(
            desktop_to_device.header(),
            &desktop_identity_fingerprint,
            &device_identity_fingerprint,
            desktop_to_device.ciphertext_and_tag().len(),
        )
        .unwrap();
        let device_to_desktop_aad = envelope_aad(
            device_to_desktop.header(),
            &device_identity_fingerprint,
            &desktop_identity_fingerprint,
            device_to_desktop.ciphertext_and_tag().len(),
        )
        .unwrap();

        serde_json::json!({
            "protocol_version": VERSION,
            "connection_id_hex": hex(CONNECTION_A.as_bytes()),
            "desktop": {
                "role": "desktop",
                "identity_public_sec1_hex": hex(&desktop_identity_public),
                "identity_private_jwk": private_jwk(
                    &DESKTOP_IDENTITY,
                    &desktop_identity_public,
                    "sign",
                ),
                "ephemeral_public_sec1_hex": hex(&desktop_ephemeral_public),
                "ephemeral_private_jwk": private_jwk(
                    &DESKTOP_EPHEMERAL,
                    &desktop_ephemeral_public,
                    "deriveBits",
                ),
                "signature_raw_hex": hex(&desktop_signature)
            },
            "device": {
                "role": "device",
                "identity_public_sec1_hex": hex(&device_identity_public),
                "identity_private_jwk": private_jwk(
                    &DEVICE_IDENTITY,
                    &device_identity_public,
                    "sign",
                ),
                "ephemeral_public_sec1_hex": hex(&device_ephemeral_public),
                "ephemeral_private_jwk": private_jwk(
                    &DEVICE_EPHEMERAL,
                    &device_ephemeral_public,
                    "deriveBits",
                ),
                "signature_raw_hex": hex(&device_signature)
            },
            "transcript_hex": hex(&transcript),
            "shared_secret_hex": hex(shared.raw_secret_bytes()),
            "hkdf_salt_hex": hex(&sha256(&salt_input)),
            "desktop_to_device_key_hex": hex(&material.desktop_to_device_key.0),
            "device_to_desktop_key_hex": hex(&material.device_to_desktop_key.0),
            "sas": material.confirmation_code,
            "desktop_to_device": {
                "direction": "desktop-to-device",
                "sequence": desktop_to_device.header().sequence,
                "nonce_hex": hex(&envelope_nonce(
                    RelayDirection::DesktopToDevice,
                    desktop_to_device.header().sequence,
                )),
                "aad_hex": hex(&desktop_to_device_aad),
                "plaintext_hex": hex(desktop_plaintext),
                "ciphertext_and_tag_hex": hex(desktop_to_device.ciphertext_and_tag())
            },
            "device_to_desktop": {
                "direction": "device-to-desktop",
                "sequence": device_to_desktop.header().sequence,
                "nonce_hex": hex(&envelope_nonce(
                    RelayDirection::DeviceToDesktop,
                    device_to_desktop.header().sequence,
                )),
                "aad_hex": hex(&device_to_desktop_aad),
                "plaintext_hex": hex(device_plaintext),
                "ciphertext_and_tag_hex": hex(device_to_desktop.ciphertext_and_tag())
            }
        })
    }

    #[test]
    fn checked_in_webcrypto_fixture_matches_the_rust_contract() {
        let checked_in: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/relay-webcrypto-v1.json"))
                .expect("checked-in Relay WebCrypto fixture must be valid JSON");

        assert_eq!(relay_webcrypto_fixture(), checked_in);
    }

    #[test]
    #[ignore = "fixture generation helper; the checked-in vector test is the release gate"]
    fn print_relay_webcrypto_fixture_for_review() {
        println!(
            "{}",
            serde_json::to_string_pretty(&relay_webcrypto_fixture()).unwrap()
        );
    }
}
