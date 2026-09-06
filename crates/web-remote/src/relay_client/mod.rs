//! Mac 쪽 Relay 클라이언트.
//!
//! 기존 loopback 경로(`WebRemoteServer`, protocol-v3 `auth`, Tailscale Serve, Host 검증)에는
//! 손대지 않는다. Relay는 **바깥으로 나가는 연결만** 열며, 새 리스너를 만들지 않는다.
//!
//! 수명주기 판정([`lifecycle`])은 I/O에서 분리돼 있다 — 자격증명 회전·취소·인증 실패 뒤의
//! 동작을 실제 네트워크 없이 결정적으로 검증하기 위해서다.

pub mod adapter;
pub mod lifecycle;
pub mod tls;
pub mod worker;

pub use adapter::{DenialReason, MAX_PERMISSION_VIOLATIONS, RelayAdmission, RelayMessageAdapter};
pub use lifecycle::{
    BackoffPolicy, EndpointError, HaltReason, MAX_ENDPOINT_BYTES, PRODUCTION_RELAY_ENDPOINT,
    RelayEndpoint, RelayLifecycle, RelayState,
};
pub use tls::{MAX_RELAY_FRAME_BYTES, TlsRelayTransport};
pub use worker::{
    MAX_PENDING_COMMANDS, RelayCommand, RelayDeadlines, RelayFrameSink, RelayObserver,
    RelaySession, RelayTransport, RelayWorker, SinkOutcome, TransportError,
};
