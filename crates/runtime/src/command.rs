pub use deppy_core::{MuxPaneId, MuxTabId, SessionId};
pub use mux::SplitDirection;

/// workspace 런타임 상태 (설계문서 §14.1). 현재 단일 workspace 앱에서 실효 있는 전이는
/// Active↔Warm(앱 최소화/가림 시 render/snapshot 중단, 세션은 유지). Suspended/Closed는
/// workspace "닫기"(세션 종료)가 전제라 multi-workspace 관리 도입 시 완성된다 —
/// worker는 Active가 아니면 snapshot 생성만 멈춘다(Warm 수준). 세션 kill은 안 한다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum WorkspaceRuntimeState {
    /// visible pane render + snapshot (§14.4/14.3은 이 안에서 이미 visible-only)
    Active,
    /// status/log tail만 — renderer/snapshot 금지, 세션(PTY)은 유지
    Warm,
    /// layout/session metadata만 — (workspace-close 전제, 현재 미도달)
    Suspended,
    /// DB metadata만 — (workspace-close 전제, 현재 미도달)
    Closed,
}

/// UI → Runtime 명령 (설계문서 2.1). v0은 단일 셸 세션에 필요한 것만.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
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
        /// agent_configs.id — 세션 영속(§11.1 sessions.agent_id)에 기록된다
        agent_config_id: Option<String>,
        command: String,
        args: Vec<String>,
        env_plain: Vec<(String, String)>,
        /// (env key, credential_id)
        env_secrets: Vec<(String, String)>,
        /// status detector regex (agent_configs *_regex — PR-12)
        waiting_regex: Option<String>,
        approval_regex: Option<String>,
        error_regex: Option<String>,
        done_regex: Option<String>,
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
    /// 저장된 credential들을 로그 redaction 패턴으로 등록한다 (설계문서 7장).
    /// 값은 worker가 keyring에서 읽는다 — 명령에는 id만 실린다.
    SeedRedaction {
        credential_ids: Vec<String>,
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
    /// 이전 실행이 저장한 mux layout을 복원한다 (PR-14, 설계문서 §11.1~11.5·§14).
    /// 앱이 subscribe 직후 1회 보낸다 — subscribe→restore 순서와 "빈 상태" 전제를
    /// 코드로 보장하기 위해 worker 자율 복원이 아닌 명시적 명령으로 트리거한다.
    /// worker는 세션이 하나도 없을 때만 복원한다(이미 SpawnShell 등이 처리됐으면 skip).
    RestoreWorkspace,
    /// workspace 런타임 상태 전환 (§14.1). Active면 snapshot 생성, 그 외는 중단.
    /// **enum 끝에 append** — postcard는 variant를 index로 인코딩하므로 중간 삽입은
    /// 기존 명령의 discriminant를 밀어 remote wire 호환을 깬다 (codex 리뷰).
    SetWorkspaceState(WorkspaceRuntimeState),
    /// split 경계 마우스 드래그 리사이즈 — tab layout 안 Split을 루트 기준
    /// path(0=first/1=second)로 지정해 ratio를 바꾼다. stale path(레이아웃이 그 사이
    /// 바뀜)는 무해하게 무시된다. (append-only — wire 호환)
    ResizeSplit {
        tab: MuxTabId,
        path: Vec<u8>,
        ratio: f32,
    },
}
