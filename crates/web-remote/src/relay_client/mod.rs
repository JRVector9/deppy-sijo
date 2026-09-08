//! Mac 쪽 Relay 클라이언트.
//!
//! 기존 loopback 경로(`WebRemoteServer`, protocol-v3 `auth`, Tailscale Serve, Host 검증)에는
//! 손대지 않는다. Relay는 **바깥으로 나가는 연결만** 열며, 새 리스너를 만들지 않는다.
//!
//! 수명주기 판정([`lifecycle`])은 I/O에서 분리돼 있다 — 자격증명 회전·취소·인증 실패 뒤의
//! 동작을 실제 네트워크 없이 결정적으로 검증하기 위해서다.

pub mod adapter;
pub mod handshake;
pub mod lifecycle;
pub mod link;
pub mod session;
pub mod tls;
pub mod worker;

pub use adapter::{DenialReason, MAX_PERMISSION_VIOLATIONS, RelayAdmission, RelayMessageAdapter};
pub use handshake::{
    AuthenticatedPeer, HandshakeFailure, HandshakeStep, KnownDeviceClaim, PairingClaim, RelayClock,
    RelayHandshake, RelayIdentitySupplier,
};
pub use lifecycle::{
    BackoffPolicy, EndpointError, HaltReason, MAX_ENDPOINT_BYTES, PRODUCTION_RELAY_ADMISSION,
    PRODUCTION_RELAY_ENDPOINT, PRODUCTION_RELAY_ROUTE, RelayEndpoint, RelayLifecycle, RelayState,
};
pub use link::{
    MAX_PAIRING_FRAGMENT_CHARS, PAIRING_LINK_BYTES, PRODUCTION_RELAY_SHELL_ORIGIN,
    encode_pairing_link, shell_origin,
};
pub use session::{GateDrop, GateOutcome, RelaySessionGate};
pub use tls::{MAX_RELAY_FRAME_BYTES, TlsRelayTransport};
pub use worker::{
    MAX_PENDING_COMMANDS, RelayCommand, RelayDeadlines, RelayFrameSink, RelayObserver,
    RelaySession, RelayTransport, RelayWorker, SinkOutcome, TransportError,
};
