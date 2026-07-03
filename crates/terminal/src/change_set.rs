/// 설계문서 8.1 TerminalChangeSet. feed 후 변경 요약 —
/// 로그/status detector(PR-11/12)와 렌더 최적화(PR-21)가 소비한다.
#[derive(Debug, Default, PartialEq)]
pub struct TerminalChangeSet {
    pub dirty_rows: Vec<u16>,
    pub cursor_changed: bool,
    pub title_changed: bool,
    pub bell: bool,
    /// 설계 8.1에 없는 확장: 터미널 질의(DA 등)에 대한 응답 바이트 —
    /// 호출측이 반드시 PTY에 되돌려 써야 질의형 TUI가 멈추지 않는다.
    pub pty_responses: Vec<u8>,
}
