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
}

#[cfg(test)]
mod tests {
    use super::*;

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
