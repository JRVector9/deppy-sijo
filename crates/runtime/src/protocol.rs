//! Remote wire 프로토콜 v2 — 핸드셰이크 + 코덱 버전닝 (설계문서 §3, 단계 A).
//!
//! v1(미출시 스켈레톤)의 "첫 프레임 = 원시 토큰, 응답 `b"ok"`"를 대체한다:
//! 첫 프레임 = [`ClientHello`], 응답 = [`ServerHello`]. 매직/버전 prefix로 구버전·
//! 오접속 프레임을 hang 없이 조기 거부하고, feature 비트셋을 협상한다.
//!
//! **단계 A 범위 (§5 단계 A, §8 #1):** 협상만 한다. 서버는 아직 어떤 기능도
//! 광고하지 않으므로([`SERVER_FEATURES`] == 0) 협상 결과는 항상 비어 있고 코덱은
//! 항상 [`Codec::Plain`]이다. Plain 경로의 프레임은 기존 `postcard(RuntimeCommand)`/
//! `postcard(RuntimeEvent)`와 **바이트 동일**하다 — off-path 무변경이 A의 독립 배포 조건.
//! [`WireMsg`]/[`WireCmd`] 봉투와 [`Codec::Delta`]는 단계 B(delta)를 위한 정의일 뿐
//! 단계 A에서는 선택되지 않는다.

use std::sync::Arc;

use deppy_core::SessionId;
use terminal::{CursorSnapshot, TerminalCell, TerminalViewportSnapshot};

use crate::command::RuntimeCommand;
use crate::event::RuntimeEvent;

/// 첫 프레임을 v1 원시 토큰과 구분하고 오접속을 조기 거부하는 매직.
pub(crate) const PROTO_MAGIC: [u8; 4] = *b"DPRT";
/// 현재 프로토콜 버전.
pub(crate) const PROTO_VERSION: u16 = 2;

/// delta viewport 스트리밍 기능 비트 (§3.1). **단계 A는 정의만** — 협상되지 않는다.
pub(crate) const FEAT_DELTA_VIEWPORT: u32 = 1 << 0;

/// 서버가 실제로 지원(광고)하는 기능 집합. 단계 A는 delta 미구현이라 비어 있다 —
/// 클라이언트가 무엇을 요청하든 교집합은 0이므로 코덱은 항상 Plain으로 고정된다.
pub(crate) const SERVER_FEATURES: u32 = 0;

/// 클라이언트가 지원(요청)하는 기능 집합. 단계 A는 재구성 미구현이라 비어 있다.
pub(crate) const CLIENT_FEATURES: u32 = 0;

/// 클라이언트가 (TLS 수립 후) 보내는 첫 프레임. postcard. (§3.1)
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct ClientHello {
    pub magic: [u8; 4],
    pub proto_version: u16,
    /// 클라이언트가 지원하는 기능 비트마스크.
    pub features: u32,
    /// per-run 토큰 (TLS 안에서 안전) — 서버가 상수시간 비교.
    pub token: Vec<u8>,
}

/// 서버 응답. postcard. 인증 실패 시 서버는 이걸 보내지 않고 접속을 끊는다(v1 계약 동일). (§3.1)
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct ServerHello {
    pub proto_version: u16,
    /// features_ack = [`SERVER_FEATURES`] & client.features (교집합). 접속 코덱을 결정한다.
    pub features: u32,
}

/// delta 협상 접속의 이벤트 프레임 봉투 (§3.2). **단계 A는 정의만** — [`Codec::Delta`]가
/// 선택될 때만 쓰인다. viewport 변형은 단계 B에서 생성된다.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) enum WireMsg {
    /// 상태 이벤트(viewport 아님) — 기존 RuntimeEvent를 그대로 감싼다.
    Event(RuntimeEvent),
    /// 세션 viewport 전체 기준선. 단계 B에서 생성.
    #[allow(dead_code)] // 단계 B(delta)에서 생성. 단계 A는 봉투 정의만.
    ViewportKeyframe {
        session: SessionId,
        seq: u64,
        snapshot: Arc<TerminalViewportSnapshot>,
        bracketed_paste: bool,
    },
    /// 직전 전송본 대비 변경분. 단계 B에서 생성.
    #[allow(dead_code)] // 단계 B(delta)에서 생성. 단계 A는 봉투 정의만.
    ViewportDelta {
        session: SessionId,
        seq: u64,
        base_seq: u64,
        delta: ViewportDelta,
        bracketed_paste: bool,
    },
}

/// delta 협상 접속의 명령 프레임 봉투 (§4.4). **단계 A는 정의만.**
// large_enum_variant 예외: Command는 RuntimeCommand(SpawnAgent가 큼)를 §4.4 와이어
// 계약대로 그대로 감싸야 하고, RequestKeyframe(8B)와의 크기 차만으로 boxing을 강요하는
// 것은 봉투 정의(단계 A)에 불필요한 간접참조를 더한다. RuntimeCommand 자체가 lint를
// 넘지 않는 것과 같은 이유로 봉투도 원형을 유지한다.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) enum WireCmd {
    /// 기존 명령을 그대로 감싼다.
    Command(RuntimeCommand),
    /// 클라이언트→서버 keyframe 재동기화 요청. 단계 B에서 생성.
    #[allow(dead_code)] // 단계 B(delta)에서 생성. 단계 A는 봉투 정의만.
    RequestKeyframe { session: SessionId },
}

/// viewport 변경분 페이로드 (§4.7). **단계 A는 정의만** — diff/재구성 로직 없음.
#[allow(dead_code)] // 단계 B(delta)에서 생성/소비. 단계 A는 봉투 타입 정의만.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct ViewportDelta {
    pub cols: u16,
    pub rows: u16,
    pub cursor: CursorSnapshot,
    pub scroll_offset: i32,
    pub is_alt_screen: bool,
    pub title: Option<String>,
    /// 바뀐 row들 (각 row는 정확히 cols개 셀).
    pub changed_rows: Vec<RowPatch>,
}

/// 변경된 한 row (§4.7).
#[allow(dead_code)] // 단계 B(delta)에서 생성/소비. 단계 A는 봉투 타입 정의만.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct RowPatch {
    pub row: u16,
    pub cells: Vec<TerminalCell>,
}

/// 접속별 와이어 코덱 — 협상된 기능이 결정한다 (§3.2). 접속 단위로 확정되므로
/// pump/reader가 프레임을 인코딩·디코딩하는 방식이 하나로 고정된다.
#[derive(Clone, Copy)]
pub(crate) enum Codec {
    /// delta 미협상: RuntimeEvent/RuntimeCommand를 봉투 없이 직렬화 — **오늘과 동일 바이트**.
    Plain,
    /// delta 협상: WireMsg/WireCmd 봉투 사용. **단계 A에서는 선택되지 않는다**(SERVER_FEATURES==0).
    Delta,
}

impl Codec {
    /// 협상된 features_ack로 코덱을 고른다. 단계 A는 항상 Plain.
    pub(crate) fn from_features(features: u32) -> Self {
        if features & FEAT_DELTA_VIEWPORT != 0 {
            Codec::Delta
        } else {
            Codec::Plain
        }
    }

    /// 이벤트 프레임 payload 인코딩. Plain은 기존과 바이트 동일.
    pub(crate) fn encode_event(&self, event: &RuntimeEvent) -> anyhow::Result<Vec<u8>> {
        let payload = match self {
            Codec::Plain => postcard::to_allocvec(event)?,
            Codec::Delta => postcard::to_allocvec(&WireMsg::Event(event.clone()))?,
        };
        Ok(payload)
    }

    /// 이벤트 프레임 payload 디코딩 (클라이언트 수신).
    pub(crate) fn decode_event(&self, frame: &[u8]) -> anyhow::Result<RuntimeEvent> {
        match self {
            Codec::Plain => Ok(postcard::from_bytes(frame)?),
            Codec::Delta => match postcard::from_bytes::<WireMsg>(frame)? {
                WireMsg::Event(event) => Ok(event),
                // viewport keyframe/delta 재구성은 단계 B. 단계 A는 이 코덱 자체가 미선택.
                WireMsg::ViewportKeyframe { .. } | WireMsg::ViewportDelta { .. } => {
                    anyhow::bail!("viewport delta는 단계 B에서 지원")
                }
            },
        }
    }

    /// 명령 프레임 payload 인코딩 (클라이언트 송신).
    pub(crate) fn encode_command(&self, command: &RuntimeCommand) -> anyhow::Result<Vec<u8>> {
        let payload = match self {
            Codec::Plain => postcard::to_allocvec(command)?,
            Codec::Delta => postcard::to_allocvec(&WireCmd::Command(command.clone()))?,
        };
        Ok(payload)
    }

    /// 명령 프레임 payload 디코딩 (서버 수신).
    pub(crate) fn decode_command(&self, frame: &[u8]) -> anyhow::Result<RuntimeCommand> {
        match self {
            Codec::Plain => Ok(postcard::from_bytes(frame)?),
            Codec::Delta => match postcard::from_bytes::<WireCmd>(frame)? {
                WireCmd::Command(command) => Ok(command),
                WireCmd::RequestKeyframe { .. } => {
                    anyhow::bail!("RequestKeyframe은 단계 B에서 지원")
                }
            },
        }
    }
}
