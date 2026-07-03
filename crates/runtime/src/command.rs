pub use deppy_core::SessionId;

/// UI → Runtime 명령 (설계문서 2.1). v0은 단일 셸 세션에 필요한 것만.
#[derive(Debug, Clone, PartialEq)]
pub enum RuntimeCommand {
    SpawnShell {
        cols: u16,
        rows: u16,
        /// spawn 시점의 설정값 — 설정 변경이 다음 세션부터 반영되게 한다
        scrollback_lines: usize,
    },
    WriteInput {
        session: SessionId,
        bytes: Vec<u8>,
    },
    Resize {
        session: SessionId,
        cols: u16,
        rows: u16,
    },
    /// scrollback 스크롤 (양수 = 과거로)
    Scroll {
        session: SessionId,
        delta: i32,
    },
    KillSession {
        session: SessionId,
    },
}
