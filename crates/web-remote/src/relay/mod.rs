//! Deppy Relay security contract.
//!
//! This module is deliberately dormant until the independently authenticated outbound Relay
//! transport is wired. It does not reuse the loopback/Tailscale bearer or protocol-v3 socket.

pub mod contract;
pub mod crypto;
pub mod pairing;

pub use contract::{RelayAction, RelayPermissions};
pub use crypto::{EncryptedEnvelope, RelayIdentity, RelayRole, SecureChannel};
pub use pairing::{PairingError, PendingPairing};
