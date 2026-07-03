use deppy_core::SessionId;

use crate::session::{Session, SessionKind};

/// 플랫폼 기본 셸 세션을 spawn한다.
pub fn spawn_shell(
    id: SessionId,
    cols: u16,
    rows: u16,
    scrollback_lines: usize,
) -> anyhow::Result<Session> {
    Session::spawn_with_spec(
        id,
        SessionKind::Shell,
        &pty::default_shell(),
        cols,
        rows,
        scrollback_lines,
    )
}
