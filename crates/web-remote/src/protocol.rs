//! WS JSON 프로토콜 v1 — 브라우저 친화 텍스트 프레임 (계획 v3.3 P2).
//!
//! postcard 바이너리 코덱은 데스크톱 원격(remote.rs) 전용으로 남기고, 폰 PWA는 JSON만
//! 쓴다. 첫 프레임은 반드시 `{"type":"auth", ...}` — 인증 전 다른 메시지는 무시된다.
//!
//! 서버→클라 프레임은 대시보드(런타임 유래)와 승인(DB 유래)이 갱신 주기가 달라 분리해
//! 보낸다. 클라이언트는 각 프레임을 독립적으로 반영한다.

use serde::{Deserialize, Serialize};

/// 프로토콜 버전 — 클라/서버 합의값. 하위호환이 깨지면 증가시킨다.
pub const PROTOCOL_VERSION: u32 = 1;

/// 클라이언트 → 서버.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    /// 첫 프레임 — 페어링 토큰 인증. `v`는 선택(구클라 호환).
    Auth {
        token: String,
        #[serde(default)]
        v: u32,
    },
    /// 승인 결정 — 기존 resolve_approval 경로로 직행한다.
    Resolve {
        id: String,
        allowed: bool,
        #[serde(default)]
        remember: bool,
    },
}

impl ClientMsg {
    /// 텍스트 프레임을 파싱한다. 알 수 없는/기형 메시지는 None(무시).
    pub fn parse(text: &str) -> Option<Self> {
        serde_json::from_str(text).ok()
    }
}

/// 세션 한 행(대시보드). `status`는 snake_case 문자열(런타임 SessionStatus 매핑).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionView {
    pub id: u64,
    pub title: String,
    pub status: &'static str,
    /// 종료(SessionExited/Restored 관측) — 완료 배지용.
    pub exited: bool,
}

/// 앱 프로세스 리소스 요약(대시보드). rss는 표시 편의상 MB.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResourceView {
    pub cpu: Option<f32>,
    pub rss_mb: u64,
}

/// 승인 대기 한 행. `preview`는 proxy가 이미 redact한 표시용 텍스트 — 클라이언트는
/// 반드시 textContent로만 삽입한다(innerHTML 금지, 불변 원칙 6/10).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ApprovalView {
    pub id: String,
    pub server: String,
    pub tool: String,
    pub preview: String,
    pub created_at: i64,
}

/// 서버 → 클라이언트. 내부 태그(`type`)로 클라가 분기한다.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    /// 인증 성공 직후 1회.
    Welcome { v: u32 },
    /// 세션 상태/리소스 스냅샷(런타임 이벤트 유래).
    Dashboard {
        sessions: Vec<SessionView>,
        resource: Option<ResourceView>,
    },
    /// 승인 대기 목록(DB 폴링 유래).
    Approvals { pending: Vec<ApprovalView> },
    /// 인증 실패 등 — 직후 close.
    Error { message: String },
}

impl ServerMsg {
    /// JSON 텍스트로 직렬화한다. 직렬화 실패(사실상 불가)는 최소 error 프레임으로 대체.
    pub fn encode(&self) -> String {
        serde_json::to_string(self)
            .unwrap_or_else(|_| r#"{"type":"error","message":"encode failed"}"#.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_프레임을_파싱한다() {
        let msg = ClientMsg::parse(r#"{"type":"auth","v":1,"token":"abc"}"#).unwrap();
        assert_eq!(
            msg,
            ClientMsg::Auth {
                token: "abc".into(),
                v: 1
            }
        );
        // v 생략 허용(기본 0)
        let msg = ClientMsg::parse(r#"{"type":"auth","token":"x"}"#).unwrap();
        assert_eq!(
            msg,
            ClientMsg::Auth {
                token: "x".into(),
                v: 0
            }
        );
    }

    #[test]
    fn resolve_프레임을_파싱한다() {
        let msg = ClientMsg::parse(r#"{"type":"resolve","id":"i1","allowed":true}"#).unwrap();
        assert_eq!(
            msg,
            ClientMsg::Resolve {
                id: "i1".into(),
                allowed: true,
                remember: false
            }
        );
        let msg =
            ClientMsg::parse(r#"{"type":"resolve","id":"i2","allowed":false,"remember":true}"#)
                .unwrap();
        assert_eq!(
            msg,
            ClientMsg::Resolve {
                id: "i2".into(),
                allowed: false,
                remember: true
            }
        );
    }

    #[test]
    fn 기형이나_미지_메시지는_none() {
        assert!(ClientMsg::parse("not json").is_none());
        assert!(ClientMsg::parse(r#"{"type":"nope"}"#).is_none());
        assert!(ClientMsg::parse(r#"{"type":"auth"}"#).is_none()); // token 필수
    }

    #[test]
    fn 서버_프레임_직렬화는_type_태그를_싣는다() {
        let welcome = ServerMsg::Welcome {
            v: PROTOCOL_VERSION,
        }
        .encode();
        assert_eq!(welcome, r#"{"type":"welcome","v":1}"#);

        let dash = ServerMsg::Dashboard {
            sessions: vec![SessionView {
                id: 7,
                title: "claude".into(),
                status: "needs_approval",
                exited: false,
            }],
            resource: Some(ResourceView {
                cpu: Some(12.5),
                rss_mb: 340,
            }),
        }
        .encode();
        assert!(dash.contains(r#""type":"dashboard""#), "{dash}");
        assert!(dash.contains(r#""status":"needs_approval""#), "{dash}");
        assert!(dash.contains(r#""rss_mb":340"#), "{dash}");

        let appr = ServerMsg::Approvals {
            pending: vec![ApprovalView {
                id: "a1".into(),
                server: "github".into(),
                tool: "create_issue".into(),
                preview: "{redacted}".into(),
                created_at: 1720,
            }],
        }
        .encode();
        assert!(appr.contains(r#""type":"approvals""#), "{appr}");
        assert!(appr.contains(r#""tool":"create_issue""#), "{appr}");
    }
}
