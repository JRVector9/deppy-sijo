//! Remote wire 프로토콜 v2 — 핸드셰이크 + 코덱 버전닝 + delta viewport (설계문서 §3~§4).
//!
//! v1(미출시 스켈레톤)의 "첫 프레임 = 원시 토큰, 응답 `b"ok"`"를 대체한다:
//! 첫 프레임 = [`ClientHello`], 응답 = [`ServerHello`]. 매직/버전 prefix로 구버전·
//! 오접속 프레임을 hang 없이 조기 거부하고, feature 비트셋을 협상한다.
//!
//! **단계 B (§4 delta viewport):** 서버/클라이언트 모두 [`FEAT_DELTA_VIEWPORT`]를
//! 광고하므로 loopback 접속은 [`Codec::Delta`]로 협상된다. Delta 접속은 viewport를
//! [`WireMsg::ViewportKeyframe`](전체 기준선)/[`WireMsg::ViewportDelta`](변경분)로,
//! 나머지 이벤트는 [`WireMsg::Event`] 봉투로 나른다. row 단위 content-diff([`diff_viewport`])와
//! 재구성([`apply_delta`])은 여기에 산다 — remote.rs의 2-스레드 transport 구조는 그대로다.
//!
//! **off-path 불변 (§3.2, §8 #1):** [`Codec::Plain`](delta 미협상)의 프레임은 여전히
//! 기존 `postcard(RuntimeCommand)`/`postcard(RuntimeEvent)`와 **바이트 동일**하다 —
//! Plain 경로 인코딩은 이 변경으로 바뀌지 않는다(가드 테스트로 못박음).

use std::sync::Arc;

use deppy_core::SessionId;
use terminal::{
    CellGrapheme, CellRange, CursorSnapshot, TerminalCell, TerminalViewportSnapshot,
    validate_cell_graphemes,
};

use crate::command::RuntimeCommand;
use crate::event::RuntimeEvent;

/// 첫 프레임을 v1 원시 토큰과 구분하고 오접속을 조기 거부하는 매직.
pub(crate) const PROTO_MAGIC: [u8; 4] = *b"DPRT";
/// 현재 프로토콜 버전. v3: SetSessionDefaultEnv 추가. v4: SetShellCwd 추가(끝에 append) —
/// 구버전 피어가 미지의 variant를 스트림 중간에서 만나 오해독하는 대신 handshake에서 거부.
/// **v6 (v3.7 I1)**: PaneSnapshot에 persistent_session_id 추가. enum variant append와 달리
/// **구조체 필드 추가는 기존 메시지(MuxUpdated)의 바이트를 바꾸므로** 버전을 올려야 한다 —
/// postcard 구조체는 태그 없는 순차 인코딩이다. hello가 정확 일치만 허용하므로 구버전
/// 피어는 조용한 오해독 대신 접속 단계에서 거부된다.
/// **v7 (T3)**: SearchScrollback 명령 + ScrollbackSearchResult 이벤트를 enum 끝에 append.
/// variant append만으로는 기존 바이트가 안 바뀌지만, 구버전 피어가 새 variant를 스트림에서
/// 만나면 오해독하므로 handshake 거부를 위해 버전을 올린다.
/// **v8 (B-1)**: TerminalCell에 attrs(SGR bold/italic/underline/strikeout/dim) 필드 추가 —
/// 구조체 필드 추가라 Viewport 바이트가 바뀐다(v6과 같은 이유로 필수 bump).
/// **v9 (셸 통합 1·2단계, 2026-07-17)**: ScrollToPrompt 명령 + ExtractLastOutput 명령/
/// LastOutputExtracted 이벤트를 enum 끝에 append — v7과 같은 이유(구버전 피어의 미지
/// variant 오해독 방지)로 handshake에서 거부되도록 버전을 올린다. 1단계(ScrollToPrompt)가
/// bump를 누락해 이 bump가 두 변경을 함께 커버한다.
/// **v10 (B00b)**: terminal AgentSpawnResolved correlation event를 enum 끝에 append.
/// 구버전 피어가 새 variant를 오해독하지 않도록 handshake에서 정확 버전을 거부한다.
/// **v11**: DurableEventBarrier 명령과 DurableEventBarrierReached 이벤트를 enum 끝에 append.
/// 로컬과 원격 runtime이 같은 durable FIFO acknowledgement 계약을 쓰도록 정확 버전을 올린다.
/// **v12**: InspectUnattachedSessions/KillUnattachedSessions 명령과 대응 결과 이벤트를 enum
/// 끝에 append. v11 피어가 미지 variant를 스트림에서 받기 전에 handshake에서 거부한다.
/// **v13**: SetScrollbackLimit/ScrollbackLimitApplied 추가. 기존 메시지 바이트는 유지하고
/// 새 variant를 이해하지 못하는 피어는 handshake에서 명확하게 거부한다.
/// **v14**: 실제 resize token/owner epoch/적용 stamp 및 tracked viewport를 끝에 append.
/// 구버전에는 실제 적용 보장을 흉내 내지 않고 기존 exact-version handshake로 거부한다.
/// v15: 워크스페이스 API 환경 연결을 기본 env 명령에 함께 전달한다.
/// v16: 프로젝트별 dotenv 선택 목록과 루트를 기본 환경에 포함한다.
/// v17: 기본환경 버전과 실제 프로세스 적용 ACK를 전달한다.
/// v18: operation-correlated PTY input admission; reject older peers before decode.
/// v19: admission-denied input result; older peers must reject this value before decode.
/// v20: sparse multi-scalar graphemes in viewport and row patches.
/// v21: accepted input submission timestamps; reject v20 before unknown event decode.
/// v22: atomic tracked input batches and possibly-partial admission results.
/// v23: atomic agent launch beside an exact pane; reject older peers before decode.
/// v24: direct terminal input mapped against live terminal modes on the worker.
pub(crate) const PROTO_VERSION: u16 = 24;

/// delta viewport 스트리밍 기능 비트 (§3.1).
pub(crate) const FEAT_DELTA_VIEWPORT: u32 = 1 << 0;

/// 서버가 실제로 지원(광고)하는 기능 집합. delta viewport를 광고한다 —
/// 클라이언트도 요청하면 교집합이 [`FEAT_DELTA_VIEWPORT`]가 되어 코덱이 Delta로 협상된다.
pub(crate) const SERVER_FEATURES: u32 = FEAT_DELTA_VIEWPORT;

/// 클라이언트가 지원(요청)하는 기능 집합. delta viewport 재구성을 지원한다.
pub(crate) const CLIENT_FEATURES: u32 = FEAT_DELTA_VIEWPORT;

/// heavy-repaint keyframe 폴백 임계 (§4.4-4, §4.7): 변경 row가 전체의 이 비율(%)
/// 이상이면 delta보다 keyframe이 싸므로 전체 스냅샷으로 폴백한다.
pub(crate) const HEAVY_REPAINT_PERCENT: usize = 60;

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

/// delta 협상 접속의 이벤트 프레임 봉투 (§3.2). [`Codec::Delta`]가 선택될 때만 쓰인다.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) enum WireMsg {
    /// 상태 이벤트(viewport 아님) — 기존 RuntimeEvent를 그대로 감싼다.
    Event(RuntimeEvent),
    /// 세션 viewport 전체 기준선. (재)구독·리사이즈·alt-screen·heavy repaint 시.
    ViewportKeyframe {
        session: SessionId,
        seq: u64,
        snapshot: Arc<TerminalViewportSnapshot>,
        bracketed_paste: bool,
    },
    /// 직전 전송본 대비 변경분.
    ViewportDelta {
        session: SessionId,
        seq: u64,
        base_seq: u64,
        delta: ViewportDelta,
        bracketed_paste: bool,
    },
    ViewportKeyframeTracked {
        session: SessionId,
        seq: u64,
        snapshot: Arc<TerminalViewportSnapshot>,
        bracketed_paste: bool,
        stamp: crate::ResizeStamp,
    },
    ViewportDeltaTracked {
        session: SessionId,
        seq: u64,
        base_seq: u64,
        delta: ViewportDelta,
        bracketed_paste: bool,
        stamp: crate::ResizeStamp,
    },
}

/// delta 협상 접속의 명령 프레임 봉투 (§4.4).
// large_enum_variant 예외: Command는 RuntimeCommand(SpawnAgent가 큼)를 §4.4 와이어
// 계약대로 그대로 감싸야 하고, RequestKeyframe(8B)와의 크기 차만으로 boxing을 강요하는
// 것은 불필요한 간접참조를 더한다. RuntimeCommand 자체가 lint를 넘지 않는 것과 같은
// 이유로 봉투도 원형을 유지한다.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) enum WireCmd {
    /// 기존 명령을 그대로 감싼다.
    Command(RuntimeCommand),
    /// 클라이언트→서버 keyframe 재동기화 요청 (§4.4). 재구성 seq gap 방어.
    RequestKeyframe { session: SessionId },
}

/// viewport 변경분 페이로드 (§4.7).
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
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct RowPatch {
    pub row: u16,
    pub cells: Vec<TerminalCell>,
    /// Sparse entries use column indices within this row.
    pub graphemes: Vec<CellGrapheme>,
}

/// 서버 수신 명령 프레임 디코드 결과 (§4.4). Delta 접속은 RequestKeyframe 제어를 함께 나른다.
// large_enum_variant 예외: WireCmd와 같은 이유 — Command는 RuntimeCommand를 원형대로 담고,
// RequestKeyframe(8B)와의 크기 차만으로 boxing을 강요하지 않는다.
#[allow(clippy::large_enum_variant)]
pub(crate) enum DecodedCommand {
    /// 실제 실행할 런타임 명령.
    Command(RuntimeCommand),
    /// 이 세션의 keyframe 재동기화 요청 — 서버 pump가 다음 tick에 baseline을 버리고 keyframe.
    RequestKeyframe(SessionId),
}

/// 클라이언트 수신 이벤트 프레임 디코드 결과 (§3.2/§4.4). Delta 접속은 viewport를
/// keyframe/delta 두 형태로 받고, reader 스레드가 상태(recon)를 들고 재구성한다.
pub(crate) enum DecodedEvent {
    /// viewport 아닌 상태 이벤트 — 그대로 dispatch.
    Event(RuntimeEvent),
    /// 세션 viewport 전체 기준선.
    Keyframe {
        session: SessionId,
        seq: u64,
        snapshot: Arc<TerminalViewportSnapshot>,
        bracketed_paste: bool,
    },
    /// 직전 전송본 대비 변경분.
    Delta {
        session: SessionId,
        seq: u64,
        base_seq: u64,
        delta: ViewportDelta,
        bracketed_paste: bool,
    },
    KeyframeTracked {
        session: SessionId,
        seq: u64,
        snapshot: Arc<TerminalViewportSnapshot>,
        bracketed_paste: bool,
        stamp: crate::ResizeStamp,
    },
    DeltaTracked {
        session: SessionId,
        seq: u64,
        base_seq: u64,
        delta: ViewportDelta,
        bracketed_paste: bool,
        stamp: crate::ResizeStamp,
    },
}

/// 접속별 와이어 코덱 — 협상된 기능이 결정한다 (§3.2). 접속 단위로 확정되므로
/// pump/reader가 프레임을 인코딩·디코딩하는 방식이 하나로 고정된다.
#[derive(Clone, Copy)]
pub(crate) enum Codec {
    /// delta 미협상: RuntimeEvent/RuntimeCommand를 봉투 없이 직렬화 — **오늘과 동일 바이트**.
    Plain,
    /// delta 협상: WireMsg/WireCmd 봉투 사용. viewport는 keyframe/delta로 스트리밍된다.
    Delta,
}

impl Codec {
    /// 협상된 features_ack로 코덱을 고른다.
    pub(crate) fn from_features(features: u32) -> Self {
        if features & FEAT_DELTA_VIEWPORT != 0 {
            Codec::Delta
        } else {
            Codec::Plain
        }
    }

    /// viewport 아닌 이벤트 프레임 payload 인코딩. Plain은 기존과 바이트 동일.
    /// (Delta에서 viewport는 이 경로가 아니라 [`encode_wire_msg`]로 keyframe/delta 인코딩.)
    pub(crate) fn encode_event(&self, event: &RuntimeEvent) -> anyhow::Result<Vec<u8>> {
        let payload = match self {
            Codec::Plain => postcard::to_allocvec(event)?,
            Codec::Delta => encode_wire_msg(&WireMsg::Event(event.clone()))?,
        };
        Ok(payload)
    }

    /// 이벤트 프레임 payload 디코딩 (클라이언트 수신). Delta는 keyframe/delta를 구분해 돌려준다.
    pub(crate) fn decode_event(&self, frame: &[u8]) -> anyhow::Result<DecodedEvent> {
        match self {
            Codec::Plain => Ok(DecodedEvent::Event(postcard::from_bytes(frame)?)),
            Codec::Delta => Ok(match postcard::from_bytes::<WireMsg>(frame)? {
                WireMsg::Event(event) => DecodedEvent::Event(event),
                WireMsg::ViewportKeyframeTracked {
                    session,
                    seq,
                    snapshot,
                    bracketed_paste,
                    stamp,
                } => DecodedEvent::KeyframeTracked {
                    session,
                    seq,
                    snapshot,
                    bracketed_paste,
                    stamp,
                },
                WireMsg::ViewportDeltaTracked {
                    session,
                    seq,
                    base_seq,
                    delta,
                    bracketed_paste,
                    stamp,
                } => DecodedEvent::DeltaTracked {
                    session,
                    seq,
                    base_seq,
                    delta,
                    bracketed_paste,
                    stamp,
                },
                WireMsg::ViewportKeyframe {
                    session,
                    seq,
                    snapshot,
                    bracketed_paste,
                } => DecodedEvent::Keyframe {
                    session,
                    seq,
                    snapshot,
                    bracketed_paste,
                },
                WireMsg::ViewportDelta {
                    session,
                    seq,
                    base_seq,
                    delta,
                    bracketed_paste,
                } => DecodedEvent::Delta {
                    session,
                    seq,
                    base_seq,
                    delta,
                    bracketed_paste,
                },
            }),
        }
    }

    /// 명령 프레임 payload 인코딩 (클라이언트 송신). Plain은 기존과 바이트 동일.
    pub(crate) fn encode_command(&self, command: &RuntimeCommand) -> anyhow::Result<Vec<u8>> {
        let payload = match self {
            Codec::Plain => postcard::to_allocvec(command)?,
            Codec::Delta => postcard::to_allocvec(&WireCmd::Command(command.clone()))?,
        };
        Ok(payload)
    }

    /// 명령 프레임 payload 디코딩 (서버 수신). Delta는 RequestKeyframe 제어를 구분해 돌려준다.
    pub(crate) fn decode_command(&self, frame: &[u8]) -> anyhow::Result<DecodedCommand> {
        match self {
            Codec::Plain => Ok(DecodedCommand::Command(postcard::from_bytes(frame)?)),
            Codec::Delta => Ok(match postcard::from_bytes::<WireCmd>(frame)? {
                WireCmd::Command(command) => DecodedCommand::Command(command),
                WireCmd::RequestKeyframe { session } => DecodedCommand::RequestKeyframe(session),
            }),
        }
    }
}

/// [`WireMsg`] 프레임 payload 인코딩 (Delta 접속의 pump가 viewport keyframe/delta 송신 시).
pub(crate) fn encode_wire_msg(msg: &WireMsg) -> anyhow::Result<Vec<u8>> {
    Ok(postcard::to_allocvec(msg)?)
}

/// RequestKeyframe 제어 프레임 payload 인코딩 (§4.4, Delta 접속 전용). 클라이언트 reader가
/// seq gap을 보면 이 프레임을 명령 채널로 보내 서버에 keyframe 재동기화를 요청한다.
pub(crate) fn encode_request_keyframe(session: SessionId) -> anyhow::Result<Vec<u8>> {
    Ok(postcard::to_allocvec(&WireCmd::RequestKeyframe {
        session,
    })?)
}

/// row 단위 content-diff (§4.1, §4.7). 직전 전송본 `prev` 대비 `cur`의 변경분을 만든다.
/// keyframe으로 폴백해야 하면 `None`:
/// - 차원(cols/rows) 또는 alt-screen 토글 변경 → baseline 무효 (§4.4-2,3).
/// - 변경 row가 [`HEAVY_REPAINT_PERCENT`]% 이상 → keyframe이 더 싸다 (§4.4-4).
///
/// `prev`/`cur`는 각각 `cols*rows`개 셀(호출부가 보장; validate_event가 와이어에서 이미 강제).
pub(crate) fn diff_viewport(
    prev: &TerminalViewportSnapshot,
    cur: &TerminalViewportSnapshot,
) -> Option<ViewportDelta> {
    if prev.cols != cur.cols || prev.rows != cur.rows || prev.is_alt_screen != cur.is_alt_screen {
        return None; // → keyframe
    }
    let cols = cur.cols as usize;
    let mut changed_rows = Vec::new();
    for r in 0..cur.rows as usize {
        let rng = r * cols..(r + 1) * cols;
        if prev.visible_cells[rng.clone()] != cur.visible_cells[rng.clone()]
            || prev.row_graphemes(r) != cur.row_graphemes(r)
        {
            changed_rows.push(RowPatch {
                row: r as u16,
                cells: cur.visible_cells[rng].to_vec(),
                graphemes: cur
                    .row_graphemes(r)
                    .iter()
                    .map(|entry| CellGrapheme {
                        index: entry.index - r * cols,
                        text: entry.text.clone(),
                    })
                    .collect(),
            });
        }
    }
    // heavy-repaint 폴백: 변경 row * 100 ≥ 전체 row * 임계% 이면 keyframe.
    if changed_rows.len() * 100 >= cur.rows as usize * HEAVY_REPAINT_PERCENT {
        return None;
    }
    Some(ViewportDelta {
        cols: cur.cols,
        rows: cur.rows,
        cursor: cur.cursor,
        scroll_offset: cur.scroll_offset,
        is_alt_screen: cur.is_alt_screen,
        title: cur.title.clone(),
        changed_rows,
    })
}

/// delta 재구성 (§4.3): 직전 재구성본 `prev`에 변경분 `delta`를 얹어 전체 스냅샷을 만든다.
///
/// **기형 peer 방어(codex P1):** 정상 경로에서는 서버 diff가 차원 일치를 보장하지만,
/// 원격 peer가 보낸 `ViewportDelta`가 기형(baseline과 차원 불일치 / `row`가 범위 밖 /
/// `cells.len()`이 cols와 불일치)이면 `copy_from_slice`에서 reader 스레드가 패닉한다.
/// 그래서 적용 **전에** 전부 검증하고, 불일치면 `Err`를 돌려 호출측이 gap 복구(keyframe
/// 재요청) 경로로 안전하게 처리하게 한다 — 인덱스 out-of-bounds/패닉 없음.
pub(crate) fn try_apply_delta(
    prev: &TerminalViewportSnapshot,
    delta: &ViewportDelta,
) -> Result<TerminalViewportSnapshot, &'static str> {
    if prev.cols != delta.cols || prev.rows != delta.rows {
        return Err("delta 차원이 baseline과 불일치");
    }
    let cols = delta.cols as usize;
    let rows = delta.rows as usize;
    for patch in &delta.changed_rows {
        if patch.row as usize >= rows {
            return Err("delta row 인덱스가 범위 밖");
        }
        if patch.cells.len() != cols {
            return Err("delta patch cells 수가 cols와 불일치");
        }
        validate_cell_graphemes(&patch.cells, &patch.graphemes)?;
    }
    // baseline이 온전한지도 확인 — cols*rows 슬라이스 인덱싱이 안전해야 한다.
    if prev.visible_cells.len() != cols * rows {
        return Err("baseline visible_cells 크기가 cols*rows와 불일치");
    }
    validate_cell_graphemes(&prev.visible_cells, &prev.graphemes)?;
    let mut graphemes = prev.graphemes.to_vec();
    let mut cells = prev.visible_cells.to_vec(); // COW clone (오늘도 Arc 클론 취급 중)
    for patch in &delta.changed_rows {
        let base = patch.row as usize * cols;
        cells[base..base + cols].copy_from_slice(&patch.cells);
        graphemes.retain(|entry| entry.index < base || entry.index >= base + cols);
        graphemes.extend(patch.graphemes.iter().map(|entry| CellGrapheme {
            index: base + entry.index,
            text: entry.text.clone(),
        }));
    }
    Ok(TerminalViewportSnapshot {
        cols: delta.cols,
        rows: delta.rows,
        cursor: delta.cursor,
        visible_cells: cells.into(),
        graphemes: {
            graphemes.sort_unstable_by_key(|entry| entry.index);
            terminal::share_cell_graphemes(graphemes)
        },
        dirty_ranges: row_patches_to_dirty_ranges(&delta.changed_rows, cols, rows),
        title: delta.title.clone(),
        scroll_offset: delta.scroll_offset,
        is_alt_screen: delta.is_alt_screen,
    })
}

fn row_patches_to_dirty_ranges(patches: &[RowPatch], cols: usize, rows: usize) -> Vec<CellRange> {
    if cols == 0 || rows == 0 || patches.is_empty() {
        return Vec::new();
    }
    let mut changed_rows: Vec<usize> = patches
        .iter()
        .map(|patch| patch.row as usize)
        .filter(|row| *row < rows)
        .collect();
    changed_rows.sort_unstable();
    changed_rows.dedup();

    let mut ranges = Vec::new();
    let mut start_row: Option<usize> = None;
    let mut last_row = 0usize;
    for row in changed_rows {
        match start_row {
            None => {
                start_row = Some(row);
                last_row = row;
            }
            Some(start) if row == last_row.saturating_add(1) => {
                start_row = Some(start);
                last_row = row;
            }
            Some(start) => {
                ranges.push(CellRange {
                    start: start * cols,
                    end: (last_row + 1) * cols,
                });
                start_row = Some(row);
                last_row = row;
            }
        }
    }
    if let Some(start) = start_row {
        ranges.push(CellRange {
            start: start * cols,
            end: (last_row + 1) * cols,
        });
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracked_resize_append_preserves_legacy_command_event_and_envelope_bytes() {
        let command = RuntimeCommand::Resize {
            session: SessionId(7),
            cols: 80,
            rows: 24,
        };
        assert_eq!(postcard::to_allocvec(&command).unwrap(), [3, 7, 80, 24]);
        let event = RuntimeEvent::ShellSpawned {
            session: SessionId(7),
        };
        assert_eq!(postcard::to_allocvec(&event).unwrap(), [0, 7]);
        assert_eq!(
            postcard::to_allocvec(&WireMsg::Event(event)).unwrap(),
            [0, 0, 7]
        );
    }

    #[test]
    fn compact_cell_keeps_six_field_wire_bytes_and_json() {
        #[derive(serde::Serialize)]
        struct LegacyCell {
            c: char,
            fg: [u8; 3],
            bg: [u8; 3],
            wide: bool,
            wide_spacer: bool,
            attrs: terminal::CellAttrs,
        }
        for attrs in 0..32 {
            for wide in [false, true] {
                for spacer in [false, true] {
                    let cell = TerminalCell::new(
                        '한',
                        [1, 2, 3],
                        [4, 5, 6],
                        wide,
                        spacer,
                        terminal::CellAttrs(attrs),
                    );
                    let legacy = LegacyCell {
                        c: cell.c,
                        fg: cell.fg,
                        bg: cell.bg,
                        wide,
                        wide_spacer: spacer,
                        attrs: terminal::CellAttrs(attrs),
                    };
                    let actual = postcard::to_allocvec(&cell).unwrap();
                    assert_eq!(actual, postcard::to_allocvec(&legacy).unwrap());
                    assert_eq!(postcard::from_bytes::<TerminalCell>(&actual).unwrap(), cell);
                    assert_eq!(
                        serde_json::to_string(&cell).unwrap(),
                        serde_json::to_string(&legacy).unwrap()
                    );
                }
            }
        }
        let malformed = LegacyCell {
            c: 'a',
            fg: [0; 3],
            bg: [0; 3],
            wide: false,
            wide_spacer: false,
            attrs: terminal::CellAttrs(0x80),
        };
        assert!(
            postcard::from_bytes::<TerminalCell>(&postcard::to_allocvec(&malformed).unwrap())
                .is_err()
        );
    }

    #[test]
    fn protocol_version_tracks_live_scrollback_wire_variants() {
        let protocol_source = include_str!("protocol.rs");
        let command_source = include_str!("command.rs");
        let event_source = include_str!("event.rs");

        assert!(command_source.contains("InspectUnattachedSessions"));
        assert!(command_source.contains("KillUnattachedSessions"));
        assert!(event_source.contains("UnattachedSessionsInspected"));
        assert!(event_source.contains("UnattachedSessionsKilled"));
        assert!(command_source.contains("SetScrollbackLimit"));
        assert!(event_source.contains("ScrollbackLimitApplied"));
        assert!(protocol_source.contains("**v13**"));
        assert_eq!(PROTO_VERSION, 24);
    }
}
