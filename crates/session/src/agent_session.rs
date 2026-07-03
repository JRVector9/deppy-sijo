use deppy_core::SessionId;
use pty::CommandSpec;

use crate::session::{Session, SessionKind};

/// agent 세션을 spawn한다 (설계문서 PR-09가 소비).
/// env는 호출측(runtime)이 secret resolve를 끝낸 평문 쌍 —
/// 이 crate는 secret을 모른다 (PR-08 완료 기준).
pub fn spawn_agent(
    id: SessionId,
    spec: &CommandSpec,
    cols: u16,
    rows: u16,
    scrollback_lines: usize,
) -> anyhow::Result<Session> {
    Session::spawn_with_spec(id, SessionKind::Agent, spec, cols, rows, scrollback_lines)
}
