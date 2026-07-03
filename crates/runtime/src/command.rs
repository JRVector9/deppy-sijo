pub use deppy_core::{MuxPaneId, MuxTabId, SessionId};
pub use mux::SplitDirection;

/// UI → Runtime 명령 (설계문서 2.1). v0은 단일 셸 세션에 필요한 것만.
#[derive(Debug, Clone, PartialEq)]
pub enum RuntimeCommand {
    SpawnShell {
        cols: u16,
        rows: u16,
        /// spawn 시점의 설정값 — 설정 변경이 다음 세션부터 반영되게 한다
        scrollback_lines: usize,
    },
    /// agent command 실행 (설계문서 PR-09). secret env는 credential_id 참조로
    /// 전달되고 worker가 spawn 직전에만 resolve한다 (6.3) — 값은 이 명령에 없다.
    SpawnAgent {
        cols: u16,
        rows: u16,
        scrollback_lines: usize,
        command: String,
        args: Vec<String>,
        env_plain: Vec<(String, String)>,
        /// (env key, credential_id)
        env_secrets: Vec<(String, String)>,
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
    /// focused pane을 분할하고 새 셸 세션을 attach한다 (PR-10)
    SplitPane {
        pane: MuxPaneId,
        direction: SplitDirection,
        scrollback_lines: usize,
    },
    /// pane을 닫는다 — 세션 kill 포함. 마지막 pane이면 tab도 닫힌다
    ClosePane {
        pane: MuxPaneId,
    },
    CloseTab {
        tab: MuxTabId,
    },
    SelectTab {
        tab: MuxTabId,
    },
    /// active pane 변경 — Viewport push 대상(14.4)이 바뀐다
    FocusPane {
        pane: MuxPaneId,
    },
}
