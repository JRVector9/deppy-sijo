//! 공유 id 타입 + 최소 공용 유틸(time/fs) (설계문서 9장 core, 10장 의존 방향의 최하층).

pub mod fs;
pub mod time;

/// runtime 내부 세션 식별자. 영속 id(sessions 테이블)와의 매핑은 PR-08.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct SessionId(pub u64);

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
