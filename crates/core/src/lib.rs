//! 공유 id 타입 + 최소 공용 유틸(time/fs) (설계문서 9장 core, 10장 의존 방향의 최하층).

pub mod credential_env;
pub mod env_sources;
pub mod fs;
pub mod time;

/// runtime 내부 세션 식별자. 영속 id(sessions 테이블)와의 매핑은 PR-08.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct SessionId(pub u64);

/// 에이전트 hook 세션 키 — `{workspace_id}:{session_id}`.
///
/// runtime이 셸에 `DEPPY_SESSION_ID`로 주입하는 값의 형식이며(in_process의 session_key),
/// hook이 보고하는 `agent_needs_input.session_key`와 proxy가 승인 행에 싣는
/// `pending_approvals.pane_id`가 모두 이 형식이다. SessionId는 워커마다 1부터 재배정되어
/// 전역 유일하지 않으므로 workspace_id로 스코프한다.
///
/// **DB 조인으로는 세션을 찾을 수 없다**(2026-07-17 실측): 이 키는 `mux_panes.id`(순수
/// UUID)와 다른 식별자 공간이라 `pending_approvals.pane_id = mux_panes.id` 조인은 절대
/// 매칭되지 않는다. 세션을 알아내려면 이 함수로 파싱해 워크스페이스별 런타임 상태에서
/// 찾아야 한다 — 생성 규칙(runtime)과 소비처(app 인박스, web-remote 대시보드)가 갈라져
/// 있어 파싱을 여기 최하층에 둔다.
pub fn parse_session_key(key: &str) -> Option<(&str, SessionId)> {
    // 워크스페이스 id는 UUID라 ':'를 포함하지 않지만, 형식이 바뀌어도 마지막 ':'가
    // 구분자라는 규약은 유지되도록 rsplit을 쓴다.
    let (workspace_id, session_id) = key.rsplit_once(':')?;
    if workspace_id.is_empty() {
        return None;
    }
    let session_id = session_id.parse::<u64>().ok()?;
    Some((workspace_id, SessionId(session_id)))
}

#[cfg(test)]
mod session_key_tests {
    use super::*;

    #[test]
    fn 워크스페이스와_세션번호를_나눈다() {
        let (ws, session) = parse_session_key("315f68b6-333f-409f-a2c5-922b9eacfd7e:2").unwrap();
        assert_eq!(ws, "315f68b6-333f-409f-a2c5-922b9eacfd7e");
        assert_eq!(session, SessionId(2));
    }

    #[test]
    fn 형식이_아니면_none() {
        assert_eq!(parse_session_key("no-colon"), None);
        assert_eq!(parse_session_key("ws:not-a-number"), None);
        assert_eq!(parse_session_key(":42"), None); // 빈 workspace id
        assert_eq!(parse_session_key("ws:"), None); // 빈 session id
        assert_eq!(parse_session_key(""), None);
    }

    /// mux_panes.id(순수 UUID)를 넣으면 반드시 None이어야 한다 — 두 식별자 공간을
    /// 섞어 쓰던 것이 조인 버그의 원인이었다.
    #[test]
    fn mux_pane_uuid는_세션키가_아니다() {
        assert_eq!(
            parse_session_key("130d9017-be25-469e-8f8d-984abacae701"),
            None
        );
    }
}

/// 영속 테이블(11장)이 TEXT UUID id를 쓰므로 mux 계열 id는 String UUID다 —
/// PR-14 persistence에서 무변환 매핑.
macro_rules! uuid_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
        pub struct $name(pub String);

        impl $name {
            #[allow(clippy::new_without_default)]
            pub fn new() -> Self {
                Self(uuid::Uuid::new_v4().to_string())
            }
        }
    };
}

uuid_id!(
    /// 프로젝트 단위 mux container (workspaces 테이블 id와 동일 개체)
    WorkspaceId
);
uuid_id!(MuxWindowId);
uuid_id!(MuxTabId);
uuid_id!(MuxPaneId);
