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
#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
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
    /// User status override. Existing detector events remain unchanged.
    SetUserStatusOverride {
        session: SessionId,
        override_: session::UserStatusOverride,
    },
    /// pane 제목을 바꾼다(세션 이름 rename). mux.panes의 title을 갱신하고 영속한다.
    /// (append-only — wire 호환)
    RenamePane {
        pane: MuxPaneId,
        title: String,
    },
    /// 이후 SpawnShell이 사용할 워크스페이스 기본 env(.env 자동 주입 — 2026-07-07).
    /// secret은 credential_id 참조로만 전달되고 worker가 spawn 직전에 resolve한다(6.3).
    /// **wire 계약**: postcard enum discriminant라 variant는 항상 끝에만 추가한다(codex High).
    SetSessionDefaultEnv {
        env_plain: Vec<(String, String)>,
        /// (env key, credential_id)
        env_secrets: Vec<(String, String)>,
    },
    /// 이후 SpawnShell/SpawnAgent가 쓸 셸 cwd를 갱신한다(프로젝트 폴더 live 변경 —
    /// 2026-07-08). None이면 앱 cwd 상속. **wire 계약: variant는 끝에만 추가**.
    SetShellCwd(Option<std::path::PathBuf>),
}

impl std::fmt::Debug for RuntimeCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RuntimeCommand::SpawnShell {
                cols,
                rows,
                scrollback_lines,
            } => f
                .debug_struct("SpawnShell")
                .field("cols", cols)
                .field("rows", rows)
                .field("scrollback_lines", scrollback_lines)
                .finish(),
            RuntimeCommand::SpawnAgent {
                cols,
                rows,
                scrollback_lines,
                agent_config_id,
                command,
                args,
                env_plain,
                env_secrets,
                waiting_regex,
                approval_regex,
                error_regex,
                done_regex,
            } => f
                .debug_struct("SpawnAgent")
                .field("cols", cols)
                .field("rows", rows)
                .field("scrollback_lines", scrollback_lines)
                .field("agent_config_id_set", &agent_config_id.is_some())
                .field("command", command)
                .field("args_count", &args.len())
                .field("env_plain_count", &env_plain.len())
                .field("env_secret_count", &env_secrets.len())
                .field("waiting_regex_set", &waiting_regex.is_some())
                .field("approval_regex_set", &approval_regex.is_some())
                .field("error_regex_set", &error_regex.is_some())
                .field("done_regex_set", &done_regex.is_some())
                .finish(),
            RuntimeCommand::SetSessionDefaultEnv {
                env_plain,
                env_secrets,
            } => f
                .debug_struct("SetSessionDefaultEnv")
                .field("env_plain_count", &env_plain.len())
                .field("env_secret_count", &env_secrets.len())
                .finish(),
            RuntimeCommand::SetShellCwd(cwd) => {
                f.debug_tuple("SetShellCwd").field(&cwd.is_some()).finish()
            }
            RuntimeCommand::WriteInput { session, bytes } => f
                .debug_struct("WriteInput")
                .field("session", session)
                .field("bytes_len", &bytes.len())
                .finish(),
            RuntimeCommand::Resize {
                session,
                cols,
                rows,
            } => f
                .debug_struct("Resize")
                .field("session", session)
                .field("cols", cols)
                .field("rows", rows)
                .finish(),
            RuntimeCommand::Scroll { session, delta } => f
                .debug_struct("Scroll")
                .field("session", session)
                .field("delta", delta)
                .finish(),
            RuntimeCommand::KillSession { session } => f
                .debug_struct("KillSession")
                .field("session", session)
                .finish(),
            RuntimeCommand::SeedRedaction { credential_ids } => f
                .debug_struct("SeedRedaction")
                .field("credential_count", &credential_ids.len())
                .finish(),
            RuntimeCommand::SplitPane {
                pane,
                direction,
                scrollback_lines,
            } => f
                .debug_struct("SplitPane")
                .field("pane", pane)
                .field("direction", direction)
                .field("scrollback_lines", scrollback_lines)
                .finish(),
            RuntimeCommand::ClosePane { pane } => {
                f.debug_struct("ClosePane").field("pane", pane).finish()
            }
            RuntimeCommand::CloseTab { tab } => {
                f.debug_struct("CloseTab").field("tab", tab).finish()
            }
            RuntimeCommand::SelectTab { tab } => {
                f.debug_struct("SelectTab").field("tab", tab).finish()
            }
            RuntimeCommand::FocusPane { pane } => {
                f.debug_struct("FocusPane").field("pane", pane).finish()
            }
            RuntimeCommand::RestoreWorkspace => f.write_str("RestoreWorkspace"),
            RuntimeCommand::SetWorkspaceState(state) => {
                f.debug_tuple("SetWorkspaceState").field(state).finish()
            }
            RuntimeCommand::ResizeSplit { tab, path, ratio } => f
                .debug_struct("ResizeSplit")
                .field("tab", tab)
                .field("path", path)
                .field("ratio", ratio)
                .finish(),
            RuntimeCommand::SetUserStatusOverride { session, override_ } => f
                .debug_struct("SetUserStatusOverride")
                .field("session", session)
                .field("override", override_)
                .finish(),
            RuntimeCommand::RenamePane { pane, title } => f
                .debug_struct("RenamePane")
                .field("pane", pane)
                .field("title", title)
                .finish(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_command_debug는_spawn_agent와_input_payload를_숨긴다() {
        let command = RuntimeCommand::SpawnAgent {
            cols: 120,
            rows: 40,
            scrollback_lines: 10_000,
            agent_config_id: Some("agent-secret-id".to_owned()),
            command: "/bin/sh".to_owned(),
            args: vec!["--token".to_owned(), "sk-debug-never-log".to_owned()],
            env_plain: vec![("API_KEY".to_owned(), "plain-debug-never-log".to_owned())],
            env_secrets: vec![("SECRET".to_owned(), "cred-debug-never-log".to_owned())],
            waiting_regex: Some("waiting-secret-pattern".to_owned()),
            approval_regex: None,
            error_regex: None,
            done_regex: None,
        };
        let input = RuntimeCommand::WriteInput {
            session: SessionId(7),
            bytes: b"paste-debug-never-log".to_vec(),
        };
        let seed = RuntimeCommand::SeedRedaction {
            credential_ids: vec!["cred-seed-never-log".to_owned()],
        };

        let text = format!("{command:?}\n{input:?}\n{seed:?}");

        for forbidden in [
            "agent-secret-id",
            "sk-debug-never-log",
            "plain-debug-never-log",
            "cred-debug-never-log",
            "waiting-secret-pattern",
            "paste-debug-never-log",
            "cred-seed-never-log",
            "API_KEY",
            "SECRET",
        ] {
            assert!(
                !text.contains(forbidden),
                "Debug leaked {forbidden}: {text}"
            );
        }
        assert!(text.contains("args_count"));
        assert!(text.contains("bytes_len"));
        assert!(text.contains("credential_count"));
    }
}
