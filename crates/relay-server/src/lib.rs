//! 신뢰하지 않는 Relay 데이터 평면 서버.
//!
//! 상태 기계([`core`])와 전송(`main.rs`)을 분리한다. 상태 기계에는 I/O가 전혀 없어서
//! 속도 제한·큐 상한·느린 소비자·시한 만료를 실제 시계나 소켓 없이 결정적으로 검증할 수
//! 있다. 남용 테스트가 전부 여기 붙는 이유다.

pub mod core;

pub use core::{
    AdmissionRefusal, ConnectionKey, RelayAction, RelayCore, RelayLimits, RelayStats, RouteVerifier,
};
