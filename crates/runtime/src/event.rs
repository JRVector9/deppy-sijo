use std::sync::Arc;

use terminal::TerminalViewportSnapshot;

use mux::MuxSnapshot;
use pty::PtyInputPressure;
use session::{SessionStatus, SessionStatusView};

use crate::command::SessionId;
use crate::resource_monitor::{ProcessResourceSnapshot, SessionResourceUsage};

/// SpawnFailed의 출처 구분 — 셸/에이전트 UI가 서로의 실패를 오귀속하지 않게 한다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SpawnKind {
    Shell,
    Agent,
}

/// Stable localized message payload crossing the runtime boundary.
///
/// `message_id` is the user-facing key. `args` contains non-localized values
/// such as command names, credential ids, or diagnostic text. `diagnostic` is
/// optional debug detail and should not be used as the stable UI message.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MessagePayload {
    pub message_id: String,
    pub args: Vec<MessageArg>,
    pub diagnostic: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MessageArg {
    pub key: String,
    pub value: String,
}

impl MessagePayload {
    pub fn new(message_id: impl Into<String>) -> Self {
        Self {
            message_id: message_id.into(),
            args: Vec::new(),
            diagnostic: None,
        }
    }

    pub fn arg(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.args.push(MessageArg {
            key: key.into(),
            value: value.into(),
        });
        self
    }

    pub fn diagnostic(mut self, diagnostic: impl Into<String>) -> Self {
        self.diagnostic = Some(diagnostic.into());
        self
    }

    pub fn arg_value(&self, key: &str) -> Option<&str> {
        self.args
            .iter()
            .find(|arg| arg.key == key)
            .map(|arg| arg.value.as_str())
    }
}

/// Runtime → UI 이벤트 (설계문서 2.1).
/// Viewport는 output batch 주기(설계문서 10.1)마다 push된다 —
/// remote 전환 시 terminal delta 스트림으로 대체되는 자리 (8.2).
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub enum RuntimeEvent {
    ShellSpawned {
        session: SessionId,
    },
    /// SpawnAgent 성공 — 셸과 소유 UI가 다르므로 이벤트를 구분한다
    AgentSpawned {
        session: SessionId,
    },
    SpawnFailed {
        kind: SpawnKind,
        message: MessagePayload,
    },
    Viewport {
        session: SessionId,
        snapshot: Arc<TerminalViewportSnapshot>,
        /// 입력 매핑(bracketed paste wrap)에 필요한 터미널 모드
        bracketed_paste: bool,
    },
    SessionExited {
        session: SessionId,
        exit_code: Option<u32>,
    },
    /// mux 구조(tab/pane/layout/focus) 변경 — UI는 이걸로만 배치를 그린다
    MuxUpdated {
        snapshot: Arc<MuxSnapshot>,
    },
    /// status detector 감지 결과 (PR-12) — batch 주기로 평가된다
    SessionStatusChanged {
        session: SessionId,
        status: SessionStatus,
    },
    /// Process resource sample. App process CPU/RSS plus optional per-session
    /// child process tree aggregation.
    ResourceUsage {
        snapshot: ProcessResourceSnapshot,
        session_usage: Vec<SessionResourceUsage>,
    },
    /// PTY input queue pressure. UI may show this as a visible backpressure
    /// signal, but input is never silently dropped.
    PtyInputPressure {
        session: SessionId,
        pressure: PtyInputPressure,
    },
    /// Additive status view carrying confidence/source/override state. Existing
    /// `SessionStatusChanged` remains the compatibility event.
    SessionStatusViewChanged {
        session: SessionId,
        view: SessionStatusView,
    },
    /// 재시작 시 아카이브에서 열람 전용으로 복원된 이미-종료된 세션 (PR-A2).
    /// `SessionExited`와 달리 완료 **알림을 재발화하지 않는다** — 대신 UI가 생존
    /// 추적(LiveSessionTracker)과 exit_code 부기를 갱신하는 데 쓴다.
    /// **variant는 enum 끝에만 추가** (postcard discriminant — remote wire 호환).
    SessionRestored {
        session: SessionId,
        exit_code: Option<u32>,
    },
    /// `SearchScrollback` 응답 (T3). 요청 query를 되돌려줘 UI가 늦게 온 stale 결과를
    /// 버릴 수 있게 한다. **variant는 enum 끝에만 추가** (postcard discriminant — wire 호환).
    ScrollbackSearchResult {
        session: SessionId,
        query: String,
        result: terminal::ScrollbackSearchResult,
    },
}

/// 최신값 슬롯에서 Viewport를 교체할 때, **아직 소비되지 않은** 이전 이벤트의
/// dirty_ranges를 새 이벤트에 합친다.
///
/// 슬롯 덮어쓰기는 콘텐츠(전체 grid)에는 안전하지만 `dirty_ranges`는 "직전
/// take_snapshot 대비 델타"다 — 소비 전에 덮어쓰면 이전 델타의 행들이 renderer의
/// 행 갤리 재shaping 대상에서 빠져 **화면에 낡은 텍스트가 남는다** (2026-07-14
/// "붙여넣기 후 커서 앞 글자 미표시" 원인. worker 8ms 페이싱 vs UI vsync 16.6ms라
/// 출력 버스트에서는 덮어쓰기가 상시 발생한다).
///
/// 크기/스크롤이 바뀐 경우 이전 범위가 새 grid와 안 맞을 수 있지만, renderer는
/// shape 변화(cols/rows/scroll/alt) 시 캐시를 통째로 버리므로 과잉 무효화만
/// 생길 뿐 유실은 없다 — 무조건 합쳐도 안전하다.
pub fn merge_unconsumed_viewport_dirty(prev: &RuntimeEvent, next: &mut RuntimeEvent) {
    if let (
        RuntimeEvent::Viewport {
            snapshot: prev_snapshot,
            ..
        },
        RuntimeEvent::Viewport { snapshot, .. },
    ) = (prev, next)
        && !prev_snapshot.dirty_ranges.is_empty()
    {
        // visible_cells는 Arc 공유라 make_mut의 스냅샷 클론은 저렴하다(셀 복사 없음).
        let merged = Arc::make_mut(snapshot);
        merged
            .dirty_ranges
            .extend(prev_snapshot.dirty_ranges.iter().cloned());
        // 소비자가 오래 멈춘 채 덮어쓰기가 반복되면 누적 범위가 무한히 자란다
        // (codex 리뷰 HIGH). 행 수를 넘으면 정보량이 "전체 dirty"와 같으므로
        // 전체 grid 범위 하나로 붕괴시켜 상한을 둔다.
        if merged.dirty_ranges.len() > merged.rows as usize {
            let cells = (merged.cols as usize) * (merged.rows as usize);
            merged.dirty_ranges.clear();
            merged.dirty_ranges.push(terminal::CellRange {
                start: 0,
                end: cells.saturating_sub(1),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn viewport_event(dirty: Vec<terminal::CellRange>) -> RuntimeEvent {
        RuntimeEvent::Viewport {
            session: SessionId(1),
            snapshot: Arc::new(TerminalViewportSnapshot {
                cols: 4,
                rows: 2,
                cursor: terminal::CursorSnapshot {
                    col: 0,
                    row: 0,
                    shape: terminal::CursorShape::Block,
                    visible: true,
                },
                visible_cells: Vec::new().into(),
                dirty_ranges: dirty,
                title: None,
                scroll_offset: 0,
                is_alt_screen: false,
            }),
            bracketed_paste: false,
        }
    }

    fn dirty_of(event: &RuntimeEvent) -> &[terminal::CellRange] {
        match event {
            RuntimeEvent::Viewport { snapshot, .. } => &snapshot.dirty_ranges,
            _ => panic!("viewport 아님"),
        }
    }

    #[test]
    fn 슬롯_덮어쓰기는_미소비_dirty_델타를_합친다() {
        let range = |start, end| terminal::CellRange { start, end };
        let prev = viewport_event(vec![range(0, 3)]);
        let mut next = viewport_event(vec![range(4, 7)]);
        merge_unconsumed_viewport_dirty(&prev, &mut next);
        assert_eq!(dirty_of(&next), &[range(4, 7), range(0, 3)]);

        // 이전 델타가 비었으면 스냅샷 클론(make_mut) 자체를 하지 않는다.
        let empty_prev = viewport_event(vec![]);
        let mut next = viewport_event(vec![range(4, 7)]);
        let before = match &next {
            RuntimeEvent::Viewport { snapshot, .. } => Arc::as_ptr(snapshot),
            _ => unreachable!(),
        };
        merge_unconsumed_viewport_dirty(&empty_prev, &mut next);
        let after = match &next {
            RuntimeEvent::Viewport { snapshot, .. } => Arc::as_ptr(snapshot),
            _ => unreachable!(),
        };
        assert_eq!(before, after);
        assert_eq!(dirty_of(&next), &[range(4, 7)]);
    }

    #[test]
    fn spawn_failed_payload_uses_stable_message_id_and_args() {
        let payload = MessagePayload::new("runtime.spawn_failed.shell")
            .arg("error", "missing executable")
            .diagnostic("No such file or directory");
        assert_eq!(payload.message_id, "runtime.spawn_failed.shell");
        assert_eq!(payload.arg_value("error"), Some("missing executable"));
        assert_eq!(
            payload.diagnostic.as_deref(),
            Some("No such file or directory")
        );
    }

    #[test]
    fn spawn_failed_payload_roundtrips_through_postcard() {
        let event = RuntimeEvent::SpawnFailed {
            kind: SpawnKind::Agent,
            message: MessagePayload::new("runtime.spawn_failed.agent_secret")
                .arg("credential_id", "cred-1")
                .arg("error", "not found")
                .diagnostic("keyring lookup failed"),
        };
        let bytes = postcard::to_allocvec(&event).unwrap();
        let decoded: RuntimeEvent = postcard::from_bytes(&bytes).unwrap();
        match decoded {
            RuntimeEvent::SpawnFailed { kind, message } => {
                assert_eq!(kind, SpawnKind::Agent);
                assert_eq!(message.message_id, "runtime.spawn_failed.agent_secret");
                assert_eq!(message.arg_value("credential_id"), Some("cred-1"));
                assert_eq!(message.arg_value("error"), Some("not found"));
            }
            _ => panic!("unexpected event"),
        }
    }
}
