//! 워크스페이스 뷰 (설계문서 PR-10): tab bar + split pane 렌더.
//! Runtime Boundary(2장) 준수 — 명령 전송/이벤트 수신/스냅샷 렌더만.
//! mux 배치는 MuxUpdated 스냅샷이 유일한 근거, active tab visible pane만 live render (14.4).

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use runtime::{
    LayoutNode, MuxSnapshot, RuntimeCommand, RuntimeEvent, SessionId, SessionStatus, SpawnKind,
    SplitDirection,
};
use terminal::{TerminalViewportSnapshot, input_mapper, renderer_egui};

use super::format_bytes;
use crate::config::TerminalConfig;

/// 경로 해석 캐시 TTL. 실제 filesystem/process 조회는 App host가 수행한다.
const PATH_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(2);
/// 창(pane) 리사이즈 드래그 중 PTY resize를 보내기 전에 목표 크기가 안정될 때까지
/// 기다리는 디바운스 시간. 드래그 중에는 매 프레임 avail 크기가 바뀌어 목표 cols/rows도
/// 계속 바뀌는데, 그때마다 그대로 PTY에 보내면 alacritty가 매번 실제로 grid를 reflow하고
/// 자식 프로세스에 SIGWINCH를 보내 화면을 다시 그리게 만든다 — 드래그 중 화면이 계속
/// 다시 그려지는 것이 사용자에게 깜빡임으로 보인다(2026-08-18 사용자 보고). 목표가 이
/// 시간만큼 안 바뀌어야 그 순간의 최종 목표를 정확히 한 번 보낸다.
const RESIZE_DRAG_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(120);
const RESIZE_VIEWPORT_QUIET: std::time::Duration = std::time::Duration::from_millis(32);
const RESIZE_VIEWPORT_HARD_DEADLINE: std::time::Duration = std::time::Duration::from_millis(250);
const PROTOCOL_RETRY_BASE: std::time::Duration = std::time::Duration::from_millis(16);
const PROTOCOL_RETRY_LIMIT: u8 = 6;
// 사용자 요청은 고정 상한으로 보관하고, 경로 미리보기는 별도의 최신 한 건만 둔다.
const WORKSPACE_IO_QUEUE_CAP: usize = 8;
const WORKSPACE_PATH_MAX_BYTES: usize = 32 * 1024;
const WORKSPACE_URL_MAX_BYTES: usize = 32 * 1024;
const TERMINAL_CLIPBOARD_PATH_MAX_ITEMS: usize = 16;
const TERMINAL_CLIPBOARD_PATH_MAX_BYTES: usize = 256 * 1024;
const TERMINAL_CLIPBOARD_TEXT_MAX_BYTES: usize = 1024 * 1024;
const WORKSPACE_NOTICE_SUMMARY_MAX_BYTES: usize = 4 * 1024;
const WORKSPACE_NOTICE_BODY_MAX_BYTES: usize = 2 * 1024;
const WORKSPACE_NOTICE_TOTAL_MAX_BYTES: usize = 5 * 1024;
const WORKSPACE_PROTOCOL_CAP: usize = 8;
// Direct terminal gestures may use eight extra ordered slots under host pressure. Payloads in
// both queued and in-flight slots share the original eight MiB ceiling; no shadow payload exists.
const TERMINAL_PROTOCOL_PRESSURE_CAP: usize = 2 * WORKSPACE_PROTOCOL_CAP;
const TERMINAL_PROTOCOL_RETAINED_INPUT_MAX_BYTES: usize =
    WORKSPACE_PROTOCOL_CAP * WORKSPACE_PROTOCOL_INPUT_MAX_BYTES;
// Terminal paste accepts files/selections up to the existing 1 MiB clipboard ceiling. This is a
// local PTY byte stream, not the Connector/MCP tool-argument contract whose independent cap is
// 32 KiB.
const WORKSPACE_PROTOCOL_INPUT_MAX_BYTES: usize = 1024 * 1024;
const WORKSPACE_PROTOCOL_QUERY_MAX_BYTES: usize = 32 * 1024;
const WORKSPACE_PROTOCOL_ID_MAX_BYTES: usize = 128;
const WORKSPACE_PROTOCOL_SPLIT_PATH_MAX_ITEMS: usize = 256;
const WORKSPACE_PROTOCOL_SCROLLBACK_MAX_LINES: usize = 100_000;
const SESSION_PROJECT_NAME_MAX_ITEMS: usize = 256;
const SESSION_PROJECT_NAME_MAX_BYTES: usize = 1_024;
const SESSION_PROJECT_NAME_SNAPSHOT_MAX_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionProjectNameSnapshotError {
    InvalidRevision,
    TooManyItems,
    InvalidCwd,
    InvalidName,
    DuplicateSession,
    ByteBudgetExceeded,
}

#[derive(Clone)]
struct SessionProjectNameEntry {
    session: SessionId,
    cwd: Arc<str>,
    name: Arc<str>,
}

/// Bounded immutable projection of project display names computed by the App host.
///
/// Entries include the cwd used to compute the name so a late snapshot cannot label a session
/// after that shell has moved. Cloning the snapshot clones only one `Arc`; render performs a
/// binary-search lookup and never probes the filesystem.
#[derive(Clone, Default)]
pub struct SessionProjectNameSnapshot {
    revision: u64,
    entries: Arc<[SessionProjectNameEntry]>,
    retained_bytes: usize,
}

impl SessionProjectNameSnapshot {
    pub fn try_new(
        revision: u64,
        entries: Vec<(SessionId, String, String)>,
    ) -> Result<Self, SessionProjectNameSnapshotError> {
        if revision == 0 {
            return Err(SessionProjectNameSnapshotError::InvalidRevision);
        }
        if entries.len() > SESSION_PROJECT_NAME_MAX_ITEMS {
            return Err(SessionProjectNameSnapshotError::TooManyItems);
        }

        let mut retained_bytes = 0usize;
        let mut entries = entries
            .into_iter()
            .map(|(session, cwd, name)| {
                if cwd.is_empty()
                    || cwd.len() > WORKSPACE_PATH_MAX_BYTES
                    || cwd.as_bytes().contains(&0)
                {
                    return Err(SessionProjectNameSnapshotError::InvalidCwd);
                }
                if name.trim().is_empty()
                    || name.len() > SESSION_PROJECT_NAME_MAX_BYTES
                    || name.as_bytes().contains(&0)
                {
                    return Err(SessionProjectNameSnapshotError::InvalidName);
                }
                retained_bytes = retained_bytes
                    .checked_add(cwd.len())
                    .and_then(|bytes| bytes.checked_add(name.len()))
                    .ok_or(SessionProjectNameSnapshotError::ByteBudgetExceeded)?;
                if retained_bytes > SESSION_PROJECT_NAME_SNAPSHOT_MAX_BYTES {
                    return Err(SessionProjectNameSnapshotError::ByteBudgetExceeded);
                }
                Ok(SessionProjectNameEntry {
                    session,
                    cwd: cwd.into(),
                    name: name.into(),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        entries.sort_unstable_by_key(|entry| entry.session.0);
        if entries
            .windows(2)
            .any(|window| window[0].session == window[1].session)
        {
            return Err(SessionProjectNameSnapshotError::DuplicateSession);
        }
        Ok(Self {
            revision,
            entries: entries.into(),
            retained_bytes,
        })
    }

    fn project_name(&self, session: SessionId, cwd: &str) -> Option<&str> {
        let index = self
            .entries
            .binary_search_by_key(&session.0, |entry| entry.session.0)
            .ok()?;
        let entry = &self.entries[index];
        (entry.cwd.as_ref() == cwd).then_some(entry.name.as_ref())
    }

    fn retain_matching_cwds(&self, cwds: &HashMap<SessionId, String>) -> Self {
        self.filtered(|entry| {
            cwds.get(&entry.session)
                .is_some_and(|cwd| cwd == entry.cwd.as_ref())
        })
    }

    fn retain_live_sessions(&self, alive: &HashSet<SessionId>) -> Self {
        self.filtered(|entry| alive.contains(&entry.session))
    }

    fn filtered(&self, keep: impl Fn(&SessionProjectNameEntry) -> bool) -> Self {
        if self.entries.iter().all(&keep) {
            return self.clone();
        }
        let entries: Vec<_> = self
            .entries
            .iter()
            .filter(|entry| keep(entry))
            .cloned()
            .collect();
        let retained_bytes = entries
            .iter()
            .map(|entry| entry.cwd.len() + entry.name.len())
            .sum();
        Self {
            revision: self.revision,
            entries: entries.into(),
            retained_bytes,
        }
    }
}

impl fmt::Debug for SessionProjectNameSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionProjectNameSnapshot")
            .field("entries", &self.entries.len())
            .field("retained_bytes", &self.retained_bytes)
            .finish()
    }
}

/// Capacity-one, immutable request for the composition root to deliver through
/// the operating-system notification API. Workspace rendering only stages this
/// value; it never executes the native effect. Payload Debug is always redacted.
pub struct WorkspaceNotice {
    summary: Box<str>,
    body: Box<str>,
}

impl WorkspaceNotice {
    fn try_new(summary: String, body: &str) -> Option<Self> {
        if summary.is_empty()
            || summary.len() > WORKSPACE_NOTICE_SUMMARY_MAX_BYTES
            || body.len() > WORKSPACE_NOTICE_BODY_MAX_BYTES
            || summary.len().saturating_add(body.len()) > WORKSPACE_NOTICE_TOTAL_MAX_BYTES
            || summary.as_bytes().contains(&0)
            || body.as_bytes().contains(&0)
        {
            return None;
        }
        Some(Self {
            summary: summary.into(),
            body: body.into(),
        })
    }

    pub fn summary(&self) -> &str {
        &self.summary
    }

    pub fn body(&self) -> &str {
        &self.body
    }
}

impl fmt::Debug for WorkspaceNotice {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceNotice")
            .field("payload", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WorkspaceProtocolOperation(u64);

#[cfg(test)]
impl WorkspaceProtocolOperation {
    /// 테스트에서만 쓰는 생성자 — 필드가 비공개라 App 쪽 테스트가 프로토콜 경로를
    /// 재현할 수 없었다(2026-08-21).
    pub(crate) fn for_test(raw: u64) -> Self {
        Self(raw)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceProtocolErrorCode {
    Busy,
    InvalidCommand,
    PayloadTooLarge,
    DeliveryFailed,
}

/// One validated UI-to-runtime command. This value is non-Clone/non-Serialize and its Debug
/// implementation exposes only bounded correlation metadata and a low-cardinality command kind.
pub struct WorkspaceProtocolIntent {
    operation: WorkspaceProtocolOperation,
    generation: u64,
    command: RuntimeCommand,
    spawn_cwd: Option<String>,
}

impl WorkspaceProtocolIntent {
    pub fn operation(&self) -> WorkspaceProtocolOperation {
        self.operation
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn focus_pane(&self) -> Option<&runtime::MuxPaneId> {
        match &self.command {
            RuntimeCommand::FocusPane { pane } => Some(pane),
            _ => None,
        }
    }

    pub fn into_command(self) -> RuntimeCommand {
        self.command
    }
}

impl fmt::Debug for WorkspaceProtocolIntent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceProtocolIntent")
            .field("operation", &self.operation)
            .field("generation", &self.generation)
            .field("kind", &workspace_protocol_kind(&self.command))
            .finish()
    }
}

pub struct WorkspaceProtocolCompletion {
    pub operation: WorkspaceProtocolOperation,
    pub generation: u64,
    pub result: Result<(), WorkspaceProtocolErrorCode>,
}

struct PendingProtocolIntent {
    spawn: bool,
    spawn_cwd: Option<String>,
    input_bytes: usize,
}

enum PendingShellSpawn {
    Awaiting { cwd: Option<String> },
    CwdWritePending { session: SessionId, cwd: String },
}

fn workspace_protocol_kind(command: &RuntimeCommand) -> &'static str {
    match command {
        RuntimeCommand::SpawnShell { .. } => "spawn_shell",
        RuntimeCommand::WriteInput { .. } => "write_input",
        RuntimeCommand::Resize { .. } | RuntimeCommand::ResizeTracked { .. } => "resize",
        RuntimeCommand::Scroll { .. } => "scroll",
        RuntimeCommand::SplitPane { .. } => "split_pane",
        RuntimeCommand::ClosePane { .. } => "close_pane",
        RuntimeCommand::FocusPane { .. } => "focus_pane",
        RuntimeCommand::ResizeSplit { .. } => "resize_split",
        RuntimeCommand::SearchScrollback { .. } => "search_scrollback",
        RuntimeCommand::ScrollToBottom { .. } => "scroll_to_bottom",
        RuntimeCommand::ScrollToPrompt { .. } => "scroll_to_prompt",
        RuntimeCommand::ExtractLastOutput { .. } => "extract_last_output",
        _ => "invalid",
    }
}

fn protocol_input_bytes(command: &RuntimeCommand) -> usize {
    match command {
        RuntimeCommand::WriteInput { bytes, .. } => bytes.capacity(),
        _ => 0,
    }
}

pub(crate) fn terminal_protocol_command(command: &RuntimeCommand) -> bool {
    matches!(
        command,
        RuntimeCommand::WriteInput { .. }
            | RuntimeCommand::FocusPane { .. }
            | RuntimeCommand::Resize { .. }
            | RuntimeCommand::ResizeTracked { .. }
            | RuntimeCommand::ResizeSplit { .. }
            | RuntimeCommand::Scroll { .. }
            | RuntimeCommand::ScrollToBottom { .. }
            | RuntimeCommand::ScrollToPrompt { .. }
    )
}

fn workspace_protocol_command_is_valid(
    command: &RuntimeCommand,
) -> Result<(), WorkspaceProtocolErrorCode> {
    let id_is_valid = |id: &str| {
        !id.is_empty() && id.len() <= WORKSPACE_PROTOCOL_ID_MAX_BYTES && !id.as_bytes().contains(&0)
    };
    match command {
        RuntimeCommand::SpawnShell {
            cols,
            rows,
            scrollback_lines,
        } => {
            if *cols > 0
                && *rows > 0
                && *scrollback_lines <= WORKSPACE_PROTOCOL_SCROLLBACK_MAX_LINES
            {
                Ok(())
            } else {
                Err(WorkspaceProtocolErrorCode::InvalidCommand)
            }
        }
        RuntimeCommand::ResizeTracked {
            token, cols, rows, ..
        } => {
            if token.is_valid() && *cols > 0 && *rows > 0 {
                Ok(())
            } else {
                Err(WorkspaceProtocolErrorCode::InvalidCommand)
            }
        }
        RuntimeCommand::Resize { cols, rows, .. } => {
            if *cols > 0 && *rows > 0 {
                Ok(())
            } else {
                Err(WorkspaceProtocolErrorCode::InvalidCommand)
            }
        }
        RuntimeCommand::Scroll { .. }
        | RuntimeCommand::ScrollToBottom { .. }
        | RuntimeCommand::ExtractLastOutput { .. } => Ok(()),
        RuntimeCommand::WriteInput { bytes, .. } => {
            if bytes.len() <= WORKSPACE_PROTOCOL_INPUT_MAX_BYTES {
                Ok(())
            } else {
                Err(WorkspaceProtocolErrorCode::PayloadTooLarge)
            }
        }
        RuntimeCommand::SplitPane {
            pane,
            scrollback_lines,
            ..
        } => {
            if id_is_valid(&pane.0) && *scrollback_lines <= WORKSPACE_PROTOCOL_SCROLLBACK_MAX_LINES
            {
                Ok(())
            } else {
                Err(WorkspaceProtocolErrorCode::InvalidCommand)
            }
        }
        RuntimeCommand::ClosePane { pane } | RuntimeCommand::FocusPane { pane } => {
            if id_is_valid(&pane.0) {
                Ok(())
            } else {
                Err(WorkspaceProtocolErrorCode::InvalidCommand)
            }
        }
        RuntimeCommand::ResizeSplit {
            tab, path, ratio, ..
        } => {
            if id_is_valid(&tab.0)
                && path.len() <= WORKSPACE_PROTOCOL_SPLIT_PATH_MAX_ITEMS
                && path.iter().all(|part| *part <= 1)
                && ratio.is_finite()
                && (0.0..=1.0).contains(ratio)
            {
                Ok(())
            } else {
                Err(WorkspaceProtocolErrorCode::InvalidCommand)
            }
        }
        RuntimeCommand::SearchScrollback {
            query, max_matches, ..
        } => {
            if !query.is_empty()
                && query.len() <= WORKSPACE_PROTOCOL_QUERY_MAX_BYTES
                && !query.as_bytes().contains(&0)
                && (1..=SEARCH_MAX_MATCHES).contains(max_matches)
            {
                Ok(())
            } else if query.len() > WORKSPACE_PROTOCOL_QUERY_MAX_BYTES {
                Err(WorkspaceProtocolErrorCode::PayloadTooLarge)
            } else {
                Err(WorkspaceProtocolErrorCode::InvalidCommand)
            }
        }
        RuntimeCommand::ScrollToPrompt { direction, .. } => {
            if matches!(direction, -1 | 1) {
                Ok(())
            } else {
                Err(WorkspaceProtocolErrorCode::InvalidCommand)
            }
        }
        _ => Err(WorkspaceProtocolErrorCode::InvalidCommand),
    }
}

// 각 split leaf가 독립 터미널이 되는 패널형 구조. 헤더는 한 줄로 얇게 유지하고
// PTY는 외곽 카드 여백 없이 패널 면을 채운다.
/// pane 헤더(세션·문서 탭 줄)의 높이. 29 → 27pt (2026-10-05 사용자 요청).
/// 전체 앱 제목바와 별도로 탭 줄의 2pt를 본문에 돌려준다.
const TERMINAL_PANE_HEADER_HEIGHT: f32 = 27.0;
/// 각 terminal leaf가 분할 축에서 유지하는 최소 logical pixel 크기.
/// 좌/우 분할에는 너비, 상/하 분할에는 높이로 적용한다.
const TERMINAL_PANE_MIN_SIZE: f32 = 50.0;
/// pane 사이의 실제 구분선 폭. 최소 크기와 split 배치가 같은 값을 공유해야 한다.
const TERMINAL_SPLIT_GAP: f32 = 1.0;
const TERMINAL_STREAM_LEFT_PADDING: f32 = 3.0;
const TERMINAL_STREAM_RIGHT_PADDING: f32 = 3.0;
const TERMINAL_STREAM_VERTICAL_PADDING: f32 = 6.0;
const ARCHIVED_AGENT_NOTICE_HEIGHT: f32 = 36.0;

#[derive(Clone, Copy, Debug, PartialEq)]
struct ArchivedAgentNoticeStyle {
    fill: egui::Color32,
    separator: egui::Stroke,
    text: egui::Color32,
    button_fill: egui::Color32,
    button_stroke: egui::Stroke,
    button_height: f32,
    button_corner_radius: u8,
}

fn archived_agent_notice_style() -> ArchivedAgentNoticeStyle {
    ArchivedAgentNoticeStyle {
        fill: egui::Color32::from_rgb(0x1a, 0x1d, 0x23),
        separator: egui::Stroke::new(1.0, egui::Color32::from_rgb(0x35, 0x3b, 0x45)),
        text: egui::Color32::from_rgb(0xb0, 0xb5, 0xbf),
        button_fill: egui::Color32::from_rgb(0x29, 0x2e, 0x37),
        button_stroke: egui::Stroke::new(1.0, egui::Color32::from_rgb(0x48, 0x50, 0x5d)),
        button_height: 26.0,
        button_corner_radius: 4,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct PaneHeaderStyle {
    background: egui::Color32,
    selection_fill: Option<egui::Color32>,
    active_stroke: Option<egui::Stroke>,
}

impl PaneHeaderStyle {
    fn line_stroke(self, visuals: &egui::Visuals) -> egui::Stroke {
        self.active_stroke
            .unwrap_or_else(|| crate::ui::designall::separator_stroke(visuals))
    }
}

/// 채도를 `factor`배로 낮춘다 (0.0이면 무채색, 1.0이면 원본). luma 쪽으로 섞으므로
/// **밝기는 유지되고 채도만 빠진다** — 알파를 낮추는 것과 다르다. 알파를 낮추면 선이
/// 그냥 어두워져 신호가 약해지는데, "쨍하다"는 건 채도 문제이지 밝기 문제가 아니다
/// (2026-08-07 사용자).
fn desaturate(color: egui::Color32, factor: f32) -> egui::Color32 {
    let [red, green, blue, _] = color.to_array();
    let luma = 0.2126 * f32::from(red) + 0.7152 * f32::from(green) + 0.0722 * f32::from(blue);
    let mix =
        |channel: u8| (f32::from(channel) * factor + luma * (1.0 - factor)).clamp(0.0, 255.0) as u8;
    egui::Color32::from_rgb(mix(red), mix(green), mix(blue))
}

/// 포커스 상단선이 워크스페이스 고유색을 얼마나 살릴지. 팔레트가 주황 78%·보라 71%처럼
/// 채도가 높아 1px 선인데도 쨍하게 튀었다. 낮추면 워크스페이스별 선 무게 편차도 준다.
const PANE_HEADER_IDENTITY_SATURATION: f32 = 0.6;

/// pane 강조 플래시의 최대 알파. 예전에는 255(완전 불투명)에 풀 채도 시안이라
/// 2px 4변 면적에서 가장 세게 튀었다(2026-08-08 사용자). "쨍함"의 원인은 채도라
/// 채도를 0.6으로 낮추는 쪽으로 잡고, 알파는 놓치지 않을 만큼 남긴다 — 150은
/// 너무 약해 알림을 놓친다는 같은 날 피드백으로 200으로 올렸다.
const PANE_FLASH_PEAK_ALPHA: f32 = 200.0;

/// pane 전체 강조 플래시 색 — `remain`은 남은 비율(1.0 = 방금, 0.0 = 끝).
///
/// 예전에는 `selection.bg_fill`(풀 채도 시안)을 그대로 썼다. 같은 pane의 상단선은
/// 워크스페이스 고유색인데 플래시만 청록이라 둘이 따로 놀았고, 경계 드래그 라인과도
/// 같은 색이라 무엇이 반응한 건지 읽히지 않았다. 상단선과 **같은 채도 규칙**을 써서
/// "이 워크스페이스가 반응했다"로 읽히게 한다.
fn pane_flash_color(identity_color: egui::Color32, remain: f32) -> egui::Color32 {
    let accent = desaturate(identity_color, PANE_HEADER_IDENTITY_SATURATION);
    let alpha = (remain.clamp(0.0, 1.0) * PANE_FLASH_PEAK_ALPHA) as u8;
    egui::Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), alpha)
}

fn pane_header_style(identity_color: egui::Color32, focused: bool) -> PaneHeaderStyle {
    PaneHeaderStyle {
        // 탭 바는 터미널과 **같은 면**이다 — 그 pane의 제목이지 앱 크롬이 아니다.
        // app_background(사이드바·크롬과 같은 단)를 쓰던 동안에는 터미널 위에 밝은 띠가
        // 얹혀 pane이 두 조각으로 보였다(2026-08-07 사용자). 테마와 무관하게 항상 다크인
        // 면이라 tokens가 아니라 렌더러 상수를 그대로 쓴다.
        background: terminal::renderer_egui::TERMINAL_SURFACE_BG,
        selection_fill: None,
        // 포커스 상단선은 **그 pane이 속한 워크스페이스 색**이다. 예전에는 accent를
        // 썼는데, 경계 드래그 라인(widgets.active.bg_stroke)도 accent라 둘이 같은 청록으로
        // 보였다(2026-08-07 사용자). 같은 자리(pane 상단선)를 쓰는 cross-workspace 붙임
        // pane이 이미 워크스페이스 색을 쓰고 있으므로, 자기 워크스페이스 pane도 같은
        // 규칙으로 맞춘다 — 사이드바 아바타·세션 레일과도 색이 이어진다.
        active_stroke: focused.then_some(egui::Stroke::new(
            1.0,
            desaturate(identity_color, PANE_HEADER_IDENTITY_SATURATION).gamma_multiply(0.55),
        )),
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
#[allow(dead_code)]
struct PaneDropFeedbackStyle {
    outline: egui::Stroke,
    outline_inset: f32,
    insertion_width: f32,
    insertion_color: egui::Color32,
    label_fill: egui::Color32,
    label_text: egui::Color32,
}

#[allow(dead_code)]
fn pane_drop_feedback_style(tokens: crate::ui::designall::Tokens) -> PaneDropFeedbackStyle {
    PaneDropFeedbackStyle {
        outline: egui::Stroke::new(2.0, tokens.accent.gamma_multiply(0.72)),
        outline_inset: 2.0,
        insertion_width: 3.0,
        insertion_color: tokens.accent,
        label_fill: egui::Color32::from_rgb(0xed, 0x5b, 0x61),
        label_text: tokens.text,
    }
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
struct PaneDropFeedbackLabelLayout {
    rect: egui::Rect,
    galley: Arc<egui::Galley>,
}

#[allow(dead_code)]
fn layout_pane_drop_feedback_label(
    painter: &egui::Painter,
    pane_rect: egui::Rect,
    label: impl Into<String>,
    style: PaneDropFeedbackStyle,
) -> Option<PaneDropFeedbackLabelLayout> {
    let margin = 8.0;
    let padding = egui::vec2(6.0, 3.0);
    let max_text_width = pane_rect.width() - margin * 2.0 - padding.x * 2.0;
    if max_text_width <= 0.0 {
        return None;
    }

    let mut job = egui::text::LayoutJob::single_section(
        label.into(),
        egui::TextFormat {
            font_id: egui::FontId::proportional(13.0),
            color: style.label_text,
            ..Default::default()
        },
    );
    job.wrap = egui::text::TextWrapping {
        max_width: max_text_width,
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    let galley = painter.layout_job(job);
    if galley.size().x > max_text_width
        || galley.size().y + margin * 2.0 + padding.y * 2.0 > pane_rect.height()
    {
        return None;
    }

    let label_size = galley.size() + padding * 2.0;
    let rect = egui::Rect::from_center_size(pane_rect.center(), label_size);
    Some(PaneDropFeedbackLabelLayout { rect, galley })
}

#[allow(dead_code)]
pub(crate) fn paint_session_pane_drop_feedback(ui: &egui::Ui, rect: egui::Rect, label: &str) {
    let rect = rect.intersect(ui.clip_rect());
    if !rect.is_positive() {
        return;
    }

    let style = pane_drop_feedback_style(crate::ui::designall::tokens(ui.visuals()));
    let painter = ui.painter().with_clip_rect(rect);
    painter.rect_stroke(
        rect.shrink(style.outline_inset),
        0.0,
        style.outline,
        egui::StrokeKind::Inside,
    );
    painter.rect_filled(
        egui::Rect::from_min_max(
            egui::pos2(
                (rect.right() - style.insertion_width).max(rect.left()),
                rect.top(),
            ),
            rect.max,
        ),
        0.0,
        style.insertion_color,
    );

    if let Some(label) = layout_pane_drop_feedback_label(&painter, rect, label, style) {
        painter.rect_filled(label.rect, 3.0, style.label_fill);
        painter.galley(
            label.rect.min + egui::vec2(6.0, 3.0),
            label.galley,
            style.label_text,
        );
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct AttachedIdentityStyle {
    top_line: egui::Stroke,
    header_fill: egui::Color32,
    body_fill: egui::Color32,
}

fn attached_identity_style(identity_color: egui::Color32) -> AttachedIdentityStyle {
    AttachedIdentityStyle {
        top_line: egui::Stroke::new(1.0, identity_color),
        // 위 pane_header_style과 같은 이유로 터미널 작업면 색을 쓴다.
        header_fill: terminal::renderer_egui::TERMINAL_SURFACE_BG,
        body_fill: terminal::renderer_egui::TERMINAL_SURFACE_BG,
    }
}

/// 헤더 상단선의 y — 선이 헤더 **첫 물리행부터** 덮도록 놓는다.
///
/// egui의 `round_to_pixel_center`는 문서대로 홀수 물리픽셀 폭 전용이다. 이 선은
/// 1.0 포인트라 Retina에서 2 물리픽셀(짝수)이라, 픽셀 중심에 맞추면 양끝이 반 픽셀씩
/// 걸쳐 뭉개지고 `header.top()`이 소수일 땐 첫 행이 비어 1px 여백처럼 보인다.
/// 짝수 폭은 픽셀 **경계**에 맞춘 뒤 half-width를 더해야 한다.
///
/// 경계는 반올림이 아니라 **내림**이다. header_top이 물리픽셀 중간에 걸릴 때 반올림이
/// 위로 가면 그만큼 헤더 안쪽에 빈 띠가 남는다 — 살짝 위로 겹치는 쪽이 낫다.
fn pane_header_top_line_y(header_top: f32, stroke_width: f32, pixels_per_point: f32) -> f32 {
    crate::ui::snap_edge_line_to_pixel(header_top, stroke_width, pixels_per_point)
}

fn pane_header_active_boundary(header: egui::Rect, close: egui::Rect) -> f32 {
    (close.right() + 6.0).min(header.right()).max(header.left())
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct TerminalPaneLayout {
    header: egui::Rect,
    surface: egui::Rect,
    content: egui::Rect,
    archived_notice: Option<egui::Rect>,
}

#[cfg(test)]
fn terminal_pane_layout(rect: egui::Rect) -> TerminalPaneLayout {
    terminal_pane_layout_with_embedded_header(rect, true)
}

#[cfg(test)]
fn terminal_pane_layout_with_embedded_header(
    rect: egui::Rect,
    embedded_header: bool,
) -> TerminalPaneLayout {
    terminal_pane_layout_for_state(rect, embedded_header, false)
}

fn terminal_pane_layout_for_state(
    rect: egui::Rect,
    embedded_header: bool,
    show_archived_notice: bool,
) -> TerminalPaneLayout {
    let header_height = if embedded_header {
        TERMINAL_PANE_HEADER_HEIGHT.min(rect.height().max(0.0) * 0.5)
    } else {
        0.0
    };
    let header = egui::Rect::from_min_max(
        rect.min,
        egui::pos2(rect.right(), rect.top() + header_height),
    );
    let surface = egui::Rect::from_min_max(egui::pos2(rect.left(), header.bottom()), rect.max);
    let archived_notice = show_archived_notice.then(|| {
        let height = ARCHIVED_AGENT_NOTICE_HEIGHT.min(surface.height().max(0.0) * 0.5);
        egui::Rect::from_min_max(
            egui::pos2(surface.left(), surface.bottom() - height),
            surface.max,
        )
    });
    let terminal_surface = egui::Rect::from_min_max(
        surface.min,
        egui::pos2(
            surface.right(),
            archived_notice.map_or(surface.bottom(), |notice| notice.top()),
        ),
    );
    let pad_left = TERMINAL_STREAM_LEFT_PADDING.min(terminal_surface.width().max(0.0) * 0.25);
    let pad_right = TERMINAL_STREAM_RIGHT_PADDING.min(terminal_surface.width().max(0.0) * 0.25);
    let pad_y = TERMINAL_STREAM_VERTICAL_PADDING.min(terminal_surface.height().max(0.0) * 0.25);
    let content = egui::Rect::from_min_max(
        egui::pos2(
            terminal_surface.left() + pad_left,
            terminal_surface.top() + pad_y,
        ),
        egui::pos2(
            terminal_surface.right() - pad_right,
            terminal_surface.bottom() - pad_y,
        ),
    );
    TerminalPaneLayout {
        header,
        surface,
        content,
        archived_notice,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct TerminalLayoutMetric {
    min_size: egui::Vec2,
    /// preorder 벡터에서 이 node와 모든 descendant가 차지하는 항목 수.
    subtree_len: usize,
}

/// layout 전체의 minimum을 재귀 반환값으로 한 번만 계산해 preorder 벡터에 기록한다.
/// render도 같은 순서로 순회하므로 HashMap이나 node별 재귀 재계산 없이 자식 metric을
/// O(1)에 찾는다. 기존의 매-frame `LayoutNode::clone`도 이 compact 벡터로 대체한다.
fn terminal_layout_metrics(node: &LayoutNode) -> Vec<TerminalLayoutMetric> {
    fn append(node: &LayoutNode, metrics: &mut Vec<TerminalLayoutMetric>) -> TerminalLayoutMetric {
        let index = metrics.len();
        metrics.push(TerminalLayoutMetric {
            min_size: egui::Vec2::ZERO,
            subtree_len: 1,
        });

        let metric = match node {
            LayoutNode::Pane(_) => TerminalLayoutMetric {
                min_size: egui::vec2(TERMINAL_PANE_MIN_SIZE, TERMINAL_PANE_MIN_SIZE),
                subtree_len: 1,
            },
            LayoutNode::Split {
                direction,
                first,
                second,
                ..
            } => {
                let first = append(first, metrics);
                let second = append(second, metrics);
                let min_size = match direction {
                    SplitDirection::Horizontal => egui::vec2(
                        first.min_size.x + TERMINAL_SPLIT_GAP + second.min_size.x,
                        first.min_size.y.max(second.min_size.y),
                    ),
                    SplitDirection::Vertical => egui::vec2(
                        first.min_size.x.max(second.min_size.x),
                        first.min_size.y + TERMINAL_SPLIT_GAP + second.min_size.y,
                    ),
                };
                TerminalLayoutMetric {
                    min_size,
                    subtree_len: 1 + first.subtree_len + second.subtree_len,
                }
            }
        };
        metrics[index] = metric;
        metric
    }

    let mut metrics = Vec::new();
    append(node, &mut metrics);
    metrics
}

#[cfg(test)]
fn terminal_layout_min_size(node: &LayoutNode) -> egui::Vec2 {
    terminal_layout_metrics(node)[0].min_size
}

fn terminal_split_gap(rect: egui::Rect, direction: SplitDirection) -> f32 {
    let axis_extent = match direction {
        SplitDirection::Horizontal => rect.width(),
        SplitDirection::Vertical => rect.height(),
    };
    if axis_extent <= TERMINAL_SPLIT_GAP {
        axis_extent.max(0.0)
    } else {
        TERMINAL_SPLIT_GAP
    }
}

/// 저장 ratio와 드래그 preview를 현재 rect의 실제 px minimum으로 제한한다.
///
/// 창이 subtree minimum보다 작으면 50px 보장은 물리적으로 불가능하다. 이 경우 한쪽을
/// 임의로 굶기지 않고 두 subtree가 요구하는 크기에 비례해 가용 공간을 나눈다.
fn terminal_split_ratio(
    rect: egui::Rect,
    direction: SplitDirection,
    requested_ratio: f32,
    first_min: egui::Vec2,
    second_min: egui::Vec2,
) -> f32 {
    let (axis_extent, first_min, second_min) = match direction {
        SplitDirection::Horizontal => (rect.width(), first_min.x, second_min.x),
        SplitDirection::Vertical => (rect.height(), first_min.y, second_min.y),
    };
    let available = (axis_extent - terminal_split_gap(rect, direction)).max(0.0);
    let required = first_min + second_min;

    if available < required {
        return first_min / required;
    }

    let requested_ratio = if requested_ratio.is_finite() {
        requested_ratio
    } else {
        0.5
    };
    let min_ratio = first_min / available;
    let max_ratio = 1.0 - second_min / available;
    if min_ratio >= max_ratio {
        // available == required인 비대칭 subtree는 서로 다른 f32 연산 반올림 때문에
        // min_ratio가 max_ratio보다 1 ULP 커질 수 있다. f32::clamp는 그때 panic하므로
        // 정확한 minimum 비율로 수렴시킨다.
        return first_min / required;
    }
    requested_ratio.clamp(min_ratio, max_ratio)
}

fn terminal_split_rects(
    rect: egui::Rect,
    direction: SplitDirection,
    ratio: f32,
) -> (egui::Rect, egui::Rect, egui::Rect) {
    let gap = terminal_split_gap(rect, direction);
    let ratio = if ratio.is_finite() {
        ratio.clamp(0.0, 1.0)
    } else {
        0.5
    };

    match direction {
        SplitDirection::Horizontal => {
            let split_x = rect.min.x + (rect.width() - gap).max(0.0) * ratio;
            (
                egui::Rect::from_min_max(rect.min, egui::pos2(split_x, rect.max.y)),
                egui::Rect::from_min_max(egui::pos2(split_x + gap, rect.min.y), rect.max),
                egui::Rect::from_min_max(
                    egui::pos2(split_x, rect.min.y),
                    egui::pos2(split_x + gap, rect.max.y),
                ),
            )
        }
        SplitDirection::Vertical => {
            let split_y = rect.min.y + (rect.height() - gap).max(0.0) * ratio;
            (
                egui::Rect::from_min_max(rect.min, egui::pos2(rect.max.x, split_y)),
                egui::Rect::from_min_max(egui::pos2(rect.min.x, split_y + gap), rect.max),
                egui::Rect::from_min_max(
                    egui::pos2(rect.min.x, split_y),
                    egui::pos2(rect.max.x, split_y + gap),
                ),
            )
        }
    }
}

fn terminal_split_hit_rect(
    parent: egui::Rect,
    gap_rect: egui::Rect,
    direction: SplitDirection,
) -> egui::Rect {
    gap_rect
        .expand2(match direction {
            SplitDirection::Horizontal => egui::vec2(2.0, 0.0),
            SplitDirection::Vertical => egui::vec2(0.0, 2.0),
        })
        .intersect(parent)
}

fn split_handle_id(tab: &runtime::MuxTabId, path: &[u8]) -> egui::Id {
    egui::Id::new(("split_handle", tab, path))
}

/// layout 트리의 pane을 배치 순서대로 모은다.
fn layout_panes<'a>(node: &'a LayoutNode, out: &mut Vec<&'a runtime::MuxPaneId>) {
    match node {
        LayoutNode::Pane(id) => out.push(id),
        LayoutNode::Split { first, second, .. } => {
            layout_panes(first, out);
            layout_panes(second, out);
        }
    }
}

/// 보조 탭을 붙일 pane — focused pane이 이 layout 안에 있으면 그것, 없으면 첫 pane.
fn aux_tab_owner_pane(
    layout: &LayoutNode,
    pending: Option<&runtime::MuxPaneId>,
    focused: Option<&runtime::MuxPaneId>,
) -> Option<runtime::MuxPaneId> {
    let mut panes = Vec::new();
    layout_panes(layout, &mut panes);
    pending
        .filter(|id| panes.contains(id))
        .or_else(|| focused.filter(|id| panes.contains(id)))
        .or_else(|| panes.first().copied())
        .cloned()
}

fn keeps_embedded_pane_header(_layout: &LayoutNode) -> bool {
    true
}

/// pane 헤더 우측 도구 버튼 한 변(정사각)과 간격 — pane_header_buttons와
/// render_pane_header의 제목 폭 계산이 공유하는 단일 원천.
const PANE_HEADER_TOOLBAR_BUTTON: f32 = 20.0;
const PANE_HEADER_TOOLBAR_GAP: f32 = 2.0;
/// 제목의 좌측 원점. 예전 17px는 앞의 포커스 점(중심 8, 반지름 4)을 피한 값이었다 —
/// 점을 지웠으니 그 자리를 되돌린다. pane_header_buttons와 render_pane_header가
/// **같은 값**을 써야 닫기 버튼 위치와 제목 폭 계산이 어긋나지 않는다.
/// 헤더 탭 제목의 왼쪽 들여쓰기. 문서 툴바가 이 값에 자기 라벨을 맞춘다
/// (`ui::document::toolbar`) — 둘이 따로 놀면 툴바가 헤더와 어긋나 보인다.
pub(crate) const PANE_HEADER_TITLE_LEFT: f32 = 10.0;

/// 문서 탭 하나를 식별하는 안정 id(멀티 문서 탭 설계 §1) — 헤더에서의 위치(인덱스)가
/// 아니다. 인덱스는 탭이 닫히면 밀려서 조용히 어긋난다. App이 한 번 배정하면 그
/// 문서가 열려 있는 동안 바뀌지 않고, 닫힌 뒤에도 재사용하지 않는다(닫힌 문서의
/// 지연 IO 결과가 같은 id를 재사용한 새 문서에 잘못 적용되는 걸 막는다).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DocumentTabId(pub u32);

/// 보조 탭 종류 — 헤더에 붙는 순서이자 hover 문구 키의 근거다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneAuxTabKind {
    History,
    Git,
    /// md·txt 등 문서를 pane 본문 전체에 연다(설계 §1) — 이력·Git과 같은 기구를 쓴다.
    /// 문서는 여러 개를 동시에 열 수 있어(멀티 문서 탭 설계) id로 어느 것인지 구분한다.
    Document(DocumentTabId),
}

impl PaneAuxTabKind {
    fn hint_key(self) -> &'static str {
        match self {
            Self::History => "workspace.tab.history_hint",
            Self::Git => "workspace.tab.git_hint",
            Self::Document(_) => "workspace.tab.document_hint",
        }
    }

    fn close_key(self) -> &'static str {
        match self {
            Self::History => "workspace.tab.history_close",
            Self::Git => "workspace.tab.git_close",
            Self::Document(_) => "workspace.tab.document_close",
        }
    }
}

/// 세션 헤더 옆에 붙는 보조 탭의 표시 상태 — App이 소유하고 매 프레임 넘긴다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaneAuxTab {
    pub kind: PaneAuxTabKind,
    pub label: String,
    pub active: bool,
}

/// 보조 탭이 App으로 올려보내는 의도. 여기서 파생되는 `RuntimeCommand`는 없다 —
/// 세션 X(`ClosePane`)와 이력 X는 끝까지 다른 동작이어야 한다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneAuxTabIntent {
    /// 보조 탭을 눌렀다 — 보조 본문을 활성화한다.
    Activate,
    /// 보조 탭이 활성인 동안 세션 탭을 눌렀다 — 터미널로 돌아가되 탭은 남긴다.
    ShowSession,
    /// 보조 탭 X — UI 탭만 닫는다.
    Close,
}

/// 현재 세션 pane 헤더 옆에 붙는 **보조 UI 탭**의 상태.
///
/// runtime의 `MuxTabId`/pane과 무관하다 — 이 상태가 바뀌어도 PTY·세션·mux 탭은
/// 생성되거나 종료되지 않는다. 세션 X와 보조 탭 X가 서로 다른 동작인 이유다.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PaneAuxTabState {
    #[default]
    Closed,
    OpenInactive,
    OpenActive,
}

impl PaneAuxTabState {
    /// 탭 chrome이 헤더에 존재하는지. `Closed`면 세션 헤더는 예전 그대로다.
    pub fn is_open(self) -> bool {
        self != Self::Closed
    }

    pub fn is_active(self) -> bool {
        self == Self::OpenActive
    }

    /// 레일 「보조 탭」 클릭 — 닫혀 있으면 열고 활성화, 이미 활성이면 세션으로 돌아가되
    /// 탭은 남긴다.
    pub fn on_rail_click(self) -> Self {
        match self {
            Self::Closed | Self::OpenInactive => Self::OpenActive,
            Self::OpenActive => Self::OpenInactive,
        }
    }

    /// 보조 탭 클릭 — 열려 있을 때만 활성화한다.
    pub fn on_tab_click(self) -> Self {
        match self {
            Self::Closed => Self::Closed,
            Self::OpenInactive | Self::OpenActive => Self::OpenActive,
        }
    }

    /// 세션 탭 클릭 — 터미널을 보여주되 보조 탭은 유지한다.
    pub fn on_session_tab_click(self) -> Self {
        match self {
            Self::Closed => Self::Closed,
            Self::OpenInactive | Self::OpenActive => Self::OpenInactive,
        }
    }

    /// 보조 탭 X — UI 탭만 제거한다. 세션에는 어떤 종료 명령도 보내지 않는다.
    pub fn on_close(self) -> Self {
        Self::Closed
    }
}

/// 보조 탭 라벨 좌측 여백·라벨과 닫기 중심 간격·닫기 뒤 여백. 세션 탭이 쓰는 값과
/// 같은 규칙(제목 10pt 들여쓰기, 제목 끝 +14pt에 닫기 중심, 닫기 뒤 6pt)이라 두 탭의
/// 리듬이 어긋나지 않는다.
const PANE_AUX_TAB_LABEL_LEFT: f32 = 10.0;
const PANE_AUX_TAB_CLOSE_GAP: f32 = 14.0;
const PANE_AUX_TAB_RIGHT_PAD: f32 = 6.0;
const PANE_AUX_TAB_CLOSE_SIZE: f32 = 20.0;
/// 헤더가 아무리 좁아도 보조 탭 라벨에 남기는 최소 폭 — 0폭 라벨을 만들지 않는다.
const PANE_AUX_TAB_MIN_LABEL: f32 = 14.0;

/// 라벨 폭에서 보조 탭이 헤더에서 차지하는 전체 폭.
fn pane_aux_tab_width(label_width: f32) -> f32 {
    PANE_AUX_TAB_LABEL_LEFT
        + label_width
        + PANE_AUX_TAB_CLOSE_GAP
        + PANE_AUX_TAB_CLOSE_SIZE * 0.5
        + PANE_AUX_TAB_RIGHT_PAD
}

/// 보조 탭이 헤더 절반을 넘지 않게 라벨 폭을 자른다. 좁은 폭 우선순위 1은 **세션 제목
/// 최소 폭**이라, 보조 탭이 먼저 양보한다. 탭이 여럿이면 절반 예산을 탭 수로 나눠 쓴다
/// (탭 1개일 때는 `count == 1`이라 기존 계산과 정확히 같다).
fn pane_aux_tab_label_width(header_width: f32, natural_label_width: f32, tab_count: usize) -> f32 {
    let count = tab_count.max(1) as f32;
    let budget = (header_width * 0.5 / count - pane_aux_tab_width(0.0)).max(PANE_AUX_TAB_MIN_LABEL);
    natural_label_width.max(0.0).min(budget)
}

/// 보조 탭 기하 — 탭 본체와 닫기가 각각 독립 히트박스를 갖는다.
#[derive(Clone, Copy, Debug, PartialEq)]
struct PaneAuxTabGeometry {
    /// 탭 전체(활성화 클릭 대상). 닫기는 이 위에 **나중에** 등록해 우선권을 갖는다.
    tab: egui::Rect,
    label_left: f32,
    label_width: f32,
    /// 남은 폭이 모자라면 없다 — 라벨(=탭 전환)이 닫기보다 우선한다.
    close: Option<egui::Rect>,
}

/// 세션 닫기(×) 오른쪽에 보조 탭을 놓는다 — 시작점은 세션 탭의 accent 경계(또는 그
/// 앞에 이미 놓인 보조 탭의 오른쪽 끝)다. 여러 탭을 이어 붙일 때는 `left`를 호출부가
/// 직접 관리한다(`layout_aux_tabs` 참고).
///
/// 좁은 폭에서는 순서대로 양보한다: `include_close`가 꺼져 있으면(호출부의 축약
/// 우선순위 판단) 애초에 닫기를 만들지 않는다. `include_close`가 켜져 있어도 자리가
/// 없으면(다음 탭이 밀려 들어와 이 탭 몫이 줄었을 때) 닫기만 접는다. 최소 라벨 폭조차
/// 없으면 탭 자체를 만들지 않는다(레일로 계속 전환할 수 있다). 어떤 경우에도
/// `toolbar_left`나 헤더 오른쪽 끝을 넘지 않는다.
fn pane_aux_tab_geometry(
    header: egui::Rect,
    left: f32,
    toolbar_left: f32,
    label_width: f32,
    include_close: bool,
) -> Option<PaneAuxTabGeometry> {
    let center_y = header.center().y;
    let limit = toolbar_left.min(header.right());
    let label_left = left + PANE_AUX_TAB_LABEL_LEFT;
    let label_width = label_width.min(limit - PANE_AUX_TAB_RIGHT_PAD - label_left);
    if label_width < PANE_AUX_TAB_MIN_LABEL {
        return None;
    }
    let close_center_x = label_left + label_width + PANE_AUX_TAB_CLOSE_GAP;
    let close = (include_close
        && close_center_x + PANE_AUX_TAB_CLOSE_SIZE * 0.5 + PANE_AUX_TAB_RIGHT_PAD <= limit)
        .then(|| {
            egui::Rect::from_center_size(
                egui::pos2(close_center_x, center_y),
                egui::vec2(PANE_AUX_TAB_CLOSE_SIZE, PANE_AUX_TAB_CLOSE_SIZE),
            )
        });
    let right =
        close.map_or(label_left + label_width, |rect| rect.right()) + PANE_AUX_TAB_RIGHT_PAD;
    Some(PaneAuxTabGeometry {
        tab: egui::Rect::from_min_max(
            egui::pos2(left, header.top()),
            egui::pos2(right.min(limit), header.bottom()),
        ),
        label_left,
        label_width,
        close,
    })
}

/// 배치된 보조 탭 하나 — 기하까지 계산이 끝난 뒤의 결과다.
#[derive(Clone, Debug, PartialEq)]
struct AuxTabPlacement {
    kind: PaneAuxTabKind,
    label: String,
    active: bool,
    geometry: PaneAuxTabGeometry,
}

/// 비활성 탭만, 축약 우선순위(멀티 문서 탭 설계 §5)로 나열한다 — 비활성 문서를
/// 오른쪽(탭 스트립에서 나중에 오는 것)부터, 그다음 Git(비활성이면), 그다음
/// 이력(비활성이면). **활성 탭은 이 목록에 절대 포함되지 않는다** — ⓑ(탭을 통째로
/// 빼기)가 이 순서를 그대로 쓰므로, 활성 탭은 탭 자체가 빠지는 일이 없다(새 불변식).
/// 문서가 먼저인 이유는 그대로다: 파일명이 라벨이라 길고, 닫기가 툴바에도 있다.
fn aux_tab_inactive_priority(tabs: &[&PaneAuxTab]) -> Vec<PaneAuxTabKind> {
    let mut order: Vec<PaneAuxTabKind> = tabs
        .iter()
        .rev()
        .filter(|tab| !tab.active && matches!(tab.kind, PaneAuxTabKind::Document(_)))
        .map(|tab| tab.kind)
        .collect();
    for target in [PaneAuxTabKind::Git, PaneAuxTabKind::History] {
        if let Some(tab) = tabs.iter().find(|tab| !tab.active && tab.kind == target) {
            order.push(tab.kind);
        }
    }
    order
}

/// ⓐ(×부터 빼기) 순서 — `aux_tab_inactive_priority` 뒤에 활성 탭을 하나 더 얹는다.
/// **활성 탭은 언제나 맨 마지막**이라 ×가 가장 늦게 접힌다(새 불변식: 활성 탭은
/// 절대 버리지 않는다 — ⓐ 단계에서는 ×는 잃을 수 있지만 탭 자체는 ⓑ에서도 살아남는다).
fn aux_tab_strip_priority(tabs: &[&PaneAuxTab]) -> Vec<PaneAuxTabKind> {
    let mut order = aux_tab_inactive_priority(tabs);
    if let Some(tab) = tabs.iter().find(|tab| tab.active) {
        order.push(tab.kind);
    }
    order
}

/// 주어진 탭 집합을 세션 ×의 accent 경계에서 시작해 왼→오로 배치해본다. `stripped`에
/// 속한 종류는 처음부터 ×를 만들지 않는다. 하나라도 최소 라벨 폭을 못 채우면 이
/// 시도 전체가 실패다(`None`) — 호출부가 다음 축약 단계로 넘어간다.
///
/// `stripped`에 없는 탭인데도 `pane_aux_tab_geometry`가 자리가 없어 ×를 자체적으로
/// 접었다면(뒤에 놓인 탭일수록 room이 먼저 바닥난다) 이 시도도 실패로 친다 — 그러지
/// 않으면 우선순위(축약 순서 ⓐ)를 무시하고 "포지션상 뒤에 있다"는 이유만으로
/// 엉뚱한 탭의 ×가 먼저 사라진다(활성 탭이 우연히 뒤쪽에 있으면 새 불변식이 깨진다).
fn try_layout_aux_tabs(
    header: egui::Rect,
    session_close: egui::Rect,
    toolbar_left: f32,
    visible: &[&PaneAuxTab],
    stripped: &[PaneAuxTabKind],
    natural_width: &impl Fn(&str) -> f32,
) -> Option<Vec<AuxTabPlacement>> {
    let mut left = pane_header_active_boundary(header, session_close);
    let mut out = Vec::with_capacity(visible.len());
    for tab in visible {
        let width =
            pane_aux_tab_label_width(header.width(), natural_width(&tab.label), visible.len());
        let include_close = !stripped.contains(&tab.kind);
        let geometry = pane_aux_tab_geometry(header, left, toolbar_left, width, include_close)?;
        if include_close && geometry.close.is_none() {
            return None;
        }
        left = geometry.tab.right();
        out.push(AuxTabPlacement {
            kind: tab.kind,
            label: tab.label.clone(),
            active: tab.active,
            geometry,
        });
    }
    Some(out)
}

/// 세션 ×의 accent 경계에서 시작해 왼→오로 이어 붙인다. 좁을 때의 축약 순서(멀티
/// 문서 탭 설계 §5, 새 불변식 — **활성 탭은 절대 버리지 않는다**):
/// ⓐ 비활성 문서(오른쪽부터) → Git → 이력 → 활성 탭 순으로 ×를 뺀다 → ⓑ 그래도
/// 모자라면 같은 순서(활성 탭 제외)로 탭 자체를 뺀다 → ⓒ 활성 탭과 세션 제목은
/// 마지막까지 남는다(탭이 하나도 안 들어가도 세션 헤더는 그대로다). 탭 개수에
/// 고정 상한이 없다 — App이 문서 탭 개수를 이미 유계로 관리한다.
fn layout_aux_tabs(
    header: egui::Rect,
    session_close: egui::Rect,
    toolbar_left: f32,
    tabs: &[PaneAuxTab],
    natural_width: impl Fn(&str) -> f32,
) -> Vec<AuxTabPlacement> {
    let mut visible: Vec<&PaneAuxTab> = tabs.iter().collect();
    loop {
        if visible.is_empty() {
            return Vec::new();
        }
        let strip_order = aux_tab_strip_priority(&visible);
        for strip in 0..=strip_order.len() {
            let stripped = &strip_order[..strip];
            if let Some(placements) = try_layout_aux_tabs(
                header,
                session_close,
                toolbar_left,
                &visible,
                stripped,
                &natural_width,
            ) {
                return placements;
            }
        }
        // ×를 전부 빼도 안 맞는다 — 비활성 탭 중 우선순위 맨 앞을 통째로 뺀다. 활성
        // 탭은 `aux_tab_inactive_priority`에 아예 없으므로 여기서 뽑힐 수 없다.
        let Some(drop_kind) = aux_tab_inactive_priority(&visible).first().copied() else {
            // 남은 게 활성 탭 하나뿐인데 그마저 안 들어간다 — 더 뺄 게 없다.
            return Vec::new();
        };
        visible.retain(|tab| tab.kind != drop_kind);
    }
}

/// 닫기(×) 글리프. 세션 닫기와 보조 탭 닫기가 같은 모양을 쓰되 **색만** 다르다
/// (세션은 error 톤, 보조 탭은 중립 — 세션을 끝내지 않기 때문).
fn paint_close_glyph(painter: &egui::Painter, center: egui::Pos2, color: egui::Color32) {
    let d = 4.0;
    painter.line_segment(
        [center + egui::vec2(-d, -d), center + egui::vec2(d, d)],
        egui::Stroke::new(1.5, color),
    );
    painter.line_segment(
        [center + egui::vec2(-d, d), center + egui::vec2(d, -d)],
        egui::Stroke::new(1.5, color),
    );
}

/// 탭 라벨 한 줄 — 넘치면 '…'로 줄인다. 세션 제목과 보조 탭 라벨의 단일 원천.
fn paint_tab_label(
    painter: &egui::Painter,
    clip: egui::Rect,
    center_y: f32,
    font: egui::FontId,
    color: egui::Color32,
    text: String,
) {
    let mut job = egui::text::LayoutJob::single_section(
        text,
        egui::TextFormat {
            font_id: font,
            color,
            ..Default::default()
        },
    );
    job.wrap = egui::text::TextWrapping {
        max_width: clip.width().max(0.0),
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    let galley = painter.layout_job(job);
    painter.with_clip_rect(clip).galley(
        egui::pos2(clip.left(), center_y - galley.size().y / 2.0),
        galley,
        color,
    );
}

/// 헤더 바탕 — 배경, 선택 탭 위의 accent 상단선, 하단 separator.
///
/// `accent_range`가 없으면 상단선을 그리지 않는다(비포커스 pane).
fn paint_pane_header_base(
    ui: &egui::Ui,
    header: egui::Rect,
    style: PaneHeaderStyle,
    accent_range: Option<egui::Rangef>,
) {
    ui.painter().rect_filled(header, 0.0, style.background);
    if let Some(selection_fill) = style.selection_fill {
        ui.painter().rect_filled(header, 0.0, selection_fill);
    }
    if let (Some(active_stroke), Some(range)) = (style.active_stroke, accent_range) {
        // round_to_pixel_center는 문서가 밝히듯 **홀수 물리픽셀 폭**용이다. 이 선은
        // 1.0 **포인트**라 Retina에서 2 물리픽셀(짝수)이므로, 픽셀 중심에 맞추면
        // 양끝이 반 픽셀씩 걸쳐 뭉개지고 header.top()이 소수일 땐 헤더 첫 행이 아예
        // 비어 1px 여백으로 보인다(2026-08-07 사용자).
        //
        // 짝수 폭은 **경계**에 맞춰야 한다 — 헤더 상단을 픽셀 격자에 스냅한 뒤
        // half-width를 더하면 선이 첫 행부터 정확히 덮는다.
        let top_y = pane_header_top_line_y(
            header.top(),
            active_stroke.width,
            ui.ctx().pixels_per_point(),
        );
        let range = egui::Rangef::new(
            range.min,
            crate::ui::snap_line_to_pixel(
                range.max,
                active_stroke.width,
                ui.ctx().pixels_per_point(),
            ),
        );
        ui.painter().hline(range, top_y, active_stroke);
    }
    // 헤더와 본문 사이 하단 구분선은 긋지 않는다 — 탭 면과 터미널이 한 덩어리로
    // 이어져 보여야 한다(2026-08-22 사용자 요청).
}

/// 두 탭 사이 세로 헤어라인 — 같은 배경을 쓰는 두 영역의 경계를 읽히게 한다.
fn paint_tab_divider(ui: &egui::Ui, header: egui::Rect, x: f32, style: PaneHeaderStyle) {
    if x <= header.left() || x >= header.right() {
        return;
    }
    let stroke = style.line_stroke(ui.visuals());
    ui.painter().vline(
        crate::ui::snap_line_to_pixel(x, stroke.width, ui.ctx().pixels_per_point()),
        egui::Rangef::new(
            pane_header_top_line_y(header.top(), stroke.width, ui.ctx().pixels_per_point()),
            header.bottom(),
        ),
        stroke,
    );
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NewSessionRequest {
    NewTab,
    SplitRight(runtime::MuxPaneId),
}

/// pane 헤더 버튼 기하 — 닫기(×)는 마지막까지 남는 버튼이다.
struct PaneHeaderButtons {
    /// 닫기(×) 히트박스. 항상 존재한다.
    close: egui::Rect,
    /// 표시할 우측 도구 히트박스(왼쪽→오른쪽). 아이콘은 전체 목록의 뒤에서부터
    /// `toolbar.len()`개를 대응시킨다 (왼쪽 도구부터 숨김).
    toolbar: Vec<egui::Rect>,
    /// 도구 영역의 왼쪽 경계 — 보조 탭이 넘으면 안 되는 선. 도구가 0개면 헤더 오른쪽
    /// 여백(4pt) 자리다.
    toolbar_left: f32,
}

/// 헤더 폭·제목 폭으로 닫기(×)와 우측 도구의 히트박스를 계산한다.
///
/// codex 리뷰 P2 회귀 가드: compact 헤더(3e3e909)는 visible_toolbar를
/// clamp(1,·)로 최소 1개 강제해 당시 최소 pane(≈59px)에서 Split
/// 버튼이 닫기 히트박스 22px 중 17px를 덮었고, 도구 interact가 나중에
/// 등록되므로 겹침 클릭이 닫기 대신 분할을 실행했다. 지금은 도구 0개를
/// 허용하고, 만에 하나 기하가 어긋나 도구가 닫기를 덮으면 왼쪽 도구를 더
/// 숨겨 닫기가 항상 우선하도록 보장한다.
/// `aux_width`는 헤더 오른쪽 도구 앞에 보조 탭이 미리 잡아둔 폭이다. 0이면 보조 탭이
/// 없던 시절과 정확히 같은 기하가 나온다.
fn pane_header_buttons(
    header: egui::Rect,
    title_width: f32,
    icon_count: usize,
    aux_width: f32,
) -> PaneHeaderButtons {
    let title_left = header.left() + PANE_HEADER_TITLE_LEFT;
    let center_y = header.center().y;
    // 제목과 닫기 버튼을 먼저 온전히 확보한다. 분할 pane이 좁아지면 우측 도구를
    // 왼쪽부터 단계적으로 숨겨(0개 허용) 제목 글자가 중간에서 잘리는 일을 막는다.
    let toolbar_available = (header.width() - title_width - aux_width - 61.0).max(0.0);
    let mut visible_toolbar = (((toolbar_available + PANE_HEADER_TOOLBAR_GAP)
        / (PANE_HEADER_TOOLBAR_BUTTON + PANE_HEADER_TOOLBAR_GAP))
        .floor() as usize)
        .min(icon_count);
    loop {
        let toolbar_width = PANE_HEADER_TOOLBAR_BUTTON * visible_toolbar as f32
            + PANE_HEADER_TOOLBAR_GAP * visible_toolbar.saturating_sub(1) as f32;
        let toolbar_left = header.right() - 4.0 - toolbar_width;
        let close_center_x = (title_left + title_width + 14.0)
            .min(toolbar_left - aux_width - 11.0)
            .max(title_left + 8.0);
        let close = egui::Rect::from_center_size(
            egui::pos2(close_center_x, center_y),
            egui::vec2(20.0, 20.0),
        );
        let toolbar: Vec<egui::Rect> = (0..visible_toolbar)
            .map(|index| {
                egui::Rect::from_min_size(
                    egui::pos2(
                        toolbar_left
                            + index as f32 * (PANE_HEADER_TOOLBAR_BUTTON + PANE_HEADER_TOOLBAR_GAP),
                        center_y - PANE_HEADER_TOOLBAR_BUTTON * 0.5,
                    ),
                    egui::vec2(PANE_HEADER_TOOLBAR_BUTTON, PANE_HEADER_TOOLBAR_BUTTON),
                )
            })
            .collect();
        // 닫기 우선 가드 — 가장 왼쪽 도구가 닫기를 덮으면 하나 더 숨기고 재계산.
        if let Some(leftmost) = toolbar.first()
            && close.intersects(*leftmost)
        {
            visible_toolbar -= 1;
            continue;
        }
        return PaneHeaderButtons {
            close,
            toolbar,
            toolbar_left,
        };
    }
}

#[derive(Clone, Copy)]
enum TerminalToolbarIcon {
    Search,
    NewTerminal,
    SplitColumns,
    SplitRows,
}

/// 세션 헤더 Search 버튼 클릭이 보조 검색(이력·Git) 토글로 가야 하는지 —
/// 그 pane의 보조 본문이 활성일 때만 그렇다(2026-08-18 스펙 "진입").
/// 다른 도구(새 셸·분할)나 보조 본문이 비활성인 Search는 항상 기존 경로
/// (`activate_terminal_toolbar`/입력 소유권 없을 때의 포커스 클레임)로 간다 —
/// 그 경로의 `input_enabled` 게이트는 그대로 두고 이 조건이 그 앞에 별도로 얹힌다.
fn search_click_targets_aux_search(icon: TerminalToolbarIcon, aux_active: bool) -> bool {
    matches!(icon, TerminalToolbarIcon::Search) && aux_active
}

/// 보조 탭이 실제로 활성인지 — App이 넘긴 원본 목록(`aux_tabs`, ground truth)만
/// 본다. `layout_aux_tabs`가 돌려주는 배치(placements)로 판정하면 안 되는 이유:
/// 헤더가 극단적으로 좁으면 그 함수가 활성 탭까지 접어(빈 Vec) 돌려줄 수 있는데,
/// 그래도 그 문서는 실제로 활성이라 본문은 계속 그려진다(레이아웃과 무관한 별도
/// 게이트) — 헤더 chrome(제목 밝기·검색 라우팅)만 이 목록을 안 쓰면 "세션이 선택된
/// 것처럼" 실제 상태와 어긋나 보인다(2026-08-22 리뷰).
fn any_aux_tab_active(aux_tabs: &[PaneAuxTab]) -> bool {
    aux_tabs.iter().any(|tab| tab.active)
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub struct AttachedPaneTarget {
    pub workspace_id: String,
    pub tab: runtime::MuxTabId,
    pub pane: runtime::MuxPaneId,
    pub session: runtime::SessionId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum AttachedPaneAvailability {
    Available,
    Suspended,
    Disconnected,
    Unavailable,
}

impl AttachedPaneAvailability {
    #[allow(dead_code)]
    fn message_key(self) -> Option<&'static str> {
        match self {
            Self::Available => None,
            Self::Suspended => Some("workspace.cross_pane.suspended"),
            Self::Disconnected => Some("workspace.cross_pane.disconnected"),
            Self::Unavailable => Some("workspace.cross_pane.input_unavailable"),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(dead_code)]
pub struct AttachedPaneOutput {
    pub focus_requested: bool,
    pub detach_requested: bool,
    pub target_present: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AttachedPaneReorder {
    pub(crate) attachment_id: crate::ui::cross_workspace::AttachmentId,
    pub(crate) destination_index: usize,
}

impl AttachedPaneReorder {
    fn new(
        attachment_id: crate::ui::cross_workspace::AttachmentId,
        destination_index: usize,
    ) -> Self {
        Self {
            attachment_id,
            destination_index: destination_index
                .min(crate::ui::cross_workspace::HARD_MAX_CROSS_WORKSPACE_PANES - 1),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AttachedPaneHeaderContext {
    pub(crate) attachment_id: crate::ui::cross_workspace::AttachmentId,
    pub(crate) destination_index: usize,
    identity_color: egui::Color32,
}

impl AttachedPaneHeaderContext {
    #[allow(dead_code)]
    pub(crate) fn new(
        attachment_id: crate::ui::cross_workspace::AttachmentId,
        destination_index: usize,
    ) -> Self {
        Self {
            attachment_id,
            destination_index: destination_index
                .min(crate::ui::cross_workspace::HARD_MAX_CROSS_WORKSPACE_PANES - 1),
            identity_color: egui::Color32::TRANSPARENT,
        }
    }

    #[allow(dead_code)]
    pub(crate) fn with_identity_color(mut self, identity_color: egui::Color32) -> Self {
        self.identity_color = identity_color;
        self
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PreparedAttachedPaneOutput {
    pub(crate) surface: AttachedPaneOutput,
    pub(crate) reorder_requested: Option<AttachedPaneReorder>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkspaceSurfaceOutput {
    pub focus_requested: bool,
    pub local_focus_claimed: Option<runtime::MuxPaneId>,
    pub document_drop_paths: Vec<PathBuf>,
    /// 이번 프레임에 보조 탭이 올린 의도(있으면 App이 그 종류의 탭 상태를 옮긴다).
    pub aux_tab_intent: Option<(PaneAuxTabKind, PaneAuxTabIntent)>,
    /// 보조 탭이 활성일 때 App이 본문을 그릴 pane body rect. 이 rect가 있으면
    /// WorkspaceUi는 그 pane의 터미널 표면·입력을 **그리지 않았다**.
    pub aux_body_rect: Option<egui::Rect>,
    /// 보조 본문이 활성인 세션 헤더에서 Search 버튼이 눌렸다 — App이 이번 프레임에
    /// `aux_search.toggle()`을 부른다(스펙 "진입"). `input_enabled` 게이트는 그대로
    /// 두고 그 앞에 얹은 별도 경로라, 이 값이 참이어도 터미널 검색은 열리지 않는다.
    pub aux_search_toggle_requested: bool,
}

#[derive(Clone, Copy)]
#[allow(dead_code)]
enum PaneRenderMode<'a> {
    Local {
        input_enabled: bool,
    },
    Attached {
        input_enabled: bool,
        workspace_id: &'a str,
        workspace_label: &'a str,
        session: SessionId,
    },
}

impl PaneRenderMode<'_> {
    fn input_enabled(self) -> bool {
        match self {
            Self::Local { input_enabled } | Self::Attached { input_enabled, .. } => input_enabled,
        }
    }

    fn is_local(self) -> bool {
        matches!(self, Self::Local { .. })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct PaneRenderOutput {
    focus_requested: bool,
    local_focus_claimed: Option<runtime::MuxPaneId>,
    document_drop_paths: Vec<PathBuf>,
    aux_tab_intent: Option<(PaneAuxTabKind, PaneAuxTabIntent)>,
    aux_body_rect: Option<egui::Rect>,
    aux_search_toggle_requested: bool,
}

impl PaneRenderOutput {
    fn merge(&mut self, other: Self) {
        self.focus_requested |= other.focus_requested;
        if other.local_focus_claimed.is_some() {
            self.local_focus_claimed = other.local_focus_claimed;
        }
        self.document_drop_paths.extend(other.document_drop_paths);
        if other.aux_tab_intent.is_some() {
            self.aux_tab_intent = other.aux_tab_intent;
        }
        if other.aux_body_rect.is_some() {
            self.aux_body_rect = other.aux_body_rect;
        }
        self.aux_search_toggle_requested |= other.aux_search_toggle_requested;
    }
}

fn pane_interaction_id(
    mode: PaneRenderMode<'_>,
    kind: &'static str,
    pane: &runtime::MuxPaneId,
) -> egui::Id {
    match mode {
        PaneRenderMode::Local { .. } => egui::Id::new((kind, pane)),
        PaneRenderMode::Attached {
            workspace_id,
            session,
            ..
        } => egui::Id::new(("attached_pane", workspace_id, session.0, kind, pane)),
    }
}

fn pane_allows_terminal_dnd(mode: PaneRenderMode<'_>) -> bool {
    mode.is_local()
}

#[allow(dead_code)]
fn find_attached_pane<'a>(
    mux: &'a MuxSnapshot,
    target: &AttachedPaneTarget,
) -> Option<&'a runtime::PaneSnapshot> {
    mux.tabs
        .iter()
        .find(|tab| tab.id == target.tab)
        .and_then(|tab| {
            tab.panes
                .iter()
                .find(|pane| pane.id == target.pane && pane.session_id == Some(target.session))
        })
}

#[allow(dead_code)]
pub(crate) fn visible_attachment_indices<'a>(
    viewport: egui::Rect,
    rects: &'a [egui::Rect],
) -> impl Iterator<Item = usize> + 'a {
    rects
        .iter()
        .take(crate::ui::cross_workspace::HARD_MAX_CROSS_WORKSPACE_PANES)
        .enumerate()
        .filter_map(move |(index, rect)| viewport.intersect(*rect).is_positive().then_some(index))
}

fn terminal_toolbar_button(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    id: egui::Id,
    icon: TerminalToolbarIcon,
) -> egui::Response {
    let response = ui.interact(rect, id, egui::Sense::click());
    if response.hovered() || response.has_focus() {
        // pane 헤더는 테마와 무관하게 **항상 다크**다(docs/ui-components.md §5, 그리고
        // header_fill이 designall::DARK.app_background 고정임을 단언하는 테스트). 그래서
        // 여기서는 visuals() 토큰을 쓰면 안 된다 — 라이트 테마에서 밝은 hover가 깔리고
        // 그 위의 흰색 아이콘(아래 #f2f2f2)이 안 보인다.
        // 값은 하드코딩하되 축만 맞춘다: 옛 값 #22222a는 240도 보랏빛이라 220도로 통일한
        // 주변 면들 사이에서 혼자 튀었다. 명도는 그대로 두고 색상만 옮겼다(2026-08-06).
        ui.painter()
            .rect_filled(rect, 1.0, egui::Color32::from_rgb(0x21, 0x24, 0x2c));
    }
    let color = if response.hovered() || response.has_focus() {
        egui::Color32::from_rgb(0xf2, 0xf2, 0xf2)
    } else {
        egui::Color32::from_rgb(0xc8, 0xcc, 0xd2)
    };
    paint_terminal_toolbar_icon(ui.painter(), rect, icon, color);
    response
}

fn paint_terminal_toolbar_icon(
    painter: &egui::Painter,
    rect: egui::Rect,
    icon: TerminalToolbarIcon,
    color: egui::Color32,
) {
    // 클릭 영역과 헤더 높이는 유지하고, 네 도형의 가로·세로 외곽만 기존보다 2pt
    // 키운다. 좁은 pane에서 버튼 수가 줄어드는 기준은 바뀌지 않는다.
    let stroke = egui::Stroke::new(1.25, color);
    let center = rect.center();
    match icon {
        TerminalToolbarIcon::Search => {
            let lens = center + egui::vec2(-0.9, -0.9);
            painter.circle_stroke(lens, 4.2, stroke);
            painter.line_segment(
                [lens + egui::vec2(3.0, 3.0), lens + egui::vec2(5.5, 5.5)],
                stroke,
            );
        }
        TerminalToolbarIcon::NewTerminal => {
            let body = egui::Rect::from_center_size(center, egui::vec2(11.0, 9.0));
            painter.rect_stroke(body, 0.75, stroke, egui::StrokeKind::Inside);
            painter.line_segment(
                [
                    center + egui::vec2(-3.5, -2.0),
                    center + egui::vec2(-1.5, 0.0),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    center + egui::vec2(-1.5, 0.0),
                    center + egui::vec2(-3.5, 2.0),
                ],
                stroke,
            );
            painter.line_segment(
                [center + egui::vec2(0.0, 2.8), center + egui::vec2(3.4, 2.8)],
                stroke,
            );
        }
        TerminalToolbarIcon::SplitColumns | TerminalToolbarIcon::SplitRows => {
            let body = egui::Rect::from_center_size(center, egui::vec2(11.0, 11.0));
            painter.rect_stroke(body, 0.75, stroke, egui::StrokeKind::Inside);
            match icon {
                TerminalToolbarIcon::SplitColumns => {
                    painter.vline(center.x, body.y_range(), stroke);
                }
                TerminalToolbarIcon::SplitRows => {
                    painter.hline(body.x_range(), center.y, stroke);
                }
                _ => unreachable!(),
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SplitDragPhase {
    Active,
    Committed { admitted: bool },
}

#[derive(Clone, Debug)]
struct SplitDragTransaction {
    tab: runtime::MuxTabId,
    path: Vec<u8>,
    ratio: f32,
    phase: SplitDragPhase,
    /// Local queue admission is not runtime delivery. Matching mux state is accepted as an ACK
    /// only after this exact operation/generation completes successfully.
    pending_delivery: Option<(WorkspaceProtocolOperation, u64)>,
    retry: ProtocolRetryBackoff,
}

#[derive(Clone, Copy, Debug)]
struct StagedTerminalResize {
    pass: u64,
    cols: u16,
    rows: u16,
}

#[derive(Clone, Copy, Debug)]
struct ResizeDeliveryRollback {
    session: SessionId,
    token: runtime::ResizeToken,
    ack_retry: bool,
    target: (u16, u16),
    previous: Option<(u16, u16)>,
}

#[derive(Clone, Copy, Debug, Default)]
struct ProtocolRetryBackoff {
    failures: u8,
    retry_at: Option<std::time::Instant>,
}

enum ProtocolRetryGate {
    Ready,
    Wait(std::time::Duration),
    Exhausted,
}

impl ProtocolRetryBackoff {
    fn record_busy(&mut self, now: std::time::Instant) -> bool {
        if self.failures >= PROTOCOL_RETRY_LIMIT {
            self.retry_at = None;
            return false;
        }
        let shift = u32::from(self.failures);
        self.failures = self.failures.saturating_add(1);
        let delay = PROTOCOL_RETRY_BASE.saturating_mul(1_u32 << shift);
        self.retry_at = Some(now + delay);
        true
    }

    fn gate(&mut self, now: std::time::Instant) -> ProtocolRetryGate {
        match self.retry_at {
            Some(retry_at) if now < retry_at => ProtocolRetryGate::Wait(retry_at - now),
            Some(_) => {
                self.retry_at = None;
                ProtocolRetryGate::Ready
            }
            None if self.failures >= PROTOCOL_RETRY_LIMIT => ProtocolRetryGate::Exhausted,
            None => ProtocolRetryGate::Ready,
        }
    }
}

#[derive(Clone, Default, Debug)]
struct TerminalPreeditState {
    text: String,
    active_range_chars: Option<std::ops::Range<usize>>,
    owner: Option<SessionId>,
}

#[derive(Debug)]
struct PendingImeSubmit {
    owner: SessionId,
    started: std::time::Instant,
    /// Last visible syllable before AppKit clears preedit ahead of Commit.
    preedit_at_submit: String,
    /// Native punctuation whose Text event was swallowed while finishing preedit.
    before_submit: Vec<u8>,
    /// Clipboard bytes requested before Enter; never deduplicate these against Commit.
    independent_before_submit: Vec<u8>,
    /// Enter itself, followed by keys typed while Commit is still pending.
    after_submit: Vec<u8>,
}

impl PendingImeSubmit {
    fn for_session(&self, session: SessionId) -> bool {
        self.owner == session
    }
}

fn append_ordered_terminal_bytes(
    pending: &mut Vec<u8>,
    deferred: &mut Option<PendingImeSubmit>,
    bytes: &[u8],
) {
    if let Some(deferred) = deferred {
        deferred.after_submit.extend_from_slice(bytes);
    } else {
        pending.extend_from_slice(bytes);
    }
}

impl TerminalPreeditState {
    fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
    fn clear(&mut self) {
        *self = Self::default();
    }
    fn is_active_for(&self, owner: SessionId) -> bool {
        self.owner == Some(owner) && !self.is_empty()
    }

    // Display projection only: it does not consume raw events or send PTY input.
    // Publish after the existing IME admission/reconciliation path accepts input.
    fn for_frame(&self, owner: SessionId, events: &[egui::Event]) -> Self {
        let mut next = if self.owner == Some(owner) {
            self.clone()
        } else {
            Self::default()
        };
        for event in events {
            match event {
                egui::Event::Ime(egui::ImeEvent::Preedit {
                    text,
                    active_range_chars,
                }) => {
                    next.text.clone_from(text);
                    next.active_range_chars = active_range_chars.as_ref().map(|range| {
                        let count = text.chars().count();
                        let start = range.start.min(count);
                        start..range.end.min(count).max(start)
                    });
                    next.owner = (!text.is_empty()).then_some(owner);
                    if text.is_empty() {
                        next.clear();
                    }
                }
                egui::Event::Ime(egui::ImeEvent::Commit(_)) => next.clear(),
                _ => {}
            }
        }
        next
    }

    fn view(&self) -> Option<renderer_egui::PreeditView<'_>> {
        (!self.is_empty()).then_some(renderer_egui::PreeditView {
            text: &self.text,
            active_range_chars: self.active_range_chars.as_ref(),
        })
    }
}

type AgentPasteTarget = (SessionId, crate::agent_detect::AgentExecutionIdentity);
type AgentSendChoice = (Vec<AgentPasteTarget>, Option<String>);

pub(crate) struct SelectedAgentPrompt {
    pub session: SessionId,
    pub execution: crate::agent_detect::AgentExecutionIdentity,
    pub prompt: std::sync::Arc<str>,
}

pub struct WorkspaceUi {
    mux: Option<Arc<MuxSnapshot>>,
    /// Composer growth crops the viewport; only permanent layout changes resize PTYs.
    composer_height_expansion: f32,
    pub cloud_answers: Arc<[crate::ui::cloud_answer::Answer]>,
    pub selected_cloud_answer: Option<String>,
    sessions: HashMap<SessionId, SessionView>,
    /// Runtime이 per-session shell metadata를 제공하기 전까지 path insert quoting에 쓰는
    /// workspace 기본 shell kind.
    shell_kind: crate::ui::file_tree::ShellKind,
    /// IME 조합 중 텍스트 (focused pane 전용)
    preedit: TerminalPreeditState,
    /// Enter and later keystrokes waiting for a terminal-owned IME Commit.
    pending_ime_submit: Option<PendingImeSubmit>,
    /// A focused pane changed before AppKit delivered its pending Commit.
    detached_ime_submit: Option<PendingImeSubmit>,
    /// A focus switch may flush the old pane's preedit before AppKit emits Commit.
    flushed_ime_commit: Option<(String, std::time::Instant)>,
    /// 이번 UI 프레임 직전에 AppKit local monitor가 본 ASCII 문장부호/숫자/공백
    /// key-down. IME Commit/Text와 대조한 뒤 누락된 문자만 복구하고 프레임 끝에 버린다.
    native_printable_key_downs: Vec<crate::native_key_monitor::NativePrintableKeyDown>,
    #[cfg(test)]
    test_native_key_downs: Vec<crate::native_key_monitor::NativePrintableKeyDown>,
    /// egui-winit이 이미지-only clipboard에서 Event::Paste 없이 소비하는 macOS Command+V
    /// 원본 key-down. 터미널 입력 소유권을 확인한 pane에서만 1회 소비한다.
    native_clipboard_paste_requested: bool,
    /// egui가 고수준 Copy를 생략하거나 한 프레임 늦게 보낼 때의 macOS Command+C
    /// 원본 key-down. 선택과 터미널 입력 소유권이 모두 확인된 경우에만 복사한다.
    native_clipboard_copy_requested: bool,
    prepared_attached_input_owner: Option<AttachedPaneTarget>,
    prepared_attached_input_pass: Option<u64>,
    prepared_attached_input_consumed: bool,
    /// 파일 트리가 이번 프레임 ⌘V/⌘C를 소비 — 같은 제스처의 터미널 붙여넣기/선택 복사
    /// 이중 처리를 누른다(App이 사이드바 렌더 직후 설정, prepare_frame이 프레임
    /// 플래그로 옮긴다. 파일 트리 §과제②③ 충돌 금지).
    suppress_paste_request: bool,
    suppress_copy_request: bool,
    /// 위 요청의 이번-프레임 확정값 (prepare_frame이 매 프레임 재계산 — 이월 없음).
    paste_suppressed: bool,
    copy_suppressed: bool,
    /// 세션 → 셸 pid (App이 ResourceUsage에서 매 프레임 갱신). 터미널 경로 더블클릭의
    /// 상대경로를 그 셸의 실제 cwd로 해석하는 데 쓴다 (2026-07-14).
    session_pids: HashMap<SessionId, u32>,
    /// 경로 해석 캐시: (세션, 단어) → 해석 결과. hover가 매 프레임 도는 경로라
    /// 같은 단어의 재해석(metadata/lsof)을 막는다. TTL PATH_CACHE_TTL.
    path_click_cache: Option<(SessionId, String, Option<PathClick>, std::time::Instant)>,
    /// UI leaf는 native I/O를 실행하지 않는다. 한 프레임은 bounded intent만 만들고
    /// App host가 완료를 돌려준다. generation은 cwd/root가 바뀐 뒤의 늦은 결과를 막는다.
    io_generation: u64,
    next_io_operation: u64,
    io_intents: VecDeque<WorkspaceIoIntent>,
    protocol_generation: u64,
    next_protocol_operation: u64,
    protocol_intents: VecDeque<WorkspaceProtocolIntent>,
    protocol_inflight: HashMap<(WorkspaceProtocolOperation, u64), PendingProtocolIntent>,
    protocol_retry_at: Option<std::time::Instant>,
    pending_path_resolution: Option<PendingPathResolution>,
    /// 직전 폴더 클릭 (경로, 시각) — 더블클릭이 clicked를 두 번 발화시켜 같은 cd가
    /// 연속 주입되는 것을 막는다.
    last_dir_click: Option<(std::path::PathBuf, std::time::Instant)>,
    /// 직전 URL 클릭 (주소, 시각) — last_dir_click과 같은 이중 발화 방지.
    last_url_click: Option<(String, std::time::Instant)>,
    /// 마지막으로 egui Event::Paste 텍스트를 직접 전송한 시각. ⌘V는 press에서
    /// Event::Paste가, release에서 키 이벤트가 **다른 프레임으로** 도착할 수 있다 —
    /// 그때 release 쪽 이미지 태스크가 클립보드 텍스트 fallback으로 같은 내용을
    /// 한 번 더 보내 이중 붙여넣기가 됐다 (2026-07-14 사용자). 같은 제스처로 보고
    /// release 태스크를 건너뛰기 위한 표식.
    last_text_paste: Option<std::time::Instant>,
    /// AppKit Command+V key-down에서 이미지 paste 태스크를 시작한 시각. 뒤이어 오는
    /// egui V key-up fallback이 같은 clipboard를 다시 붙이지 않게 하는 제스처 표식.
    last_native_paste: Option<std::time::Instant>,
    /// 세션별 마지막 전송한 (cols, rows) — 변화 시에만 Resize 전송
    resize_owner: [u8; 16],
    sent_sizes: HashMap<SessionId, (u16, u16)>,
    /// 세션별 「아직 확정되지 않은」 resize 목표 —
    /// (cols, rows, viewport 크기, 그 목표가 안정되기 시작한 시각).
    /// 창 드래그로 pane 크기가 프레임마다 바뀌는 동안 목표나 viewport 크기도 계속
    /// 갱신되고, RESIZE_DRAG_DEBOUNCE만큼 둘 다 유지돼야 비로소 전송된다
    /// (queue_terminal_resize_debounced 참고).
    pending_resize_target: HashMap<SessionId, (u16, u16, Option<egui::Vec2>, std::time::Instant)>,
    /// 트랙패드 미세 스크롤 누적 (focused pane 기준)
    scroll_residual: f32,
    /// 드래그 선택 오토스크롤 행 누적 — 경계 초과 속도(행/초)×dt의 소수부 보관 (T4)
    drag_autoscroll_residual: f32,
    /// 이번 프레임에 명령을 보냈다 — 응답 이벤트 폴링을 위해 repaint 예약
    command_sent: bool,
    /// mux focused_pane 변경 추적
    last_focused_pane: Option<runtime::MuxPaneId>,
    /// 세션별 pane 강조 플래시 (만료 시각, 총 지속시간). 포커스 이동은 FOCUS_FLASH(1초),
    /// 입력요청·작업완료는 PANE_FLASH(2초)로 서로 다르게 쓰므로 페이드 비율 계산을 위해
    /// 지속시간을 함께 보관한다(2026-07-12 사용자). 탑라인은 별도로 포커스 동안 유지.
    session_flash: HashMap<SessionId, (std::time::Instant, std::time::Duration)>,
    /// egui 포커스 동기화와 입력 대상 전환 대기. Runtime의 `FocusPane` 반영은 비동기라,
    /// 클릭·검색 닫힘 직후에도 이 pane을 먼저 입력 대상으로 삼아 첫 문자를 잃지 않는다.
    pending_focus: Option<runtime::MuxPaneId>,
    /// App이 비동기 restore 전에 건 명시적 입력 fence. Runtime snapshot 기반 refocus와
    /// 분리해야 새 runtime focus가 오래된 일반 pending을 정상적으로 교체할 수 있다.
    explicit_pending_focus: Option<runtime::MuxPaneId>,
    explicit_pending_focus_observed: bool,
    /// 이 프레임에 사용자가 터미널을 직접 클릭해 키보드 소유권을 요청했다. App이
    /// 뒤이어 렌더하는 Agents TextEdit의 지연 autofocus를 취소하는 one-shot 신호다.
    terminal_focus_claimed: bool,
    /// 성공 전달된 셸 spawn 순서와 프로토콜 입장을 기다리는 cd 후속 명령.
    /// spawn·후속 cd를 합쳐 최대 WORKSPACE_PROTOCOL_CAP개만 유지한다.
    pending_spawn_cwds: VecDeque<PendingShellSpawn>,
    /// split 경계의 로컬 미리보기와 비동기 mux ACK 수명. 릴리즈 뒤에도 matching
    /// tab/path/ratio MuxUpdated까지 미리보기를 유지해 persisted ratio로 되튀지 않는다.
    split_drag: Option<SplitDragTransaction>,
    /// matching ACK 뒤 실제 grid가 바뀌어야 하는 visible session. 각 세션은 최종 UI
    /// pass에서 distinct Resize를 admission하고 exact runtime completion을 받을 때까지
    /// queued/in-flight 상태로 추적된다.
    split_final_resize_sessions: HashSet<SessionId>,
    /// Exact queued/in-flight final Resize operations. Selection invalidation and presentation
    /// fencing begin only after the runtime accepts the matching operation.
    split_final_resize_pending: HashMap<(WorkspaceProtocolOperation, u64), (SessionId, u16, u16)>,
    /// `sent_sizes` is an optimistic queue-side dedupe. Preserve its previous value so an exact
    /// delivery failure cannot masquerade as a successful PTY resize.
    resize_delivery_rollbacks: HashMap<(WorkspaceProtocolOperation, u64), ResizeDeliveryRollback>,
    /// A closed runtime channel must not create an immediate repaint/retry loop. A different
    /// geometry or fresh viewport evidence clears this bounded per-session sentinel.
    failed_resize_targets: HashMap<SessionId, (u16, u16)>,
    resize_retry: HashMap<SessionId, ProtocolRetryBackoff>,
    /// egui render pass에서는 protocol/debounce 상태를 직접 바꾸지 않는다. sizing pass는
    /// 후보도 만들지 않고, 일반 pass 후보는 App::ui의 마지막 widget 뒤 final-pass flush가
    /// 같은 cumulative pass만 실행한다.
    staged_terminal_resizes: HashMap<SessionId, StagedTerminalResize>,
    staged_split_commit_pass: Option<u64>,
    /// 닫기 확인 대기 중인 pane — 실행 중 세션이 있는 pane 닫기는 확인을 거친다
    /// (2026-07-05 사용자 보고: 닫기 실수로 셸 전체 즉사 방지).
    confirm_close: Option<runtime::MuxPaneId>,
    /// 「에이전트로 보내기」 프리셋 프롬프트 (설정 미러 — App이 매 프레임 갱신).
    agent_send_presets: Vec<String>,
    /// pane 우클릭 → "환경변수·API 설정" 요청 (E4 ⑥). App이 프레임에서 take해
    /// 설정 창을 Environment 카테고리로 연다.
    open_environment_requested: Option<super::environment::EnvironmentOpenRequest>,
    new_session_requested: Option<NewSessionRequest>,
    /// pane 우클릭 → 세션 폴더 요청(파일 트리 이동/Finder 열기, 2026-07-18). cwd
    /// 해석(lsof 폴백 포함)과 트리·Finder 라우팅은 App 몫이라 요청만 쌓는다 — E4 ⑥
    /// take_open_environment와 같은 프레임 소비 패턴.
    session_folder_request: Option<SessionFolderRequest>,
    /// pane 우클릭 → "메모에 추가" 요청 (PR-4). 선택 원문만 담는다 — 개행 처리·
    /// 상한(WORKSPACE_NOTE_MAX_BYTES) 판정은 leaf가 storage 상수를 참조할 수 없어
    /// App이 한다(check-boundary: leaf UI must not access the storage crate).
    note_append_request: Option<String>,
    /// pane 하단 「다시 실행」 클릭 요청 (PR-3). RuntimeCommand 전송은 App 몫이라
    /// (check-boundary: leaf UI must not execute runtime protocol commands directly)
    /// 세션만 담아 요청한다 — take_open_environment와 같은 프레임 소비 패턴.
    respawn_archived_request: Option<SessionId>,
    /// 터미널 마우스 선택 (session, anchor 셀, head 셀 — 드래그 방향 그대로,
    /// 렌더/복사 시 정규화). 새 출력(Viewport)이 오면 그 세션의 선택은 해제한다.
    selection: Option<(SessionId, usize, usize)>,
    /// 활성 workspace의 프로젝트명(폴더명 ≈ 깃 레포명, 없으면 "~"). 세션 기본 제목이
    /// "셀 134" 대신 이걸로 표시된다. rename한 세션은 그대로 둔다. App이 매 프레임 세팅.
    project_name: Option<String>,
    /// UI 텍스트 배율(App이 매 프레임 set). 터미널은 zoom_factor로 같이 커지므로 font_size를
    /// 이 값으로 역보정해 물리 크기를 유지한다(UI만 스케일, 터미널 독립 — 2026-07-13).
    ui_scale: f32,
    /// 활성 워크스페이스의 고유색 — 포커스된 pane 상단선에 쓴다.
    /// App이 매 프레임 밀어 넣는다(사이드바 목록 순서에 따라 배정되므로 여기서 못 만든다).
    workspace_accent: egui::Color32,
    /// App이 소유한 보조 탭 목록(이력·Git·문서 여러 개) — 있으면 포커스된
    /// 로컬 pane 헤더의 **같은 32pt 행**에 세션 탭 옆으로 이어 붙여 그린다. runtime의
    /// `MuxTabId`/pane이 아니므로 이 값이 바뀌어도 PTY·세션·mux 탭은 생기거나 죽지
    /// 않는다. WorkspaceUi는 클릭 의도만 돌려주고 상태와 본문은 App이 소유한다.
    aux_tabs: Vec<PaneAuxTab>,
    /// 이번 프레임에 보조 탭을 붙일 pane. 보통 focused pane이지만, runtime이 아직
    /// 아무 pane도 포커스하지 않은 프레임에서는 layout의 첫 pane으로 떨어진다 —
    /// 그러지 않으면 탭도 본문도 사라져 레일만 켜진 채 화면이 반응하지 않는다.
    aux_tab_pane: Option<runtime::MuxPaneId>,
    /// 세션별 현재 작업 폴더(App이 매 프레임 set) — 1행 제목 폴더명/프로젝트명 원천.
    session_cwds: std::collections::HashMap<SessionId, String>,
    /// App host가 filesystem 밖에서 미리 계산한 세션별 프로젝트 표시명. cwd를 함께
    /// 검증하므로 늦은 결과가 이동한 셸의 제목을 덮지 못하고, immutable Arc snapshot이라
    /// 동일 revision setter는 전체 맵을 복제하지 않는다.
    session_project_names: SessionProjectNameSnapshot,
    /// 세션별 에이전트 표시정보(model/effort/context — App이 병합해 set) — 3줄 행 2/3행.
    agent_info: std::collections::HashMap<SessionId, crate::agent_detect::AgentDisplay>,
    agent_executions:
        std::collections::HashMap<SessionId, crate::agent_detect::AgentExecutionIdentity>,
    selected_agent_prompts: VecDeque<SelectedAgentPrompt>,
    /// PR-3: 세션별로 영속된 에이전트 종류("claude"/"codex"/"kimi" — App이 pane_id
    /// 키인 persisted_agents에서 세션 키로 바꿔 매 갱신마다 set). 「다시 실행」 버튼
    /// 문구(이어서/새로) 판정에만 쓴다 — leaf는 agent_resume::resume_args 같은 순수
    /// 함수는 직접 불러도 된다(check-boundary가 막는 건 storage/runtime 직접 접근).
    archived_resume_presentation:
        std::collections::HashMap<SessionId, crate::agent_resume::ArchivedResumePresentation>,
    /// App host가 수행 중인 terminal clipboard 요청. completion은 operation/generation을
    /// 모두 맞춘 뒤 정확히 한 번만 적용한다. 이전 요청은 다음 붙여넣기가 와도 보존한다.
    pending_pastes: VecDeque<PendingPaste>,
    native_error: Option<WorkspaceNativeError>,
    error: Option<String>,
    /// 현재 미전달 오류가 input backpressure 경고인지 — 해소 이벤트(queued=0)가
    /// 무관한 오류(spawn 실패 등)를 지우지 않게 구분한다(codex 2026-07-09).
    error_is_pressure: bool,
    /// 프로토콜 요청이 실제로 유실됐음을 표시하는 플래그(2026-08-18, "terminal protocol
    /// request rejected" 배너 버그 수정). send/send_keep_selection/spawn_shell_at은 catalog가
    /// 없어 문구를 미리 만들 수 없다 — 여기 플래그만 세우고 take_error_notice가
    /// catalog로 채운다. Busy(큐 포화)·InvalidCommand(내부 계약 위반)는 사용자가 어찌할 수
    /// 없는 신호라 이 플래그를 쓰지 않고 tracing으로만 남긴다 — PayloadTooLarge/DeliveryFailed
    /// 처럼 정말 되돌릴 수 없이 사라진 요청만 여기로 온다.
    protocol_request_lost: bool,
    /// 터미널 텍스트 검색 상태 (T3). Cmd+F로 열리고, 열려 있으면 focused pane 우상단에
    /// 검색 바를 그린다. 한 번에 한 세션만 검색한다.
    search: Option<TerminalSearch>,
    /// 「마지막 출력 복사」 추출 대기 세션. 응답이 오면 clipboard copy를 예약하고,
    /// 세션 소멸 시 MuxUpdated에서 정리한다.
    last_output_copy_pending: HashSet<SessionId>,
    /// handle_events가 예약한 「마지막 출력 복사」 텍스트 — 그 자리엔 egui Context가
    /// 없어 show()가 같은 프레임에 ctx.copy_text로 수행한다.
    pending_copy: Option<String>,
    /// Latest-only native notice request. Repeated render events overwrite the
    /// prior pending value instead of growing a queue or starting a worker.
    notice_intent: Option<WorkspaceNotice>,
    /// 이번 프레임에 그린 pane들의 렌더 카운터 합 (B1 실측). show() 시작에서 리셋하고
    /// pane마다 누적한다 — 정수 덧셈뿐이라 게이트 없이 항상 집계한다.
    frame_counters: renderer_egui::RenderCounters,
}

/// 터미널 검색 매치 수 상한 (T3) — worker에 보내는 요청 상한. 도달 시 결과가 잘린다.
const SEARCH_MAX_MATCHES: u32 = 1000;

/// pane 우클릭 「세션 폴더 …」 메뉴의 요청 (2026-07-18) — App이 프레임마다 take해
/// cwd를 해석(session_cwd_lookup, lsof 폴백)하고 파일 트리/Finder로 라우팅한다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionFolderRequest {
    /// 사이드바 파일 트리 루트를 이 세션의 현재 폴더로 이동.
    RevealInTree(SessionId),
    /// 이 세션의 현재 폴더를 Finder(OS 기본)로 연다.
    OpenInFinder(SessionId),
}

/// 검색 입력창이 직접 처리하는 키 동작.
#[derive(Clone, Copy)]
enum TerminalSearchKey {
    Next,
    Previous,
    Close,
}

fn terminal_search_key(event: &egui::Event) -> Option<TerminalSearchKey> {
    let egui::Event::Key {
        key,
        pressed: true,
        modifiers,
        ..
    } = event
    else {
        return None;
    };
    if modifiers.ctrl || modifiers.alt || modifiers.command || modifiers.mac_cmd {
        return None;
    }
    match key {
        egui::Key::Enter if modifiers.shift => Some(TerminalSearchKey::Previous),
        egui::Key::Enter => Some(TerminalSearchKey::Next),
        egui::Key::Escape => Some(TerminalSearchKey::Close),
        _ => None,
    }
}

/// 터미널 텍스트 검색 세션 상태 (T3).
struct TerminalSearch {
    /// 검색 대상 세션 — 이 세션의 pane에만 검색 바를 그린다.
    session: SessionId,
    /// 현재 입력된 쿼리.
    query: String,
    /// worker에 마지막으로 요청한 쿼리 — 바뀔 때만 재검색한다(매 프레임 재검색 금지).
    requested: Option<String>,
    /// 현재 검색의 전송 승인 추적. 이전 검색의 늦은 실패는 새 검색을 무효화하지 않는다.
    delivery: Option<(WorkspaceProtocolOperation, u64)>,
    /// 최근 검색 결과(화면 최하단 우선 정렬).
    matches: Vec<terminal::ScrollbackMatch>,
    /// 검색 시점의 전체 라인 수 — 스크롤 목표 클램프에 쓴다.
    total_lines: u32,
    /// 매치 상한 도달로 결과가 잘렸는지.
    capped: bool,
    /// 현재 매치 인덱스(0 = 최신). matches가 비면 무의미.
    current: usize,
    /// 이번 프레임에 입력창 포커스를 요청해야 하는지.
    focus_input: bool,
    /// 직전 렌더의 입력 위젯. 터미널이 그려지기 전에 검색 키를 소비할 때 사용한다.
    input_id: Option<egui::Id>,
    /// 다음 렌더에서 current 매치가 화면에 보이도록 스크롤해야 하는지.
    scroll_to_current: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkspaceIoOperation(u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspacePathKind {
    Directory,
    OpenableFile,
}

/// Native host 경계를 지나는 경로. raw path는 Debug에 절대 노출하지 않고, 생성 시
/// NUL/byte 상한을 검증한다. Clone/Serialize를 구현하지 않는다.
pub struct WorkspacePathPayload {
    path: PathBuf,
    bytes: usize,
}

impl WorkspacePathPayload {
    pub fn try_new(path: PathBuf) -> Result<Self, WorkspaceIoErrorCode> {
        let encoded = path.as_os_str().as_encoded_bytes();
        if encoded.contains(&0) {
            return Err(WorkspaceIoErrorCode::InvalidPath);
        }
        let bytes = encoded.len();
        if bytes == 0 || bytes > WORKSPACE_PATH_MAX_BYTES {
            return Err(WorkspaceIoErrorCode::PathTooLarge);
        }
        Ok(Self { path, bytes })
    }

    pub fn as_path(&self) -> &Path {
        &self.path
    }

    pub fn into_path(self) -> PathBuf {
        self.path
    }
}

impl std::fmt::Debug for WorkspacePathPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspacePathPayload")
            .field("path", &"REDACTED")
            .field("bytes", &self.bytes)
            .finish()
    }
}

/// Raw URL은 진단에 남기지 않는다. 터미널 링크는 http/https만 허용한다.
pub struct WorkspaceUrlPayload {
    url: String,
    bytes: usize,
}

impl WorkspaceUrlPayload {
    pub fn try_new(url: String) -> Result<Self, WorkspaceIoErrorCode> {
        let bytes = url.len();
        if bytes == 0 || bytes > WORKSPACE_URL_MAX_BYTES {
            return Err(WorkspaceIoErrorCode::UrlTooLarge);
        }
        if url
            .as_bytes()
            .iter()
            .any(|byte| matches!(byte, 0 | b'\r' | b'\n'))
            || !(url.starts_with("https://") || url.starts_with("http://"))
        {
            return Err(WorkspaceIoErrorCode::InvalidUrl);
        }
        Ok(Self { url, bytes })
    }

    pub fn as_str(&self) -> &str {
        &self.url
    }
}

impl std::fmt::Debug for WorkspaceUrlPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceUrlPayload")
            .field("url", &"REDACTED")
            .field("bytes", &self.bytes)
            .finish()
    }
}

pub enum WorkspaceIoIntent {
    ResolvePath {
        operation: WorkspaceIoOperation,
        generation: u64,
        session: SessionId,
        pid: Option<u32>,
        cwd: Option<WorkspacePathPayload>,
        word: String,
    },
    ReadTerminalClipboard {
        operation: WorkspaceIoOperation,
        generation: u64,
    },
    OpenPath(WorkspacePathPayload),
    OpenUrl(WorkspaceUrlPayload),
}

impl std::fmt::Debug for WorkspaceIoIntent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ResolvePath {
                operation,
                generation,
                session,
                pid,
                cwd,
                word,
            } => f
                .debug_struct("ResolvePath")
                .field("operation", operation)
                .field("generation", generation)
                .field("session", session)
                .field("pid", pid)
                .field("cwd", cwd)
                .field("word_bytes", &word.len())
                .finish(),
            Self::ReadTerminalClipboard {
                operation,
                generation,
            } => f
                .debug_struct("ReadTerminalClipboard")
                .field("operation", operation)
                .field("generation", generation)
                .finish(),
            Self::OpenPath(path) => f.debug_tuple("OpenPath").field(path).finish(),
            Self::OpenUrl(url) => f.debug_tuple("OpenUrl").field(url).finish(),
        }
    }
}

pub struct WorkspacePathResolution {
    pub kind: WorkspacePathKind,
    pub path: WorkspacePathPayload,
}

/// Clipboard native 결과. 경로/텍스트는 생성 시 함께 상한을 검증하며 Debug는 내용 대신
/// 계수만 노출한다. Clone/Serialize하지 않는다.
pub struct TerminalClipboardPayload {
    paths: Vec<PathBuf>,
    text: Option<String>,
    path_bytes: usize,
}

impl TerminalClipboardPayload {
    /// 파일/이미지 경로가 있으면 사용하지 않을 텍스트 flavor를 읽지 않는다.
    pub fn read_with(
        paths: Vec<PathBuf>,
        read_text: impl FnOnce() -> Option<String>,
    ) -> Result<Self, WorkspaceIoErrorCode> {
        let text = paths.is_empty().then(read_text).flatten();
        Self::try_new(paths, text)
    }

    pub fn try_new(
        paths: Vec<PathBuf>,
        text: Option<String>,
    ) -> Result<Self, WorkspaceIoErrorCode> {
        if paths.len() > TERMINAL_CLIPBOARD_PATH_MAX_ITEMS {
            return Err(WorkspaceIoErrorCode::ClipboardTooLarge);
        }
        let mut path_bytes = 0usize;
        for path in &paths {
            let encoded = path.as_os_str().as_encoded_bytes();
            if encoded.contains(&0) || encoded.len() > WORKSPACE_PATH_MAX_BYTES {
                return Err(WorkspaceIoErrorCode::InvalidPath);
            }
            path_bytes = path_bytes
                .checked_add(encoded.len())
                .ok_or(WorkspaceIoErrorCode::ClipboardTooLarge)?;
            if path_bytes > TERMINAL_CLIPBOARD_PATH_MAX_BYTES {
                return Err(WorkspaceIoErrorCode::ClipboardTooLarge);
            }
        }
        if text.as_ref().is_some_and(|value| {
            value.as_bytes().contains(&0) || value.len() > TERMINAL_CLIPBOARD_TEXT_MAX_BYTES
        }) {
            return Err(WorkspaceIoErrorCode::ClipboardTooLarge);
        }
        Ok(Self {
            paths,
            text,
            path_bytes,
        })
    }

    fn into_parts(self) -> (Vec<PathBuf>, Option<String>) {
        (self.paths, self.text)
    }
}

impl std::fmt::Debug for TerminalClipboardPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalClipboardPayload")
            .field("paths", &self.paths.len())
            .field("path_bytes", &self.path_bytes)
            .field("text_bytes", &self.text.as_ref().map_or(0, String::len))
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceIoErrorCode {
    Busy,
    InvalidPath,
    PathTooLarge,
    InvalidUrl,
    UrlTooLarge,
    ClipboardTooLarge,
    NativeFailure,
}

pub enum WorkspaceIoCompletion {
    PathResolved {
        operation: WorkspaceIoOperation,
        generation: u64,
        result: Option<WorkspacePathResolution>,
    },
    TerminalClipboardRead {
        operation: WorkspaceIoOperation,
        generation: u64,
        result: Result<TerminalClipboardPayload, WorkspaceIoErrorCode>,
    },
    /// OS가 경로/URL 열기를 거부했거나 host worker를 시작하지 못했다.
    OpenPathFailed,
    OpenUrlFailed,
}

struct PendingPathResolution {
    operation: WorkspaceIoOperation,
    generation: u64,
    session: SessionId,
    word: String,
}

/// App host paste 1건의 컨텍스트 — 요청 시점 세션/모드를 캡처해 완료 시 그대로 쓴다.
struct PendingPaste {
    operation: WorkspaceIoOperation,
    generation: u64,
    session: SessionId,
    bracketed: bool,
    shell_kind: crate::ui::file_tree::ShellKind,
    /// egui Event::Paste로 이미 받은 텍스트(있으면) — 이미지가 없을 때의 fallback.
    text_fallback: Option<Vec<u8>>,
    /// 요청 시각 — 워크스페이스가 warm으로 물러났다 돌아온 뒤 도착한 옛 paste가
    /// 살아있는 세션(바뀐 프롬프트)에 뒤늦게 꽂히지 않게 만료시킨다(codex Medium).
    requested_at: std::time::Instant,
}

#[derive(Clone, Copy)]
enum WorkspaceNativeError {
    Busy,
    ClipboardFailed,
    ClipboardTooLarge,
    ClipboardExpired,
    PathRejected,
    UrlRejected,
}

/// 알림 센터가 같은 원인의 반복 오류를 합칠 수 있게 하는 안정적인 분류다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceErrorKind {
    NativeBusy,
    ClipboardFailed,
    ClipboardTooLarge,
    ClipboardExpired,
    PathRejected,
    UrlRejected,
    ProtocolRequestLost,
    InputPressure,
    Other,
}

pub struct WorkspaceErrorNotice {
    pub kind: WorkspaceErrorKind,
    pub message: String,
}

/// App host paste 결과의 수명 — 이보다 오래된 완료는 버린다.
const PASTE_TASK_TTL: std::time::Duration = std::time::Duration::from_secs(10);

/// 세션별 화면 캐시. hidden tab 세션의 스냅샷은 `MuxUpdated`에서 버린다.
#[derive(Clone)]
struct ResizePresentationFence {
    target: (u16, u16),
    started_at: std::time::Instant,
    last_target_at: Option<std::time::Instant>,
    latest_target: Option<Arc<TerminalViewportSnapshot>>,
    stable_had_visible_text: bool,
}

#[derive(Clone, Copy)]
struct TrackedResize {
    token: runtime::ResizeToken,
    target: (u16, u16),
    admitted: bool,
    applied: Option<runtime::ResizeStamp>,
    retry_at: Option<std::time::Instant>,
    retries: u8,
    retry_pending: Option<(WorkspaceProtocolOperation, u64)>,
    retry_delivery: ProtocolRetryBackoff,
}

struct InitialSnapshotFence {
    started_at: std::time::Instant,
    latest_blank: Option<Arc<TerminalViewportSnapshot>>,
}

#[derive(Default)]
struct SessionView {
    snapshot: Option<Arc<TerminalViewportSnapshot>>,
    /// 스냅샷 세대 — 새 스냅샷을 받을 때마다 +1. 렌더러가 "이 스냅샷의 dirty를 이미
    /// 소비했는지" 판정해, 새 출력이 없는 repaint에서 전 행을 다시 shaping하지 않게 한다
    /// (2026-07-14 실측: idle repaint마다 rows_rebuilt≈전체 행 — 캐시 무효화 버그).
    snapshot_gen: u64,
    /// freeze(선택 중) 동안 도착한 최신 snapshot을 보관 — 선택 해제 시 이걸로 catch-up해
    /// 화면이 선택 당시에 머무는 것을 막는다(codex). 프레임 시작 시 프로모트한다.
    pending_snapshot: Option<Arc<TerminalViewportSnapshot>>,
    /// split로 갓 생긴 shell의 output-free 80×24 seed만 숨기는 causal one-shot gate.
    /// deadline은 replay/후속 blank로 연장하지 않는다.
    initial_presentation: Option<InitialSnapshotFence>,
    /// 최종 split Resize 뒤 TUI의 clear→redraw 중간 viewport가 표시되지 않도록 마지막
    /// stable 화면을 유지하는 유계 fence. target shape 후보만 latest-wins로 받는다.
    resize_presentation: Option<ResizePresentationFence>,
    resize_request: Option<TrackedResize>,
    resize_desired: Option<(u16, u16)>,
    resize_watermark: Option<runtime::ResizeStamp>,
    resize_generation: u64,
    resize_tracking_started: bool,
    resize_owner_epoch: u64,
    resize_owner_retries: u8,
    resize_recovery: Option<runtime::ResizeToken>,
    render_cache: renderer_egui::TerminalRenderCache,
    bracketed_paste: bool,
    /// 사이드바 세션 목록에 보여줄 최신 화면 요약 (마지막 비어있지 않은 행, ≤48자)
    summary: String,
    exit_code: Option<Option<u32>>,
    /// PR-3: 이 exit_code가 `SessionExited`(실제 종료)가 아니라 `SessionRestored`(앱
    /// 재시작 후 열람 전용 복원)로 채워졌는지. `restore_pane`(runtime)은 agent 세션만
    /// `SessionRestored`로 복원하고 셸은 항상 새 프로세스로 재기동하므로, 이 값이
    /// true면 곧 "에이전트 pane"이라는 뜻도 된다 — 별도 종류 판정이 필요 없다.
    restored_readonly: bool,
    /// status detector 감지 상태 (agent만, PR-12)
    status: Option<SessionStatus>,
    status_view: Option<runtime::SessionStatusView>,
    input_pressure: Option<runtime::PtyInputPressure>,
    /// 이 세션에서 **마지막으로 새 출력이 온 시각**(unix 초).
    ///
    /// "작업 중"과 "멈춘 것"은 화면상 둘 다 파란 점인데 실제로는 전혀 다르다. 마지막
    /// 출력 이후 경과로 조용히 죽은 세션을 찾아낸다(2026-08-08).
    last_output_at: Option<i64>,
    last_submission_at_micros: Option<i64>,
}

impl SessionView {
    fn set_resize_desired(&mut self, target: (u16, u16)) {
        if self.resize_desired != Some(target) {
            self.resize_owner_retries = 0;
            self.resize_recovery = None;
        }
        self.resize_desired = Some(target);
    }

    fn prepare_tracked_resize(
        &mut self,
        owner: [u8; 16],
        target: (u16, u16),
    ) -> Option<runtime::ResizeToken> {
        self.set_resize_desired(target);
        if let Some(request) = &self.resize_request {
            if request.admitted && request.applied.is_none() {
                return None;
            }
            if request.target == target && !request.admitted {
                return Some(request.token);
            }
        }
        let generation = self.resize_generation.checked_add(1)?;
        let owner_epoch = match self.resize_watermark {
            Some(stamp)
                if stamp.token.is_some_and(|token| token.owner == owner)
                    || (stamp.token.is_none() && self.resize_owner_epoch == stamp.owner_epoch)
                    || self.resize_request.as_ref().is_some_and(|request| {
                        request.token.owner == owner
                            && request.token.owner_epoch == stamp.owner_epoch
                    }) =>
            {
                stamp.owner_epoch
            }
            Some(stamp) => stamp.owner_epoch.checked_add(1)?,
            None => 1,
        };
        let token = runtime::ResizeToken {
            owner,
            owner_epoch,
            generation,
        };
        self.resize_generation = generation;
        self.resize_owner_epoch = owner_epoch;
        self.resize_request = Some(TrackedResize {
            token,
            target,
            admitted: false,
            applied: None,
            retry_at: None,
            retries: 0,
            retry_pending: None,
            retry_delivery: ProtocolRetryBackoff::default(),
        });
        self.arm_or_retarget_resize_presentation(target.0, target.1, std::time::Instant::now());
        Some(token)
    }

    fn resize_admitted(&mut self, token: runtime::ResizeToken, now: std::time::Instant) {
        if let Some(request) = self.resize_request.as_mut()
            && request.token == token
        {
            self.resize_tracking_started = true;
            request.admitted = true;
            if request.applied.is_none() {
                request.retry_at = Some(now + std::time::Duration::from_secs(2));
            }
        }
    }

    fn observe_resize_applied(
        &mut self,
        stamp: runtime::ResizeStamp,
        now: std::time::Instant,
    ) -> bool {
        if self
            .resize_watermark
            .is_some_and(|old| stamp.epoch < old.epoch || stamp.owner_epoch < old.owner_epoch)
        {
            return false;
        }
        self.resize_tracking_started = true;
        self.resize_watermark = Some(stamp);
        let Some(request) = self.resize_request.as_mut() else {
            return false;
        };
        if stamp.token != Some(request.token) || (stamp.cols, stamp.rows) != request.target {
            let superseded = stamp.owner_epoch > request.token.owner_epoch
                || request
                    .applied
                    .is_some_and(|applied| stamp.epoch > applied.epoch);
            if superseded {
                // 더 최신 실제 적용이 있으면 이전 quiet 후보를 나중에 승격하지 않는다.
                self.cancel_resize_request();
            }
            return false;
        }
        let first = request.applied.is_none();
        request.applied = Some(stamp);
        request.retry_at = None;
        if first && let Some(fence) = self.resize_presentation.as_mut() {
            fence.started_at = now;
            fence.latest_target = None;
            fence.last_target_at = None;
        }
        self.resize_desired == Some(request.target)
    }

    fn accepts_resize_viewport(
        &mut self,
        stamp: Option<runtime::ResizeStamp>,
        shape: (u16, u16),
        now: std::time::Instant,
    ) -> bool {
        let Some(stamp) = stamp else {
            return !self.resize_tracking_started && self.resize_request.is_none();
        };
        if shape != (stamp.cols, stamp.rows) {
            return false;
        }
        if self
            .resize_watermark
            .is_some_and(|old| stamp.epoch < old.epoch || stamp.owner_epoch < old.owner_epoch)
        {
            return false;
        }
        if self.resize_request.is_some() {
            return self.observe_resize_applied(stamp, now);
        }
        self.resize_tracking_started = true;
        self.resize_watermark = Some(stamp);
        true
    }

    fn cancel_resize_request(&mut self) {
        self.resize_request = None;
        self.resize_presentation = None;
        self.pending_snapshot = None;
    }

    fn resize_failed(&mut self, token: runtime::ResizeToken, reason: runtime::ResizeFailure) {
        if self
            .resize_request
            .as_ref()
            .is_some_and(|request| request.token == token)
            && !reason.retryable()
        {
            if matches!(
                reason,
                runtime::ResizeFailure::Conflict | runtime::ResizeFailure::Superseded
            ) && self.resize_owner_retries < 2
            {
                self.resize_recovery = Some(token);
            }
            self.cancel_resize_request();
        }
    }

    fn take_resize_owner_retry(&mut self, stamp: runtime::ResizeStamp) -> bool {
        let Some(rejected) = self.resize_recovery else {
            return false;
        };
        if stamp.owner_epoch < rejected.owner_epoch || stamp.token == Some(rejected) {
            return false;
        }
        self.resize_recovery = None;
        self.resize_owner_retries += 1;
        true
    }

    fn resize_retry_due(
        &mut self,
        now: std::time::Instant,
    ) -> Option<(runtime::ResizeToken, (u16, u16))> {
        let request = self.resize_request.as_mut()?;
        if request.retry_pending.is_some() || request.retry_at.is_none_or(|due| now < due) {
            return None;
        }
        if request.retries >= 2 {
            self.cancel_resize_request();
            return None;
        }
        request.retry_at = None;
        Some((request.token, request.target))
    }

    fn resize_retry_rejected(&mut self, token: runtime::ResizeToken, now: std::time::Instant) {
        let Some(request) = self
            .resize_request
            .as_mut()
            .filter(|request| request.token == token)
        else {
            return;
        };
        if request.applied.is_some() {
            return;
        }
        if request.retry_delivery.record_busy(now) {
            request.retry_at = request.retry_delivery.retry_at;
        } else {
            self.cancel_resize_request();
        }
    }

    fn resize_retry_completed(
        &mut self,
        token: runtime::ResizeToken,
        key: (WorkspaceProtocolOperation, u64),
        accepted: bool,
        now: std::time::Instant,
    ) {
        let Some(request) = self
            .resize_request
            .as_mut()
            .filter(|request| request.token == token && request.retry_pending == Some(key))
        else {
            return;
        };
        request.retry_pending = None;
        if request.applied.is_some() {
            return;
        }
        if accepted {
            request.retries += 1;
            request.retry_delivery = ProtocolRetryBackoff::default();
            request.retry_at = Some(now + std::time::Duration::from_secs(2));
        } else {
            self.resize_retry_rejected(token, now);
        }
    }

    fn install_snapshot(&mut self, snapshot: Arc<TerminalViewportSnapshot>) {
        self.summary = last_line_summary(&snapshot);
        self.snapshot = Some(snapshot);
        self.snapshot_gen = self.snapshot_gen.wrapping_add(1);
        self.pending_snapshot = None;
    }

    fn arm_resize_presentation(&mut self, cols: u16, rows: u16, now: std::time::Instant) {
        self.pending_snapshot = None;
        self.resize_presentation = Some(ResizePresentationFence {
            target: (cols, rows),
            started_at: now,
            last_target_at: None,
            latest_target: None,
            stable_had_visible_text: self
                .snapshot
                .as_deref()
                .is_some_and(snapshot_has_visible_text),
        });
    }

    /// 창 리사이즈로 나간 Resize도 split 최종 Resize와 같은 fence를 쓴다. fence가 이미
    /// 있으면 목표만 갈아끼워 hard deadline을 늘리지 않는다(기존 계약).
    ///
    /// 지킬 안정 화면이 **없으면**(스냅샷 미도착) 걸지 않는다 — `buffer_resize_snapshot`은
    /// fence가 있는 동안 target과 모양이 다른 viewport를 통째로 버리므로, 세션 생성 직후
    /// 첫 Resize에 걸면 첫 화면이 사라져 「연결 중」이 최대 250ms 남는다.
    fn arm_or_retarget_resize_presentation(
        &mut self,
        cols: u16,
        rows: u16,
        now: std::time::Instant,
    ) {
        if self.resize_presentation.is_some() {
            self.retarget_resize_presentation(cols, rows);
        } else if self.snapshot.is_some() {
            self.arm_resize_presentation(cols, rows, now);
        }
    }

    fn retarget_resize_presentation(&mut self, cols: u16, rows: u16) {
        let Some(fence) = self.resize_presentation.as_mut() else {
            return;
        };
        if fence.target != (cols, rows) {
            fence.target = (cols, rows);
            fence.last_target_at = None;
            fence.latest_target = None;
        }
    }

    fn arm_initial_presentation(&mut self, now: std::time::Instant) {
        if self.snapshot.is_none() && self.initial_presentation.is_none() {
            self.initial_presentation = Some(InitialSnapshotFence {
                started_at: now,
                latest_blank: None,
            });
        }
    }

    /// initial gate가 이벤트를 소비했는지 반환한다. 첫 nonblank는 즉시 설치하고,
    /// blank seed는 original deadline까지 latest-wins로 보관한다.
    fn buffer_initial_snapshot(
        &mut self,
        snapshot: Arc<TerminalViewportSnapshot>,
        now: std::time::Instant,
    ) -> bool {
        let Some(fence) = self.initial_presentation.as_ref() else {
            return false;
        };
        if snapshot_has_visible_text(&snapshot)
            || now.saturating_duration_since(fence.started_at) >= RESIZE_VIEWPORT_HARD_DEADLINE
        {
            self.initial_presentation = None;
            self.install_snapshot(snapshot);
            return true;
        }
        if let Some(fence) = self.initial_presentation.as_mut() {
            fence.latest_blank = Some(snapshot);
        }
        true
    }

    fn settle_initial_presentation(
        &mut self,
        now: std::time::Instant,
    ) -> Option<std::time::Duration> {
        let fence = self.initial_presentation.as_ref()?;
        let elapsed = now.saturating_duration_since(fence.started_at);
        if elapsed >= RESIZE_VIEWPORT_HARD_DEADLINE {
            let candidate = self
                .initial_presentation
                .take()
                .and_then(|fence| fence.latest_blank);
            if let Some(candidate) = candidate {
                self.install_snapshot(candidate);
            }
            return None;
        }
        Some(RESIZE_VIEWPORT_HARD_DEADLINE - elapsed)
    }

    /// resize fence가 있으면 viewport를 표시하지 않고 target 후보에만 보관한다.
    /// non-target은 resize 이전/중간 grid라 안정 화면을 덮을 수 없다.
    fn buffer_resize_snapshot(
        &mut self,
        snapshot: Arc<TerminalViewportSnapshot>,
        now: std::time::Instant,
    ) -> bool {
        let Some(fence) = self.resize_presentation.as_mut() else {
            return false;
        };
        if (snapshot.cols, snapshot.rows) == fence.target {
            fence.latest_target = Some(snapshot);
            fence.last_target_at = Some(now);
        }
        true
    }

    /// 승격 전이면 다음으로 확인할 one-shot repaint 지연을 반환한다.
    fn settle_resize_presentation(
        &mut self,
        now: std::time::Instant,
    ) -> Option<std::time::Duration> {
        if self.resize_request.as_ref().is_some_and(|request| {
            request.applied.is_none() || self.resize_desired != Some(request.target)
        }) {
            return None;
        }
        let fence = self.resize_presentation.as_ref()?;
        let hard_elapsed = now.saturating_duration_since(fence.started_at);
        let hard_expired = hard_elapsed >= RESIZE_VIEWPORT_HARD_DEADLINE;
        let quiet_ready = fence
            .last_target_at
            .is_some_and(|last| now.saturating_duration_since(last) >= RESIZE_VIEWPORT_QUIET);
        let candidate_allowed = fence.latest_target.as_deref().is_some_and(|candidate| {
            !fence.stable_had_visible_text || snapshot_has_visible_text(candidate)
        });
        let promote = hard_expired || (quiet_ready && candidate_allowed);

        if promote {
            let candidate = self
                .resize_presentation
                .take()
                .and_then(|fence| fence.latest_target);
            if let Some(candidate) = candidate {
                self.install_snapshot(candidate);
            }
            // 승격 이후에는 watermark가 역행을 막는다. 완료 token에 묶어 두면
            // legacy resize/backend 교체의 새 epoch 출력까지 영구 차단된다.
            self.resize_request = None;
            return None;
        }

        let hard_remaining = RESIZE_VIEWPORT_HARD_DEADLINE.saturating_sub(hard_elapsed);
        if candidate_allowed && let Some(last) = fence.last_target_at {
            let quiet_remaining =
                RESIZE_VIEWPORT_QUIET.saturating_sub(now.saturating_duration_since(last));
            Some(hard_remaining.min(quiet_remaining))
        } else {
            Some(hard_remaining)
        }
    }
}

impl WorkspaceUi {
    pub(crate) fn set_composer_height_expansion(&mut self, height: f32) {
        self.composer_height_expansion = if height.is_finite() { height } else { 0.0 };
    }

    pub fn with_resize_owner(owner: [u8; 16]) -> Self {
        let mut view = Self::new();
        view.resize_owner = owner;
        view
    }

    pub fn new() -> Self {
        Self {
            mux: None,
            composer_height_expansion: 0.0,
            cloud_answers: Arc::from([]),
            selected_cloud_answer: None,
            sessions: HashMap::new(),
            shell_kind: crate::ui::file_tree::default_shell_kind(),
            preedit: TerminalPreeditState::default(),
            pending_ime_submit: None,
            detached_ime_submit: None,
            flushed_ime_commit: None,
            native_printable_key_downs: Vec::new(),
            #[cfg(test)]
            test_native_key_downs: Vec::new(),
            native_clipboard_paste_requested: false,
            native_clipboard_copy_requested: false,
            prepared_attached_input_owner: None,
            prepared_attached_input_pass: None,
            prepared_attached_input_consumed: false,
            suppress_paste_request: false,
            suppress_copy_request: false,
            paste_suppressed: false,
            copy_suppressed: false,
            resize_owner: [1; 16],
            sent_sizes: HashMap::new(),
            pending_resize_target: HashMap::new(),
            scroll_residual: 0.0,
            drag_autoscroll_residual: 0.0,
            command_sent: false,
            last_focused_pane: None,
            session_flash: HashMap::new(),
            pending_focus: None,
            explicit_pending_focus: None,
            explicit_pending_focus_observed: false,
            terminal_focus_claimed: false,
            pending_spawn_cwds: VecDeque::with_capacity(WORKSPACE_PROTOCOL_CAP),
            split_drag: None,
            split_final_resize_sessions: HashSet::new(),
            split_final_resize_pending: HashMap::with_capacity(WORKSPACE_PROTOCOL_CAP),
            resize_delivery_rollbacks: HashMap::with_capacity(WORKSPACE_PROTOCOL_CAP),
            failed_resize_targets: HashMap::new(),
            resize_retry: HashMap::new(),
            staged_terminal_resizes: HashMap::new(),
            staged_split_commit_pass: None,
            confirm_close: None,
            agent_send_presets: Vec::new(),
            open_environment_requested: None,
            new_session_requested: None,
            session_folder_request: None,
            note_append_request: None,
            respawn_archived_request: None,
            selection: None,
            project_name: None,
            ui_scale: 1.0,
            workspace_accent: egui::Color32::TRANSPARENT,
            aux_tabs: Vec::new(),
            aux_tab_pane: None,
            session_pids: HashMap::new(),
            path_click_cache: None,
            io_generation: 1,
            next_io_operation: 1,
            io_intents: VecDeque::with_capacity(WORKSPACE_IO_QUEUE_CAP),
            protocol_generation: 1,
            next_protocol_operation: 1,
            protocol_intents: VecDeque::with_capacity(WORKSPACE_PROTOCOL_CAP),
            protocol_inflight: HashMap::with_capacity(WORKSPACE_PROTOCOL_CAP),
            protocol_retry_at: None,
            pending_path_resolution: None,
            last_dir_click: None,
            last_url_click: None,
            last_text_paste: None,
            last_native_paste: None,
            session_cwds: std::collections::HashMap::new(),
            session_project_names: SessionProjectNameSnapshot::default(),
            agent_info: std::collections::HashMap::new(),
            agent_executions: std::collections::HashMap::new(),
            selected_agent_prompts: VecDeque::new(),
            archived_resume_presentation: std::collections::HashMap::new(),
            pending_pastes: VecDeque::new(),
            native_error: None,
            error: None,
            error_is_pressure: false,
            protocol_request_lost: false,
            search: None,
            last_output_copy_pending: HashSet::new(),
            pending_copy: None,
            notice_intent: None,
            frame_counters: renderer_egui::RenderCounters::default(),
        }
    }

    /// 이번 프레임에 이 워크스페이스가 그린 터미널 렌더 카운터 합 (B1 실측).
    pub fn frame_counters(&self) -> renderer_egui::RenderCounters {
        self.frame_counters
    }

    /// Consume the one-frame terminal keyboard ownership claim. This is kept
    /// separate from runtime pane focus because Agents is rendered after the
    /// workspace and can otherwise re-request its deferred TextEdit focus.
    pub fn take_terminal_focus_claimed(&mut self) -> bool {
        std::mem::take(&mut self.terminal_focus_claimed)
    }

    /// 사이드바에서 세션 행을 눌러 그 pane으로 점프했을 때 **그 pane을 잠깐 강조**한다
    /// (2026-08-18 사용자 요청: 어디로 갔는지 보이게).
    ///
    /// 새 강조 기구를 만들지 않고 이미 있는 `session_flash`에 얹는다 — 포커스 이동
    /// (`FOCUS_FLASH`)과 상태 전이(`PANE_FLASH`)가 쓰는 바로 그 기구라 만료 정리·렌더·
    /// repaint 예약이 전부 갖춰져 있다. 별도 기구를 두면 흔한 경우(여러 pane 사이 점프)에
    /// **같은 pane에 같은 길이의 테두리가 두 겹**으로 그려진다.
    ///
    /// 이 진입점이 메우는 공백은 하나다 — `FOCUS_FLASH`는 `mux.focused_pane`이 **바뀔 때만**
    /// 뜨므로, 이미 보고 있던 세션(특히 pane이 하나뿐인 워크스페이스)을 다시 누르면 아무
    /// 확인 신호가 없었다. 여기서는 포커스가 바뀌든 말든 눌렀다는 사실 자체를 보여준다.
    ///
    /// pane에 세션이 없거나(연결 중) mux를 아직 못 받았으면 아무 일도 하지 않는다.
    pub(crate) fn flash_pane(&mut self, pane: &runtime::MuxPaneId) {
        let Some(session) = self
            .mux
            .as_ref()
            .and_then(|mux| {
                mux.tabs
                    .iter()
                    .flat_map(|tab| &tab.panes)
                    .find(|candidate| &candidate.id == pane)
            })
            .and_then(|pane| pane.session_id)
        else {
            return;
        };
        self.session_flash.insert(
            session,
            (std::time::Instant::now() + FOCUS_FLASH, FOCUS_FLASH),
        );
    }

    /// App이 저장 세션 복원/전환을 시작할 때 정확한 pane을 다음 터미널 입력 대상으로
    /// 예약한다. 실제 egui focus 요청은 그 pane의 surface가 렌더되는 첫 프레임에 소비된다.
    pub(crate) fn arm_terminal_focus(&mut self, pane: runtime::MuxPaneId) {
        self.explicit_pending_focus = Some(pane.clone());
        self.explicit_pending_focus_observed = self.mux.as_deref().is_some_and(|mux| {
            mux.tabs
                .iter()
                .flat_map(|tab| &tab.panes)
                .any(|candidate| candidate.id == pane)
        });
        self.begin_terminal_refocus(pane);
    }

    /// App이 보류 중인 포커스 대상이 stale임을 확인했거나 더 최신 네비게이션을 받았을 때
    /// 존재하지 않는 pane이 터미널 입력을 독점하지 않도록 예약을 취소한다.
    pub(crate) fn cancel_terminal_focus(&mut self) {
        self.pending_focus = None;
        self.explicit_pending_focus = None;
        self.explicit_pending_focus_observed = false;
        self.flush_pending_ime_submit();
        self.preedit.clear();
    }

    fn reconcile_explicit_terminal_focus(&mut self) {
        let Some(expected) = self.explicit_pending_focus.as_ref() else {
            return;
        };
        let pane_present = self.mux.as_deref().is_some_and(|mux| {
            mux.tabs
                .iter()
                .flat_map(|tab| &tab.panes)
                .any(|candidate| &candidate.id == expected)
        });
        self.explicit_pending_focus_observed |= pane_present;
        if self.explicit_pending_focus_observed && !pane_present {
            self.cancel_terminal_focus();
        }
    }

    /// Drains the latest native notice for `App::logic` to execute. Empty reads
    /// perform no I/O and schedule no polling or repaint lifecycle.
    pub fn take_notice_intent(&mut self) -> Option<WorkspaceNotice> {
        self.notice_intent.take()
    }

    fn stage_notice(&mut self, summary: String, body: &str) {
        if let Some(notice) = WorkspaceNotice::try_new(summary, body) {
            self.notice_intent = Some(notice);
        }
    }

    /// 세션 스냅샷이 하나라도 도착했는가 — 워크스페이스 생성 burst의 `first_snapshot`
    /// 단계 판정용 (B1). 세션이 없으면 false.
    pub fn any_snapshot(&self) -> bool {
        self.sessions.values().any(|view| view.snapshot.is_some())
    }

    fn next_io_operation(&mut self) -> WorkspaceIoOperation {
        let operation = WorkspaceIoOperation(self.next_io_operation);
        self.next_io_operation = self.next_io_operation.wrapping_add(1).max(1);
        operation
    }

    fn queue_io_intent(&mut self, intent: WorkspaceIoIntent) -> Result<(), WorkspaceIoErrorCode> {
        if matches!(intent, WorkspaceIoIntent::ResolvePath { .. }) {
            // 마우스가 움직이며 만든 조회는 최신 위치만 필요하다. 사용자 작업은 보존한다.
            self.io_intents
                .retain(|queued| !matches!(queued, WorkspaceIoIntent::ResolvePath { .. }));
        } else if self
            .io_intents
            .iter()
            .filter(|queued| !matches!(queued, WorkspaceIoIntent::ResolvePath { .. }))
            .count()
            >= WORKSPACE_IO_QUEUE_CAP
        {
            return Err(WorkspaceIoErrorCode::Busy);
        }
        self.io_intents.push_back(intent);
        Ok(())
    }

    /// 사용자 작업 최대 8개와 미리보기 1개를 보관하며, 사용자 작업부터 실행한다.
    /// 실행 권한만 넘긴다. 큐가 비면 idle thread/network/polling이 생기지 않는다.
    pub fn take_io_intent(&mut self) -> Option<WorkspaceIoIntent> {
        self.expire_pending_pastes();
        let index = self
            .io_intents
            .iter()
            .position(|intent| !matches!(intent, WorkspaceIoIntent::ResolvePath { .. }))
            .unwrap_or(0);
        self.io_intents.remove(index)
    }

    fn expire_pending_pastes(&mut self) {
        let before = self.pending_pastes.len();
        self.pending_pastes
            .retain(|pending| pending.requested_at.elapsed() <= PASTE_TASK_TTL);
        if self.pending_pastes.len() != before {
            self.native_error = Some(WorkspaceNativeError::ClipboardExpired);
            self.io_intents.retain(|intent| match intent {
                WorkspaceIoIntent::ReadTerminalClipboard {
                    operation,
                    generation,
                } => self.pending_pastes.iter().any(|pending| {
                    pending.operation == *operation && pending.generation == *generation
                }),
                _ => true,
            });
        }
    }

    /// 상단 배너 대신 App의 알림 센터가 오류를 한 번씩 가져간다. warm 작업도 동일하다.
    pub fn take_error_notice(&mut self, catalog: &i18n::Catalog) -> Option<WorkspaceErrorNotice> {
        self.expire_pending_pastes();
        if let Some(error) = self.native_error.take() {
            let (kind, key) = match error {
                WorkspaceNativeError::Busy => {
                    (WorkspaceErrorKind::NativeBusy, "workspace.native_busy")
                }
                WorkspaceNativeError::ClipboardFailed => (
                    WorkspaceErrorKind::ClipboardFailed,
                    "workspace.clipboard_failed",
                ),
                WorkspaceNativeError::ClipboardTooLarge => (
                    WorkspaceErrorKind::ClipboardTooLarge,
                    "workspace.clipboard_too_large",
                ),
                WorkspaceNativeError::ClipboardExpired => (
                    WorkspaceErrorKind::ClipboardExpired,
                    "workspace.clipboard_expired",
                ),
                WorkspaceNativeError::PathRejected => {
                    (WorkspaceErrorKind::PathRejected, "workspace.path_rejected")
                }
                WorkspaceNativeError::UrlRejected => {
                    (WorkspaceErrorKind::UrlRejected, "workspace.url_rejected")
                }
            };
            return Some(WorkspaceErrorNotice {
                kind,
                message: catalog.t(key, &[]),
            });
        }
        if std::mem::take(&mut self.protocol_request_lost) {
            return Some(WorkspaceErrorNotice {
                kind: WorkspaceErrorKind::ProtocolRequestLost,
                message: catalog.t("workspace.protocol_request_lost", &[]),
            });
        }
        self.error.take().map(|message| WorkspaceErrorNotice {
            kind: if self.error_is_pressure {
                WorkspaceErrorKind::InputPressure
            } else {
                WorkspaceErrorKind::Other
            },
            message,
        })
    }

    fn next_protocol_operation(&mut self) -> (WorkspaceProtocolOperation, u64) {
        let operation = WorkspaceProtocolOperation(self.next_protocol_operation);
        let generation = self.protocol_generation;
        self.next_protocol_operation = self.next_protocol_operation.wrapping_add(1);
        if self.next_protocol_operation == 0 {
            self.next_protocol_operation = 1;
            self.protocol_generation = self.protocol_generation.wrapping_add(1).max(1);
        }
        (operation, generation)
    }

    fn queue_protocol_intent(
        &mut self,
        command: RuntimeCommand,
    ) -> Result<(), WorkspaceProtocolErrorCode> {
        self.queue_protocol_intent_tracked(command).map(|_| ())
    }

    fn queue_protocol_intent_tracked(
        &mut self,
        command: RuntimeCommand,
    ) -> Result<(WorkspaceProtocolOperation, u64), WorkspaceProtocolErrorCode> {
        self.queue_protocol_intent_tracked_with_spawn_cwd(command, None)
    }

    fn queue_terminal_resize_tracked(
        &mut self,
        session: SessionId,
        cols: u16,
        rows: u16,
    ) -> Option<(WorkspaceProtocolOperation, u64)> {
        if self.sent_sizes.get(&session) == Some(&(cols, rows)) {
            return None;
        }
        if self.failed_resize_targets.get(&session) == Some(&(cols, rows)) {
            return None;
        }
        self.failed_resize_targets.remove(&session);
        let previous = self.sent_sizes.get(&session).copied();
        let view = self.sessions.entry(session).or_default();
        let prior_request = view.resize_request;
        let prior_fence = view.resize_presentation.clone();
        let prior_pending = view.pending_snapshot.clone();
        let prior_owner_epoch = view.resize_owner_epoch;
        let prior_generation = view.resize_generation;
        let prior_desired = view.resize_desired;
        let prior_owner_retries = view.resize_owner_retries;
        let prior_recovery = view.resize_recovery;
        let token = view.prepare_tracked_resize(self.resize_owner, (cols, rows))?;
        let key = match self.queue_protocol_intent_tracked(RuntimeCommand::ResizeTracked {
            token,
            session,
            cols,
            rows,
        }) {
            Ok(key) => key,
            Err(_) => {
                // 로컬 큐 거부는 실제 요청이 아니다. 기존 표시/진행 중 요청을 복구한다.
                let view = self.sessions.get_mut(&session).unwrap();
                view.resize_request = prior_request;
                view.resize_presentation = prior_fence;
                view.pending_snapshot = prior_pending;
                view.resize_owner_epoch = prior_owner_epoch;
                view.resize_generation = prior_generation;
                view.resize_desired = prior_desired;
                view.resize_owner_retries = prior_owner_retries;
                view.resize_recovery = prior_recovery;
                return None;
            }
        };
        self.resize_delivery_rollbacks
            .entry(key)
            .and_modify(|rollback| {
                if rollback.ack_retry {
                    rollback.previous = previous;
                }
                rollback.ack_retry = false;
                rollback.target = (cols, rows);
                rollback.token = token;
            })
            .or_insert(ResizeDeliveryRollback {
                session,
                token,
                ack_retry: false,
                target: (cols, rows),
                previous,
            });
        if let Some((pending_session, pending_cols, pending_rows)) =
            self.split_final_resize_pending.get_mut(&key)
            && *pending_session == session
        {
            *pending_cols = cols;
            *pending_rows = rows;
        }
        self.sent_sizes.insert(session, (cols, rows));
        Some(key)
    }

    fn queue_terminal_resize(&mut self, session: SessionId, cols: u16, rows: u16) -> bool {
        self.queue_terminal_resize_tracked(session, cols, rows)
            .is_some()
    }

    fn apply_split_final_resize_at(
        &mut self,
        session: SessionId,
        cols: u16,
        rows: u16,
        _now: std::time::Instant,
    ) -> bool {
        if !self.split_final_resize_sessions.contains(&session) {
            return false;
        }
        if self.sent_sizes.get(&session) == Some(&(cols, rows)) {
            let applied = self
                .sessions
                .get(&session)
                .and_then(|view| view.resize_request.as_ref())
                .is_none_or(|request| {
                    request.target == (cols, rows)
                        && request.applied.is_some()
                        && self.sessions[&session]
                            .resize_presentation
                            .as_ref()
                            .is_none_or(|fence| fence.latest_target.is_some())
                });
            if !applied {
                return false;
            }
        }
        if self.sent_sizes.get(&session) == Some(&(cols, rows))
            || self.failed_resize_targets.get(&session) == Some(&(cols, rows))
        {
            self.split_final_resize_sessions.remove(&session);
            self.pending_resize_target.remove(&session);
            return false;
        }
        let Some(key) = self.queue_terminal_resize_tracked(session, cols, rows) else {
            return false;
        };

        self.split_final_resize_sessions.remove(&session);
        self.pending_resize_target.remove(&session);
        self.split_final_resize_pending
            .insert(key, (session, cols, rows));
        true
    }

    fn finish_resize_viewport(&mut self, session: SessionId, stamp: runtime::ResizeStamp) {
        // 다른 창/legacy resize가 실제 격자를 바꾸면 이전 queue 크기로 중복 제거하지 않는다.
        if self
            .sessions
            .get(&session)
            .is_some_and(|view| view.resize_request.is_none())
            && self
                .sent_sizes
                .get(&session)
                .is_some_and(|size| *size != (stamp.cols, stamp.rows))
        {
            self.sent_sizes.remove(&session);
        }
        if self
            .sessions
            .get_mut(&session)
            .is_some_and(|view| view.take_resize_owner_retry(stamp))
        {
            self.sent_sizes.remove(&session);
            self.failed_resize_targets.remove(&session);
        }
        let Some(token) = stamp.token else {
            return;
        };
        let Some(request) = self
            .sessions
            .get(&session)
            .and_then(|view| view.resize_request.as_ref())
        else {
            return;
        };
        if request.token != token || request.applied != Some(stamp) {
            return;
        }
        let keys: Vec<_> = self
            .resize_delivery_rollbacks
            .iter()
            .filter(|(_, rollback)| rollback.session == session && rollback.token == token)
            .map(|(key, _)| *key)
            .collect();
        for key in keys {
            self.resize_delivery_rollbacks.remove(&key);
            self.split_final_resize_pending.remove(&key);
        }
        if self.sessions[&session].resize_desired == Some(request.target) {
            self.split_final_resize_sessions.remove(&session);
            if self.sessions[&session].resize_presentation.is_none() {
                self.sessions.get_mut(&session).unwrap().resize_request = None;
            }
        }
    }

    fn resize_presentation_settlement_deferred(&self, session: SessionId) -> bool {
        self.split_drag.is_some()
            || self.split_final_resize_sessions.contains(&session)
            || self
                .split_final_resize_pending
                .values()
                .any(|(pending, _, _)| *pending == session)
    }

    fn settle_session_resize_presentation(
        &mut self,
        session: SessionId,
        now: std::time::Instant,
    ) -> Option<std::time::Duration> {
        if self.resize_presentation_settlement_deferred(session) {
            return None;
        }
        let view = self.sessions.get_mut(&session)?;
        let generation_before = view.snapshot_gen;
        let repaint_after = view.settle_resize_presentation(now);
        if view.snapshot_gen != generation_before
            && self
                .selection
                .is_some_and(|(selected, _, _)| selected == session)
        {
            self.selection = None;
        }
        repaint_after
    }

    /// `queue_terminal_resize`의 디바운스 래퍼 — pane 렌더 호출부는 매 프레임 이걸 부른다.
    ///
    /// 이 세션의 **첫 크기**(세션 생성·복원 — `sent_sizes`에 항목이 없는 경우)만 지연
    /// 없이 즉시 보낸다. 그 뒤의 모든 크기 변경은 전송을 미루고 `pending_resize_target`에
    /// 목표만 갱신한다 — 그러지 않으면 매 중간 크기마다 PTY가 실제로 reflow하고 자식
    /// 프로세스가 SIGWINCH로 화면을 다시 그려 드래그 내내 깜빡인다. 같은 목표가
    /// `RESIZE_DRAG_DEBOUNCE`만큼 유지되면(=드래그가 그 크기에서 멈췄다) 그제서야 보낸다.
    ///
    /// 「첫 mismatch」의 판정 기준이 **보류 유무가 아니라 `sent_sizes` 유무**인 것이
    /// 핵심이다. 보류를 기준으로 삼으면, 목표가 직전 전송값과 같은 프레임에서 아래
    /// 최상단 가드가 보류를 지우기 때문에 **다음 변경이 매번 「첫 mismatch」로 오인**된다.
    /// 사람이 실제로 창을 끄는 속도에서는 한 칸마다 그렇게 머무는 프레임이 생겨,
    /// 그리드 한 칸 옮길 때마다 SIGWINCH가 나가 화면이 심하게 깜빡였다(2026-09-06 보고).
    /// 빠른 드래그는 머무는 프레임이 없어 증상이 나타나지 않는다 — 그래서 2026-08-18에
    /// 디바운스를 넣고도 잡히지 않았다.
    ///
    /// 드래그가 끝나 더 이상 새 프레임이 오지 않아도 최종 목표가 유실되지 않도록, 목표를
    /// 갱신할 때마다 `request_repaint_after`로 debounce 만료 시점에 다시 확인하러 오는
    /// repaint를 예약한다 — 그 시점에 다른 입력이 전혀 없어도 이 경로가 다시 실행된다.
    fn queue_terminal_resize_debounced(
        &mut self,
        ctx: &egui::Context,
        session: SessionId,
        cols: u16,
        rows: u16,
    ) -> bool {
        self.sessions
            .entry(session)
            .or_default()
            .set_resize_desired((cols, rows));
        if self.sent_sizes.get(&session) == Some(&(cols, rows)) {
            self.pending_resize_target.remove(&session);
            return false;
        }
        let now = std::time::Instant::now();
        let viewport_size = ctx.input(|input| input.viewport().inner_rect.map(|rect| rect.size()));
        match self.pending_resize_target.get(&session).copied() {
            None if !self.sent_sizes.contains_key(&session) => {
                // 이 세션에 아직 한 번도 크기를 보낸 적이 없다 — 세션 생성/복원처럼 진짜
                // 1회성이라 지연 없이 즉시 보낸다.
                self.pending_resize_target
                    .insert(session, (cols, rows, viewport_size, now));
                self.queue_terminal_resize(session, cols, rows)
            }
            None => {
                // 이미 크기를 보낸 세션의 새 목표 — 드래그의 첫 칸일 수 있다. 목표만
                // 기록하고 안정될 때까지 기다린다(아래 Some 분기와 동일한 시계).
                self.pending_resize_target
                    .insert(session, (cols, rows, viewport_size, now));
                ctx.request_repaint_after(RESIZE_DRAG_DEBOUNCE);
                false
            }
            Some((pending_cols, pending_rows, pending_viewport_size, since))
                if (pending_cols, pending_rows) == (cols, rows)
                    && pending_viewport_size == viewport_size =>
            {
                // 직전과 같은 목표 — 안정 여부만 판정한다.
                let elapsed = now.duration_since(since);
                if elapsed < RESIZE_DRAG_DEBOUNCE {
                    ctx.request_repaint_after(RESIZE_DRAG_DEBOUNCE - elapsed);
                    return false;
                }
                let admitted = self.queue_terminal_resize(session, cols, rows);
                // 전송이 **성사됐을 때만** 보류를 지운다. 프로토콜 큐가 가득 차
                // queue_terminal_resize가 삼켜버린 경우 보류를 지우면 다음 프레임이
                // None 분기로 떨어져 즉시 재전송하고, 그 실패가 다시 이 분기로 와서
                // 120ms짜리 repaint 예약을 무한히 갱신한다 — 큐가 계속 막혀 있으면
                // 앱이 영영 유휴 상태로 못 내려간다. 보류를 남겨 두면 elapsed가 계속
                // 만료 상태라 repaint를 예약하지 않고, 자연히 발생하는 프레임에서만
                // 재시도한다(디바운스 도입 전과 같은 재시도 성격).
                if self.sent_sizes.get(&session) == Some(&(cols, rows)) {
                    self.pending_resize_target.remove(&session);
                }
                admitted
            }
            Some(_) => {
                // 목표가 직전 프레임과 또 달라졌다 — 드래그가 계속되는 중. 디바운스
                // 시계를 새로 시작한다(전송하지 않는다).
                self.pending_resize_target
                    .insert(session, (cols, rows, viewport_size, now));
                ctx.request_repaint_after(RESIZE_DRAG_DEBOUNCE);
                false
            }
        }
    }

    fn begin_split_drag(&mut self, tab: runtime::MuxTabId, path: Vec<u8>, ratio: f32) {
        match self.split_drag.as_mut() {
            Some(transaction)
                if transaction.tab == tab
                    && transaction.path == path
                    && transaction.phase == SplitDragPhase::Active =>
            {
                transaction.ratio = ratio;
            }
            _ => {
                self.split_drag = Some(SplitDragTransaction {
                    tab,
                    path,
                    ratio,
                    phase: SplitDragPhase::Active,
                    pending_delivery: None,
                    retry: ProtocolRetryBackoff::default(),
                });
            }
        }
        // 직전 geometry의 debounce/final 후보가 새 drag 중에 늦게 실행되면 중간
        // SIGWINCH가 된다. sent_sizes는 마지막 stable grid 기준으로 보존한다.
        self.pending_resize_target.clear();
        self.staged_terminal_resizes.clear();
        self.split_final_resize_sessions.clear();
    }

    fn cancel_active_split_drag(&mut self, ctx: &egui::Context) {
        let Some(handle_id) = self
            .split_drag
            .as_ref()
            .filter(|transaction| transaction.phase == SplitDragPhase::Active)
            .map(|transaction| split_handle_id(&transaction.tab, &transaction.path))
        else {
            return;
        };
        if ctx.dragged_id() == Some(handle_id) {
            ctx.stop_dragging();
        }
        self.split_drag = None;
        self.staged_split_commit_pass = None;
        self.staged_terminal_resizes.clear();
        self.pending_resize_target.clear();
        self.split_final_resize_sessions.clear();
    }

    fn reconcile_active_split_drag(
        &mut self,
        ctx: &egui::Context,
        input_enabled: bool,
        active_tab: Option<&runtime::MuxTabId>,
    ) {
        let Some(transaction) = self
            .split_drag
            .as_ref()
            .filter(|transaction| transaction.phase == SplitDragPhase::Active)
        else {
            return;
        };
        let handle_id = split_handle_id(&transaction.tab, &transaction.path);
        let owns_widget = input_enabled
            && ctx.input(|input| input.focused)
            && active_tab == Some(&transaction.tab)
            && self.mux.as_deref().is_some_and(|mux| {
                mux_split_ratio(mux, &transaction.tab, &transaction.path).is_some()
            });
        if !owns_widget {
            self.cancel_active_split_drag(ctx);
            return;
        }
        let is_being_dragged = ctx.is_being_dragged(handle_id);
        let another_widget_owns_drag = ctx.dragged_id().is_some() && !is_being_dragged;
        if another_widget_owns_drag {
            self.cancel_active_split_drag(ctx);
            return;
        }
        if ctx.drag_stopped_id() == Some(handle_id) {
            self.commit_split_drag(ctx.cumulative_pass_nr());
            return;
        }

        let primary_down = ctx.input(|input| input.pointer.primary_down());
        if !is_being_dragged && !primary_down {
            self.cancel_active_split_drag(ctx);
        }
    }

    fn commit_split_drag(&mut self, pass: u64) {
        let Some(transaction) = self.split_drag.as_mut() else {
            return;
        };
        if transaction.phase == SplitDragPhase::Active {
            transaction.phase = SplitDragPhase::Committed { admitted: false };
        }
        if matches!(
            transaction.phase,
            SplitDragPhase::Committed { admitted: false }
        ) {
            self.staged_split_commit_pass = Some(pass);
        }
    }

    fn stage_unadmitted_split_commit_for_pass(
        &mut self,
        ctx: &egui::Context,
        pass: u64,
        sizing_pass: bool,
    ) {
        if sizing_pass {
            return;
        }
        let should_stage = self.split_drag.as_mut().is_some_and(|transaction| {
            if !matches!(
                transaction.phase,
                SplitDragPhase::Committed { admitted: false }
            ) || transaction.pending_delivery.is_some()
            {
                return false;
            }
            match transaction.retry.gate(std::time::Instant::now()) {
                ProtocolRetryGate::Ready => true,
                ProtocolRetryGate::Wait(after) => {
                    ctx.request_repaint_after(after);
                    false
                }
                ProtocolRetryGate::Exhausted => false,
            }
        });
        if should_stage {
            self.staged_split_commit_pass = Some(pass);
        }
    }

    fn split_preview_ratio(&self, tab: &runtime::MuxTabId, path: &[u8], persisted: f32) -> f32 {
        self.split_drag
            .as_ref()
            .filter(|transaction| &transaction.tab == tab && transaction.path == path)
            .map_or(persisted, |transaction| transaction.ratio)
    }

    fn stage_terminal_resize_for_pass(
        &mut self,
        pass: u64,
        sizing_pass: bool,
        session: SessionId,
        cols: u16,
        rows: u16,
    ) {
        if sizing_pass || self.split_drag.is_some() {
            return;
        }
        self.staged_terminal_resizes
            .insert(session, StagedTerminalResize { pass, cols, rows });
    }

    fn flush_render_side_effects_for_pass(
        &mut self,
        ctx: &egui::Context,
        pass: u64,
        will_discard: bool,
    ) -> bool {
        if will_discard {
            return false;
        }

        let mut admitted = false;

        if self.staged_split_commit_pass == Some(pass) {
            let command = self.split_drag.as_ref().and_then(|transaction| {
                (matches!(
                    transaction.phase,
                    SplitDragPhase::Committed { admitted: false }
                ) && transaction.pending_delivery.is_none())
                .then(|| RuntimeCommand::ResizeSplit {
                    tab: transaction.tab.clone(),
                    path: transaction.path.clone(),
                    ratio: transaction.ratio,
                })
            });
            if let Some(command) = command {
                match self.queue_protocol_intent_tracked(command) {
                    Ok(key) => {
                        if let Some(transaction) = self.split_drag.as_mut()
                            && matches!(
                                transaction.phase,
                                SplitDragPhase::Committed { admitted: false }
                            )
                        {
                            transaction.pending_delivery = Some(key);
                            admitted = true;
                        }
                    }
                    Err(code) => self.report_protocol_queue_rejection(code, "protocol_queue"),
                }
            }
            self.staged_split_commit_pass = None;
        }

        let now = std::time::Instant::now();
        let mut exhausted = Vec::new();
        let retries = self
            .sessions
            .iter_mut()
            .filter_map(|(session, view)| {
                let had_request = view.resize_request.is_some();
                let retry = view.resize_retry_due(now);
                if had_request && view.resize_request.is_none() {
                    exhausted.push(*session);
                }
                retry.map(|(token, (cols, rows))| (*session, token, cols, rows))
            })
            .collect::<Vec<_>>();
        for (session, token, cols, rows) in retries {
            match self.queue_protocol_intent_tracked(RuntimeCommand::ResizeTracked {
                session,
                token,
                cols,
                rows,
            }) {
                Ok(key) => {
                    if let Some(request) = self
                        .sessions
                        .get_mut(&session)
                        .and_then(|view| view.resize_request.as_mut())
                    {
                        request.retry_pending = Some(key);
                    }
                    self.resize_delivery_rollbacks.insert(
                        key,
                        ResizeDeliveryRollback {
                            session,
                            token,
                            ack_retry: true,
                            target: (cols, rows),
                            previous: None,
                        },
                    );
                    admitted = true;
                }
                Err(_) => {
                    let view = self.sessions.get_mut(&session).unwrap();
                    view.resize_retry_rejected(token, now);
                    if view.resize_request.is_none() {
                        exhausted.push(session);
                    }
                }
            }
        }
        self.resize_delivery_rollbacks.retain(|_, rollback| {
            self.sessions
                .get(&rollback.session)
                .and_then(|view| view.resize_request.as_ref())
                .is_some_and(|request| request.token == rollback.token)
        });
        self.split_final_resize_pending
            .retain(|key, _| self.resize_delivery_rollbacks.contains_key(key));
        for session in exhausted {
            self.split_final_resize_sessions.remove(&session);
        }
        for view in self.sessions.values() {
            if let Some(due) = view
                .resize_request
                .as_ref()
                .and_then(|request| request.retry_at)
            {
                ctx.request_repaint_after(due.saturating_duration_since(now));
            }
        }

        let mut staged = std::mem::take(&mut self.staged_terminal_resizes);
        for (session, resize) in staged.drain() {
            if resize.pass != pass || self.split_drag.is_some() {
                continue;
            }
            if let Some(retry) = self.resize_retry.get_mut(&session) {
                match retry.gate(std::time::Instant::now()) {
                    ProtocolRetryGate::Ready => {}
                    ProtocolRetryGate::Wait(after) => {
                        ctx.request_repaint_after(after);
                        continue;
                    }
                    ProtocolRetryGate::Exhausted => continue,
                }
            }
            if self.split_final_resize_sessions.contains(&session) {
                let final_resize_admitted = self.apply_split_final_resize_at(
                    session,
                    resize.cols,
                    resize.rows,
                    std::time::Instant::now(),
                );
                admitted |= final_resize_admitted;
                // 같은 grid면 protocol은 보내지 않아도 marker는 소비된다. pane settlement는
                // 이 tail flush보다 앞서 이미 보류됐으므로 다음 pass를 한 번 깨운다.
                if !final_resize_admitted && !self.split_final_resize_sessions.contains(&session) {
                    ctx.request_repaint();
                }
            } else {
                admitted |=
                    self.queue_terminal_resize_debounced(ctx, session, resize.cols, resize.rows);
            }
        }
        self.staged_terminal_resizes = staged;
        admitted
    }

    /// App host의 마지막 UI-producing widget 뒤에서만 호출한다. workspace 렌더 시점의
    /// `will_discard`는 뒤쪽 widget이 나중에 discard를 요청할 수 있어 final 판별이 아니다.
    pub fn flush_render_side_effects(&mut self, ctx: &egui::Context) {
        if self.flush_render_side_effects_for_pass(
            ctx,
            ctx.cumulative_pass_nr(),
            ctx.will_discard(),
        ) {
            ctx.request_repaint();
        }
    }

    fn queue_protocol_intent_with_spawn_cwd(
        &mut self,
        command: RuntimeCommand,
        spawn_cwd: Option<String>,
    ) -> Result<(), WorkspaceProtocolErrorCode> {
        self.queue_protocol_intent_tracked_with_spawn_cwd(command, spawn_cwd)
            .map(|_| ())
    }

    fn queue_protocol_intent_tracked_with_spawn_cwd(
        &mut self,
        command: RuntimeCommand,
        spawn_cwd: Option<String>,
    ) -> Result<(WorkspaceProtocolOperation, u64), WorkspaceProtocolErrorCode> {
        self.queue_protocol_intent_owned(command, spawn_cwd)
            .map_err(|(code, _)| code)
    }

    fn retained_protocol_input_bytes(&self) -> usize {
        self.protocol_intents
            .iter()
            .map(|intent| protocol_input_bytes(&intent.command))
            .sum::<usize>()
            + self
                .protocol_inflight
                .values()
                .map(|pending| pending.input_bytes)
                .sum::<usize>()
    }

    fn queue_protocol_intent_owned(
        &mut self,
        mut command: RuntimeCommand,
        spawn_cwd: Option<String>,
    ) -> Result<(WorkspaceProtocolOperation, u64), (WorkspaceProtocolErrorCode, Box<RuntimeCommand>)>
    {
        if let Err(code) = workspace_protocol_command_is_valid(&command) {
            return Err((code, Box::new(command)));
        }
        let retained_input_bytes = self.retained_protocol_input_bytes();
        if let RuntimeCommand::WriteInput { bytes, .. } = &mut command {
            // Do this once at ingestion, never on idle frames or host retries.
            if bytes.capacity() != bytes.len() {
                *bytes = std::mem::take(bytes).into_boxed_slice().into_vec();
            }
        }
        let spawn = matches!(
            &command,
            RuntimeCommand::SpawnShell { .. } | RuntimeCommand::SplitPane { .. }
        );
        if spawn
            && self
                .pending_spawn_cwds
                .len()
                .saturating_add(
                    self.protocol_intents
                        .iter()
                        .filter(|intent| {
                            matches!(
                                &intent.command,
                                RuntimeCommand::SpawnShell { .. }
                                    | RuntimeCommand::SplitPane { .. }
                            )
                        })
                        .count(),
                )
                .saturating_add(
                    self.protocol_inflight
                        .values()
                        .filter(|pending| pending.spawn)
                        .count(),
                )
                >= WORKSPACE_PROTOCOL_CAP
        {
            return Err((WorkspaceProtocolErrorCode::Busy, Box::new(command)));
        }

        if let RuntimeCommand::ResizeTracked { session, .. } = &command
            && let Some(intent) = self.protocol_intents.iter_mut().find(|intent|
                matches!(&intent.command, RuntimeCommand::ResizeTracked { session: queued, .. } if queued == session)) {
                intent.command = command;
                self.command_sent = true;
                return Ok((intent.operation, intent.generation));
            }

        if let RuntimeCommand::Resize {
            session: next_session,
            cols: next_cols,
            rows: next_rows,
        } = &command
            && let Some(WorkspaceProtocolIntent {
                operation,
                generation,
                command:
                    RuntimeCommand::Resize {
                        session: queued_session,
                        cols: queued_cols,
                        rows: queued_rows,
                    },
                ..
            }) = self.protocol_intents.iter_mut().find(|intent| {
                matches!(
                    &intent.command,
                    RuntimeCommand::Resize { session, .. } if session == next_session
                )
            })
        {
            debug_assert_eq!(queued_session, next_session);
            *queued_cols = *next_cols;
            *queued_rows = *next_rows;
            self.command_sent = true;
            return Ok((*operation, *generation));
        }

        if let RuntimeCommand::WriteInput {
            session: next_session,
            bytes: next_bytes,
        } = &command
            && let Some(WorkspaceProtocolIntent {
                operation,
                generation,
                command:
                    RuntimeCommand::WriteInput {
                        session: queued_session,
                        bytes: queued_bytes,
                    },
                ..
            }) = self.protocol_intents.back_mut()
            && queued_session == next_session
        {
            let combined = queued_bytes.len().saturating_add(next_bytes.len());
            if combined <= WORKSPACE_PROTOCOL_INPUT_MAX_BYTES {
                let other_bytes = retained_input_bytes - queued_bytes.capacity();
                let available = TERMINAL_PROTOCOL_RETAINED_INPUT_MAX_BYTES - other_bytes;
                if combined > available {
                    return Err((
                        WorkspaceProtocolErrorCode::PayloadTooLarge,
                        Box::new(command),
                    ));
                }
                // Geometric growth amortizes new gestures. Charge capacity (not length), and
                // fall back to exact growth when the aggregate ceiling has less headroom.
                let target = combined
                    .next_power_of_two()
                    .min(WORKSPACE_PROTOCOL_INPUT_MAX_BYTES)
                    .min(available);
                if target > queued_bytes.capacity() {
                    queued_bytes.reserve_exact(target - queued_bytes.len());
                }
                if other_bytes + queued_bytes.capacity()
                    > TERMINAL_PROTOCOL_RETAINED_INPUT_MAX_BYTES
                {
                    *queued_bytes = std::mem::take(queued_bytes).into_boxed_slice().into_vec();
                    return Err((
                        WorkspaceProtocolErrorCode::PayloadTooLarge,
                        Box::new(command),
                    ));
                }
                queued_bytes.extend_from_slice(next_bytes);
                self.command_sent = true;
                return Ok((*operation, *generation));
            }
            // A full command is not a full queue: retain the next whole gesture in its own
            // ordered entry when the per-command cap prevents adjacent coalescing.
        }

        if retained_input_bytes.saturating_add(protocol_input_bytes(&command))
            > TERMINAL_PROTOCOL_RETAINED_INPUT_MAX_BYTES
        {
            return Err((
                WorkspaceProtocolErrorCode::PayloadTooLarge,
                Box::new(command),
            ));
        }
        if self
            .protocol_intents
            .len()
            .saturating_add(self.protocol_inflight.len())
            >= WORKSPACE_PROTOCOL_CAP
        {
            return Err((WorkspaceProtocolErrorCode::Busy, Box::new(command)));
        }
        let (operation, generation) = self.next_protocol_operation();
        self.protocol_intents.push_back(WorkspaceProtocolIntent {
            operation,
            generation,
            command,
            spawn_cwd,
        });
        self.command_sent = true;
        Ok((operation, generation))
    }

    /// Drains one validated protocol request for the composition root. A taken request occupies
    /// a bounded slot until an exact completion is applied. The pressure reserve is shared by
    /// queued and in-flight terminal gestures, preventing a hidden host backlog.
    pub fn take_protocol_intent(&mut self) -> Option<WorkspaceProtocolIntent> {
        if self
            .protocol_retry_at
            .is_some_and(|at| std::time::Instant::now() < at)
        {
            return None;
        }
        self.protocol_retry_at = None;
        let mut intent = self.protocol_intents.pop_front()?;
        let key = (intent.operation, intent.generation);
        self.protocol_inflight.insert(
            key,
            PendingProtocolIntent {
                spawn: matches!(
                    &intent.command,
                    RuntimeCommand::SpawnShell { .. } | RuntimeCommand::SplitPane { .. }
                ),
                spawn_cwd: intent.spawn_cwd.take(),
                input_bytes: protocol_input_bytes(&intent.command),
            },
        );
        Some(intent)
    }

    /// The final-pass host only drains this explicit nonblocking terminal prefix. Unsupported
    /// lifecycle/search/prompt requests remain at the head for the next logic pass.
    pub(crate) fn take_terminal_protocol_intent(&mut self) -> Option<WorkspaceProtocolIntent> {
        if !self
            .protocol_intents
            .front()
            .is_some_and(|intent| terminal_protocol_command(&intent.command))
        {
            return None;
        }
        self.take_protocol_intent()
    }

    #[cfg(test)]
    pub(crate) fn pr10_stage_input_for_host_test(&mut self, session: SessionId, bytes: Vec<u8>) {
        self.send(RuntimeCommand::WriteInput { session, bytes });
    }

    pub(crate) fn has_queued_protocol_intents(&self) -> bool {
        !self.protocol_intents.is_empty()
    }

    pub(crate) fn protocol_retry_delay(&self) -> Option<std::time::Duration> {
        self.protocol_retry_at
            .map(|at| at.saturating_duration_since(std::time::Instant::now()))
    }

    /// A failed try_send returns the same owned command, known never to have entered the
    /// worker. Restore its exact operation/generation at the FIFO head; no payload copy/rebuild.
    pub(crate) fn return_unsent_terminal_protocol(
        &mut self,
        operation: WorkspaceProtocolOperation,
        generation: u64,
        command: RuntimeCommand,
    ) {
        let key = (operation, generation);
        if !terminal_protocol_command(&command) || !self.protocol_inflight.contains_key(&key) {
            return;
        }
        let pending = self
            .protocol_inflight
            .remove(&key)
            .expect("exact pending checked");
        // Runtime canonicalization may compact an amortized-growth buffer on first admission.
        debug_assert!(protocol_input_bytes(&command) <= pending.input_bytes);
        self.protocol_intents.push_front(WorkspaceProtocolIntent {
            operation,
            generation,
            command,
            spawn_cwd: pending.spawn_cwd,
        });
        self.protocol_retry_at = Some(std::time::Instant::now() + PROTOCOL_RETRY_BASE);
    }

    /// Applies only the exact operation/generation currently in flight. Unknown, duplicate, or
    /// pre-wrap completions are discarded without mutating UI lifecycle state.
    pub fn complete_protocol(&mut self, completion: WorkspaceProtocolCompletion) {
        let key = (completion.operation, completion.generation);
        let Some(pending) = self.protocol_inflight.remove(&key) else {
            return;
        };
        if pending.input_bytes > 0 && completion.result.is_err() {
            self.report_protocol_queue_rejection(
                WorkspaceProtocolErrorCode::DeliveryFailed,
                "terminal_host_refusal",
            );
        }
        if let Some(search) = self.search.as_mut()
            && search.delivery == Some(key)
        {
            search.delivery = None;
            if completion.result == Err(WorkspaceProtocolErrorCode::Busy) {
                search.requested = None;
            }
        }
        let split_delivery_matches = self
            .split_drag
            .as_ref()
            .is_some_and(|transaction| transaction.pending_delivery == Some(key));
        let resize_rollback =
            self.resize_delivery_rollbacks
                .get(&key)
                .copied()
                .filter(|rollback| {
                    self.sessions
                        .get(&rollback.session)
                        .and_then(|view| view.resize_request.as_ref())
                        .is_some_and(|request| request.token == rollback.token)
                });
        if let Some(rollback) = resize_rollback.filter(|rollback| rollback.ack_retry) {
            self.resize_delivery_rollbacks.remove(&key);
            let view = self.sessions.get_mut(&rollback.session).unwrap();
            view.resize_retry_completed(
                rollback.token,
                key,
                completion.result.is_ok(),
                std::time::Instant::now(),
            );
            if view.resize_request.is_none() {
                self.failed_resize_targets
                    .insert(rollback.session, rollback.target);
                self.split_final_resize_sessions.remove(&rollback.session);
                self.split_final_resize_pending
                    .retain(|_, (id, _, _)| *id != rollback.session);
                self.resize_delivery_rollbacks
                    .retain(|_, saved| saved.session != rollback.session);
            }
            return;
        }
        let final_resize = self.split_final_resize_pending.get(&key).copied();
        if completion.result.is_err() || resize_rollback.is_none() {
            self.resize_delivery_rollbacks.remove(&key);
            self.split_final_resize_pending.remove(&key);
        }
        if let Some(rollback) = resize_rollback
            && let Some(view) = self.sessions.get_mut(&rollback.session)
        {
            if completion.result.is_ok() {
                view.resize_admitted(rollback.token, std::time::Instant::now());
            } else {
                view.cancel_resize_request();
            }
        }
        match completion.result {
            Ok(()) => {
                if pending.spawn {
                    debug_assert!(self.pending_spawn_cwds.len() < WORKSPACE_PROTOCOL_CAP);
                    self.pending_spawn_cwds
                        .push_back(PendingShellSpawn::Awaiting {
                            cwd: pending.spawn_cwd,
                        });
                }
                if split_delivery_matches && let Some(transaction) = self.split_drag.as_mut() {
                    transaction.pending_delivery = None;
                    transaction.phase = SplitDragPhase::Committed { admitted: true };
                    transaction.retry = ProtocolRetryBackoff::default();
                }
                if let Some(rollback) = resize_rollback {
                    self.failed_resize_targets.remove(&rollback.session);
                    self.resize_retry.remove(&rollback.session);
                }
                // 큐 수락에서는 실제 적용과 최종 viewport 표식을 유지한다.
            }
            Err(code) => {
                let now = std::time::Instant::now();
                let mut split_retry_exhausted = false;
                let mut resize_busy_retry_scheduled = false;
                if split_delivery_matches {
                    match code {
                        WorkspaceProtocolErrorCode::Busy => {
                            if let Some(transaction) = self.split_drag.as_mut() {
                                transaction.pending_delivery = None;
                                transaction.phase = SplitDragPhase::Committed { admitted: false };
                                split_retry_exhausted = !transaction.retry.record_busy(now);
                            }
                        }
                        _ => {
                            self.split_drag = None;
                            self.staged_split_commit_pass = None;
                        }
                    }
                }
                if split_retry_exhausted {
                    self.split_drag = None;
                    self.staged_split_commit_pass = None;
                }
                if let Some(rollback) = resize_rollback
                    && self.sent_sizes.get(&rollback.session) == Some(&rollback.target)
                {
                    if let Some(previous) = rollback.previous {
                        self.sent_sizes.insert(rollback.session, previous);
                    } else {
                        self.sent_sizes.remove(&rollback.session);
                    }
                }
                if let Some(rollback) = resize_rollback {
                    match code {
                        WorkspaceProtocolErrorCode::Busy => {
                            let retry = self.resize_retry.entry(rollback.session).or_default();
                            resize_busy_retry_scheduled = retry.record_busy(now);
                            if !resize_busy_retry_scheduled {
                                self.resize_retry.remove(&rollback.session);
                                self.failed_resize_targets
                                    .insert(rollback.session, rollback.target);
                            }
                        }
                        _ => {
                            self.resize_retry.remove(&rollback.session);
                            self.failed_resize_targets
                                .insert(rollback.session, rollback.target);
                        }
                    }
                }
                if resize_busy_retry_scheduled && let Some((session, _, _)) = final_resize {
                    self.split_final_resize_sessions.insert(session);
                }
                // 운영 코드에서 app.rs가 여기로 넘기는 값은 Busy(dotenv 승인 상한 또는
                // typed runtime backpressure)와 DeliveryFailed뿐이다. DeliveryFailed의
                // 절대다수는 실제 전송
                // 실패가 아니라 "느지막이 도착한 결과가 이미 한물간 상태"(워크스페이스
                // 전환/종료, dotenv 계속 처리가 stale로 판정됨 등 반납 경로)다. 세션이
                // 정말 죽어서 벌어진 소수 사례도 종료 배지 등 별도 신호가 이미 있어, 여기서
                // 또 배너를 띄우면 "이유 모를 배너가 가끔 뜬다"(2026-08-18 사용자 보고)는
                // 원래 버그를 그대로 재현한다. 화면은 건드리지 않고 진단용 tracing만 남긴다.
                tracing::warn!(
                    kind = "workspace",
                    phase = "protocol_completion",
                    error_code = ?code,
                    "protocol intent completed with error"
                );
            }
        }
        self.flush_pending_spawn_cd_writes();
    }

    /// App host 결과를 적용한다. operation/generation이 현재 pending과 정확히 일치하지
    /// 않으면 늦은 결과로 간주해 버린다.
    pub fn complete_io(&mut self, completion: WorkspaceIoCompletion) {
        match completion {
            WorkspaceIoCompletion::PathResolved {
                operation,
                generation,
                result,
            } => {
                let Some(pending) = self.pending_path_resolution.as_ref() else {
                    return;
                };
                if pending.operation != operation
                    || pending.generation != generation
                    || generation != self.io_generation
                {
                    return;
                }
                let pending = self
                    .pending_path_resolution
                    .take()
                    .expect("exact pending checked");
                let result = result.map(|resolved| match resolved.kind {
                    WorkspacePathKind::Directory => PathClick::Dir(resolved.path.into_path()),
                    WorkspacePathKind::OpenableFile => {
                        PathClick::OpenFile(resolved.path.into_path())
                    }
                });
                self.path_click_cache = Some((
                    pending.session,
                    pending.word,
                    result,
                    std::time::Instant::now(),
                ));
            }
            WorkspaceIoCompletion::TerminalClipboardRead {
                operation,
                generation,
                result,
            } => {
                let Some(index) = self.pending_pastes.iter().position(|pending| {
                    pending.operation == operation && pending.generation == generation
                }) else {
                    return;
                };
                let pending = self
                    .pending_pastes
                    .remove(index)
                    .expect("exact pending checked");
                // cwd 세대는 경로 조회의 수명이다. 붙여넣기는 원래 요청 세션과 토큰으로 검증한다.
                if pending.requested_at.elapsed() > PASTE_TASK_TTL {
                    self.native_error = Some(WorkspaceNativeError::ClipboardExpired);
                    return;
                }
                let payload = match result {
                    Ok(payload) => payload,
                    Err(WorkspaceIoErrorCode::ClipboardTooLarge) => {
                        self.native_error = Some(WorkspaceNativeError::ClipboardTooLarge);
                        return;
                    }
                    Err(code) => {
                        tracing::warn!(error_code = ?code, "terminal clipboard operation failed");
                        self.native_error = Some(WorkspaceNativeError::ClipboardFailed);
                        return;
                    }
                };
                let (paths, text) = payload.into_parts();
                let bytes = clipboard_terminal_paste_bytes(
                    (!paths.is_empty()).then_some(paths.as_slice()),
                    || {
                        text.map(|value| terminal_text_paste_bytes(&value, pending.bracketed))
                            .or(pending.text_fallback)
                    },
                    pending.shell_kind,
                    pending.bracketed,
                );
                if let Some(mut bytes) = bytes {
                    if let Some(deferred) = self
                        .pending_ime_submit
                        .as_mut()
                        .filter(|deferred| deferred.owner == pending.session)
                        .or_else(|| {
                            self.detached_ime_submit
                                .as_mut()
                                .filter(|deferred| deferred.owner == pending.session)
                        })
                    {
                        if pending.requested_at < deferred.started {
                            deferred.independent_before_submit.append(&mut bytes);
                        } else {
                            deferred.after_submit.append(&mut bytes);
                        }
                    } else {
                        self.send(RuntimeCommand::WriteInput {
                            session: pending.session,
                            bytes,
                        });
                    }
                }
            }
            WorkspaceIoCompletion::OpenPathFailed => {
                self.native_error = Some(WorkspaceNativeError::PathRejected);
            }
            WorkspaceIoCompletion::OpenUrlFailed => {
                self.native_error = Some(WorkspaceNativeError::UrlRejected);
            }
        }
    }

    fn request_terminal_clipboard(
        &mut self,
        session: SessionId,
        bracketed: bool,
        shell_kind: crate::ui::file_tree::ShellKind,
        text_fallback: Option<Vec<u8>>,
    ) {
        if text_fallback
            .as_ref()
            .is_some_and(|text| text.len() > TERMINAL_CLIPBOARD_TEXT_MAX_BYTES)
        {
            self.native_error = Some(WorkspaceNativeError::ClipboardTooLarge);
            return;
        }
        self.expire_pending_pastes();
        // 실행 중인 요청도 예산에 포함한다. 새 요청으로 이전 paste 컨텍스트를 덮지 않는다.
        if self.pending_pastes.len() >= WORKSPACE_IO_QUEUE_CAP {
            self.native_error = Some(WorkspaceNativeError::Busy);
            return;
        }
        let operation = self.next_io_operation();
        let generation = self.io_generation;
        if self
            .queue_io_intent(WorkspaceIoIntent::ReadTerminalClipboard {
                operation,
                generation,
            })
            .is_err()
        {
            self.native_error = Some(WorkspaceNativeError::Busy);
            return;
        }
        self.pending_pastes.push_back(PendingPaste {
            operation,
            generation,
            session,
            bracketed,
            shell_kind,
            text_fallback,
            requested_at: std::time::Instant::now(),
        });
    }

    fn request_open_path(&mut self, path: PathBuf) {
        let intent = WorkspacePathPayload::try_new(path).map(WorkspaceIoIntent::OpenPath);
        if let Err(code) = intent.and_then(|intent| self.queue_io_intent(intent)) {
            self.native_error = Some(match code {
                WorkspaceIoErrorCode::Busy => WorkspaceNativeError::Busy,
                _ => WorkspaceNativeError::PathRejected,
            });
        }
    }

    fn request_open_url(&mut self, url: &str) {
        let intent = WorkspaceUrlPayload::try_new(url.to_owned()).map(WorkspaceIoIntent::OpenUrl);
        if let Err(code) = intent.and_then(|intent| self.queue_io_intent(intent)) {
            self.native_error = Some(match code {
                WorkspaceIoErrorCode::Busy => WorkspaceNativeError::Busy,
                _ => WorkspaceNativeError::UrlRejected,
            });
        }
    }

    /// Cmd+F 등으로 focused 터미널에서 검색 바를 연다 (T3). 이미 같은 세션에 열려 있으면
    /// 입력창 포커스만 다시 준다. focused pane에 세션이 없으면 무시한다.
    pub fn open_search(&mut self) {
        let Some(session) = self.focused_session() else {
            return;
        };
        self.open_search_for_session(session);
    }

    fn open_search_for_session(&mut self, session: SessionId) {
        match &mut self.search {
            Some(search) if search.session == session => {
                search.focus_input = true;
            }
            _ => {
                self.search = Some(TerminalSearch {
                    session,
                    query: String::new(),
                    requested: None,
                    delivery: None,
                    matches: Vec::new(),
                    total_lines: 0,
                    capped: false,
                    current: 0,
                    focus_input: true,
                    input_id: None,
                    scroll_to_current: false,
                });
            }
        }
    }

    /// 검색 바를 닫고 원래 터미널로 포커스를 되돌린다 (T3).
    fn close_search(&mut self) {
        let Some(search) = self.search.take() else {
            return;
        };
        // 실제 refocus는 다음 프레임 render_pane에서 pending_focus로 소비된다.
        if let Some(pane) = self.mux.as_ref().and_then(|mux| {
            mux.tabs
                .iter()
                .flat_map(|tab| &tab.panes)
                .find(|pane| pane.session_id == Some(search.session))
                .map(|pane| pane.id.clone())
        }) {
            self.request_pane_focus(pane);
        }
    }

    /// 다른 분할 pane이 PTY 이벤트를 읽기 전에 검색창 소유의 키를 소비한다.
    /// Esc는 egui의 프레임 시작 때 포커스가 해제되므로 직전 소유자도 확인한다.
    pub fn handle_terminal_search_keys(&mut self, ctx: &egui::Context) -> bool {
        let Some(input_id) = self.search.as_ref().and_then(|search| search.input_id) else {
            return false;
        };
        if super::popup::background_input_blocked(ctx)
            || !ctx.memory(|memory| {
                memory.has_focus(input_id)
                    || (memory.focused().is_none() && memory.had_focus_last_frame(input_id))
            })
        {
            return false;
        }
        let (next, previous, close) = ctx.input_mut(|input| {
            let mut next = false;
            let mut previous = false;
            let mut close = false;
            input.events.retain(|event| {
                match terminal_search_key(event) {
                    Some(TerminalSearchKey::Next) => next = true,
                    Some(TerminalSearchKey::Previous) => previous = true,
                    Some(TerminalSearchKey::Close) => close = true,
                    None => return true,
                }
                false
            });
            if next || previous || close {
                // PTY 매퍼는 egui의 소비 목록이 아닌 raw 사본을 읽는다.
                input
                    .raw
                    .events
                    .retain(|event| terminal_search_key(event).is_none());
            }
            (next, previous, close)
        });
        if close {
            self.close_search();
        } else if (next || previous)
            && let Some(search) = self.search.as_mut()
        {
            let count = search.matches.len();
            if count > 0 {
                search.current = if previous {
                    (search.current + count - 1) % count
                } else {
                    (search.current + 1) % count
                };
                search.scroll_to_current = true;
            }
        }
        next || previous || close
    }

    /// 터미널 검색 UI (T3): 뷰포트에 보이는 매치 하이라이트 + 우상단 검색 바 + 이동 스크롤.
    /// 검색은 backend(worker)가 수행하고, UI는 결과(라인 오프셋)를 받아 그리고 이동만 한다.
    #[allow(clippy::too_many_arguments)]
    fn render_terminal_search(
        &mut self,
        ui: &mut egui::Ui,
        session: SessionId,
        term_rect: egui::Rect,
        origin: egui::Pos2,
        cell_size: egui::Vec2,
        snapshot: &TerminalViewportSnapshot,
        catalog: &i18n::Catalog,
    ) -> bool {
        // 이 pane의 세션에 대한 검색만 그린다.
        if self.search.as_ref().map(|s| s.session) != Some(session) {
            return false;
        }

        // 1) 뷰포트에 보이는 매치 하이라이트. 뷰포트 행 = scroll_offset + rows-1 - line_from_bottom
        //    (total_lines와 무관 — 현재 스냅샷 스크롤 위치만으로 매핑된다).
        if let Some(search) = self.search.as_ref() {
            let rows = snapshot.rows as i32;
            let normal = egui::Color32::from_rgba_unmultiplied(0xE5, 0xC0, 0x7B, 70);
            let current = egui::Color32::from_rgba_unmultiplied(0xF2, 0x8C, 0x28, 140);
            for (i, m) in search.matches.iter().enumerate() {
                let vrow = snapshot.scroll_offset + rows - 1 - m.line_from_bottom as i32;
                if vrow < 0 || vrow >= rows {
                    continue;
                }
                let x0 = origin.x + m.col_start as f32 * cell_size.x;
                let x1 = origin.x + m.col_end as f32 * cell_size.x;
                let y0 = origin.y + vrow as f32 * cell_size.y;
                let rect = egui::Rect::from_min_size(
                    egui::pos2(x0, y0),
                    egui::vec2((x1 - x0).max(cell_size.x), cell_size.y),
                );
                let color = if i == search.current { current } else { normal };
                ui.painter().rect_filled(rect, 0.0, color);
            }
        }

        // 2) current 매치가 화면에 보이도록 스크롤(Scroll delta 양수 = 과거로 = display_offset↑).
        let scroll_delta = {
            let Some(search) = self.search.as_mut() else {
                return false;
            };
            if search.scroll_to_current {
                search.scroll_to_current = false;
                search.matches.get(search.current).and_then(|m| {
                    let rows = snapshot.rows as i32;
                    let total = search.total_lines as i32;
                    let b = m.line_from_bottom as i32;
                    // 매치를 화면 중앙 근처에 두되, 유효 범위 [0, history]로 클램프.
                    let desired = (b - rows / 2).clamp(0, (total - rows).max(0));
                    let delta = desired - snapshot.scroll_offset;
                    (delta != 0).then_some(delta)
                })
            } else {
                None
            }
        };
        if let Some(delta) = scroll_delta {
            if self.send_keep_selection(RuntimeCommand::Scroll { session, delta }) {
                self.clear_selection(session);
            } else if let Some(search) = self.search.as_mut() {
                search.scroll_to_current = true;
            }
        }

        // 3) 우상단 검색 바.
        let (mut query, focus_input, match_count, current, capped) = {
            let Some(search) = self.search.as_ref() else {
                return false;
            };
            (
                search.query.clone(),
                search.focus_input,
                search.matches.len(),
                search.current,
                search.capped,
            )
        };
        let mut do_next = false;
        let mut do_prev = false;
        let mut do_close = false;
        let input_id = ui.make_persistent_id(("terminal_search_input", session));
        if let Some(search) = self.search.as_mut() {
            search.input_id = Some(input_id);
        }

        let bar_width = 280.0;
        let pos = egui::pos2(
            (term_rect.right() - bar_width - 8.0).max(term_rect.left() + 4.0),
            term_rect.top() + 8.0,
        );
        let bar = egui::Area::new(egui::Id::new(("terminal_search", session)))
            .order(egui::Order::Foreground)
            .fixed_pos(pos)
            .constrain_to(term_rect)
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_width(bar_width);
                    ui.horizontal(|ui| {
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut query)
                                .id(input_id)
                                .return_key(None)
                                .desired_width(120.0)
                                .hint_text(catalog.t("workspace.search.hint", &[])),
                        );
                        if focus_input {
                            resp.request_focus();
                        }
                        // 매치 카운트 n/m (쿼리 없으면 공백, 매치 없으면 "없음").
                        let label = if query.trim().is_empty() {
                            String::new()
                        } else if match_count == 0 {
                            catalog.t("workspace.search.no_match", &[])
                        } else {
                            format!("{}/{}", current + 1, match_count)
                        };
                        ui.label(label);
                        if capped {
                            ui.label(catalog.t("workspace.search.capped", &[]));
                        }
                        if ui
                            .button("‹")
                            .on_hover_text(catalog.t("workspace.search.prev", &[]))
                            .clicked()
                        {
                            do_prev = true;
                        }
                        if ui
                            .button("›")
                            .on_hover_text(catalog.t("workspace.search.next", &[]))
                            .clicked()
                        {
                            do_next = true;
                        }
                        if ui
                            .button("×")
                            .on_hover_text(catalog.t("workspace.search.close", &[]))
                            .clicked()
                        {
                            do_close = true;
                        }
                    });
                });
            });
        let interacted = bar.response.contains_pointer()
            && ui.input(|input| input.pointer.any_pressed() || input.pointer.any_released());
        if !do_close && (do_next || do_prev) {
            ui.memory_mut(|memory| memory.request_focus(input_id));
        }

        // 4) 검색 바 조작 반영.
        {
            let Some(search) = self.search.as_mut() else {
                return false;
            };
            search.focus_input = false;
            if query != search.query {
                search.query = query;
                // 결과를 버렸으면 같은 문자열로 돌아와도 새 결과를 요청해야 한다.
                search.requested = None;
                search.delivery = None;
                search.matches.clear();
                search.total_lines = 0;
                search.current = 0;
                search.capped = false;
                search.scroll_to_current = false;
            }
            let m = search.matches.len();
            if !do_close && m > 0 {
                if do_next {
                    search.current = (search.current + 1) % m;
                    search.scroll_to_current = true;
                }
                if do_prev {
                    search.current = (search.current + m - 1) % m;
                    search.scroll_to_current = true;
                }
            }
        }
        if do_close {
            self.close_search();
            return true;
        }

        // 5) 쿼리가 바뀌었을 때만 재검색을 요청한다(매 프레임 금지).
        self.submit_terminal_search();
        interacted
    }

    fn submit_terminal_search(&mut self) {
        let pending = self.search.as_ref().and_then(|s| {
            (s.requested.as_deref() != Some(s.query.as_str())).then(|| (s.session, s.query.clone()))
        });
        if let Some((sess, q)) = pending {
            let empty = q.trim().is_empty();
            let delivery = if empty {
                None
            } else {
                match self.queue_protocol_intent_tracked(RuntimeCommand::SearchScrollback {
                    session: sess,
                    query: q.clone(),
                    max_matches: SEARCH_MAX_MATCHES,
                }) {
                    Ok(key) => Some(key),
                    Err(code) => {
                        self.report_protocol_queue_rejection(code, "terminal_search");
                        // 크기 초과 등 영구 거절은 입력을 바꿀 때까지 다시 보내지 않는다.
                        if code != WorkspaceProtocolErrorCode::Busy
                            && let Some(search) = self.search.as_mut()
                        {
                            search.requested = Some(q);
                            search.delivery = None;
                        }
                        return;
                    }
                }
            };
            if let Some(s) = self.search.as_mut() {
                // 큐에 들어간 쿼리만 기록한다. Busy이면 다음 completion 프레임에 재시도한다.
                s.requested = Some(q);
                s.delivery = delivery;
                if empty {
                    s.matches.clear();
                    s.total_lines = 0;
                    s.capped = false;
                    s.current = 0;
                }
            }
        }
    }

    /// 활성 workspace의 프로젝트명을 세팅한다(App이 매 프레임). 세션 기본 제목("셀 N")을
    /// 이 이름으로 표시한다.
    pub fn set_project_name(&mut self, name: Option<String>) {
        self.project_name = name;
    }

    /// UI 텍스트 배율을 세팅한다(App이 매 프레임). 터미널 font_size 역보정에 쓴다.
    /// 활성 워크스페이스 고유색. 사이드바 아바타·세션 레일과 같은 값이라,
    /// 포커스된 pane 상단선이 그 워크스페이스에 속한다는 걸 같은 색으로 잇는다.
    pub fn set_workspace_accent(&mut self, color: egui::Color32) {
        self.workspace_accent = color;
    }

    /// 포커스된 로컬 pane 헤더 옆에 붙일 보조 탭 목록. 비어 있으면 헤더는 예전 그대로다.
    /// 개수 상한은 여기서 자르지 않는다 — App이 문서 탭 개수를 이미 유계로 관리하고,
    /// 좁은 헤더에서의 축약은 `layout_aux_tabs`가 활성 탭을 보존하며 처리한다.
    pub fn set_aux_tabs(&mut self, tabs: Vec<PaneAuxTab>) {
        self.aux_tabs = tabs;
    }

    pub fn set_ui_scale(&mut self, scale: f32) {
        self.ui_scale = if scale.is_finite() && scale > 0.1 {
            scale
        } else {
            1.0
        };
    }

    /// 세션 → 셸 pid를 세팅한다(App이 매 프레임, ResourceUsage 스냅샷 기준).
    /// 터미널 경로 더블클릭의 상대경로 해석(lsof cwd 1회 조회)에 쓴다.
    pub fn set_session_pids(&mut self, pids: &[(SessionId, u32)]) {
        let next: HashMap<SessionId, u32> = pids.iter().copied().collect();
        if self.session_pids != next {
            self.session_pids = next;
            self.invalidate_path_resolution();
        }
    }

    /// (세션, 단어) 캐시를 거친 경로 해석. hover가 매 프레임 부르므로 같은 단어는
    /// 재해석하지 않고, 셸 cwd(lsof)는 세션별 백그라운드 캐시로 조회한다 — hover
    /// 경로는 UI 스레드에서 lsof를 직접 돌리지 않는다(프레임 스톨 방지, 2026-07-16).
    fn resolve_path_cached(&mut self, session: SessionId, word: &str) -> Option<PathClick> {
        if let Some((s, w, res, at)) = &self.path_click_cache
            && *s == session
            && w == word
            && at.elapsed() < PATH_CACHE_TTL
        {
            return res.clone();
        }
        if self
            .pending_path_resolution
            .as_ref()
            .is_some_and(|pending| {
                pending.session == session
                    && pending.word == word
                    && pending.generation == self.io_generation
            })
        {
            return None;
        }
        let Ok(word) = bounded_path_word(word) else {
            return None;
        };
        let cwd = self
            .session_cwds
            .get(&session)
            .map(PathBuf::from)
            .map(WorkspacePathPayload::try_new)
            .transpose()
            .ok()
            .flatten();
        let operation = self.next_io_operation();
        let pending = PendingPathResolution {
            operation,
            generation: self.io_generation,
            session,
            word: word.clone(),
        };
        let intent = WorkspaceIoIntent::ResolvePath {
            operation,
            generation: self.io_generation,
            session,
            pid: self.session_pids.get(&session).copied(),
            cwd,
            word,
        };
        if self.queue_io_intent(intent).is_ok() {
            self.pending_path_resolution = Some(pending);
        }
        None
    }

    fn invalidate_path_resolution(&mut self) {
        self.io_generation = self.io_generation.wrapping_add(1).max(1);
        self.path_click_cache = None;
        self.pending_path_resolution = None;
        self.io_intents
            .retain(|intent| !matches!(intent, WorkspaceIoIntent::ResolvePath { .. }));
    }

    /// 세션별 현재 작업 폴더를 세팅한다(App이 매 프레임, 감지 워커 lsof 결과).
    /// `style`은 위치 표시명 스타일(설정 — 현재 폴더명 vs 저장소명)을 함께 나른다.
    pub fn set_session_cwds(
        &mut self,
        cwds: std::collections::HashMap<SessionId, String>,
        _style: crate::config::SessionNameStyle,
    ) {
        if self.session_cwds != cwds {
            self.session_cwds = cwds;
            self.session_project_names = self
                .session_project_names
                .retain_matching_cwds(&self.session_cwds);
            self.invalidate_path_resolution();
        }
    }

    /// Installs an App-computed immutable project-name projection. Repeated calls with the same
    /// revision are O(1) and retain the existing Arc. The App must update cwd state first; entries
    /// for missing or already-moved sessions are discarded at this boundary.
    pub fn set_session_project_names(&mut self, snapshot: SessionProjectNameSnapshot) {
        if snapshot.revision <= self.session_project_names.revision {
            return;
        }
        self.session_project_names = snapshot.retain_matching_cwds(&self.session_cwds);
    }

    /// 「에이전트로 보내기」 프리셋을 세팅한다(App이 설정에서 매 프레임 미러).
    pub fn set_agent_send_presets(&mut self, presets: Vec<String>) {
        self.agent_send_presets = presets;
    }

    /// 세션별 에이전트 표시정보를 세팅한다(App이 병합한 최종본 — 3줄 행 렌더용).
    pub fn set_agent_info(
        &mut self,
        info: std::collections::HashMap<SessionId, crate::agent_detect::AgentDisplay>,
    ) {
        self.agent_info = info;
    }

    pub(crate) fn set_agent_executions(
        &mut self,
        executions: std::collections::HashMap<
            SessionId,
            crate::agent_detect::AgentExecutionIdentity,
        >,
    ) {
        self.agent_executions = executions;
    }

    pub(crate) fn take_selected_agent_prompt(&mut self) -> Option<SelectedAgentPrompt> {
        self.selected_agent_prompts.pop_front()
    }

    pub(crate) fn set_archived_resume_presentation(
        &mut self,
        presentations: std::collections::HashMap<
            SessionId,
            crate::agent_resume::ArchivedResumePresentation,
        >,
    ) {
        self.archived_resume_presentation = presentations;
    }

    /// 세션의 에이전트 요약 줄("Codex · gpt-5.6-sol · max")을 돌려준다 — 없으면 셸/미감지.
    /// 워크스페이스가 대기(warm)로 내려가도 이 맵은 마지막 감지값을 유지하므로(전환 시
    /// 안 지움), 활동 패널·PWA가 비활성 워크스페이스의 에이전트 정보를 보여줄 수 있다
    /// (2026-07-13 방안①). 절전되면 workspace_ui째로 사라져 자연히 표시 안 된다.
    pub fn agent_line_for(&self, session: SessionId) -> Option<String> {
        self.agent_info.get(&session).map(agent_info_line)
    }

    pub(crate) fn agent_display_for(
        &self,
        session: SessionId,
    ) -> Option<&crate::agent_detect::AgentDisplay> {
        self.agent_info.get(&session)
    }

    /// Fleet 카드용 한 줄 작업 설명. `agent_info`는 warm 전환 뒤에도 마지막
    /// transcript 설명을 보존하므로 완료/종료 카드에도 새 파일 I/O 없이 쓸 수 있다.
    pub fn agent_task_line_for(
        &self,
        session: SessionId,
        state: crate::agent_surface::AgentVisualState,
    ) -> Option<String> {
        let display = self.agent_info.get(&session)?;
        crate::fleet::task_preview(
            state,
            display.user_instruction.as_deref(),
            display.last_agent_summary.as_deref(),
        )
    }

    pub fn last_input_submission(&self, session: SessionId) -> Option<i64> {
        self.sessions
            .get(&session)
            .and_then(|view| view.last_submission_at_micros)
    }

    /// 비활성(warm) 워크스페이스의 접힌 행 상태 집계용 마지막 감지값.
    /// 활성 워크스페이스는 `session_entries`가 hook/transcript까지 병합한 값을 사용한다.
    pub fn last_session_status(&self, session: SessionId) -> Option<SessionStatus> {
        self.sessions.get(&session).and_then(|view| view.status)
    }

    /// 세션에서 감지된 에이전트 종류 — 셸이거나 미감지면 None.
    /// warm으로 내려가도 agent_info는 마지막 감지값을 유지한다(위 agent_line_for 주석).
    pub fn agent_provider_for(
        &self,
        session: SessionId,
    ) -> Option<crate::agent_surface::AgentProvider> {
        self.agent_info.get(&session).map(|d| d.kind.into())
    }

    pub fn agent_providers(
        &self,
    ) -> std::collections::HashMap<SessionId, crate::agent_surface::AgentProvider> {
        self.agent_info
            .iter()
            .map(|(session, display)| (*session, display.kind.into()))
            .collect()
    }

    /// cwd에서 뽑은 프로젝트명이 이 워크스페이스 자신의 이름과 다르면 소속을 함께
    /// 밝힌다 — 규칙 본문·근거는 모듈 자유 함수 `qualify_project_name` 주석 참고.
    /// App도 warm/유휴 워크스페이스 표시(`App::activity_session_name`, app.rs)에 같은
    /// 자유 함수를 쓴다 — 규칙이 두 곳에 따로 구현되면 같은 화면 안에서 표기가 갈릴 수
    /// 있다(2026-08-19 코드 리뷰).
    fn qualify_cwd_project_name(&self, project_name: &str) -> String {
        qualify_project_name(project_name, self.project_name.as_deref())
    }

    /// 세션 표시 제목. 우선순위: ① 사용자 rename(기본 제목이 아니면) → 그대로,
    /// ② OSC 0/2 동적 제목(프로그램이 설정, 예: cwd/명령) → 그 제목, ③ 프로젝트 폴더명(≈깃
    /// 레포명), ④ 원 표기. osc는 이 세션 터미널의 현재 OSC 제목.
    /// 세션 1행 제목 우선순위(2026-07-08 사용자): 수동 rename > 현재 작업 폴더명(git
    /// 프로젝트명) > OSC 타이틀 > "~". 폴더명이 기본이라 어느 폴더에서 작업 중인지 보인다.
    fn resolve_session_title(
        &self,
        raw: &str,
        session: Option<SessionId>,
        osc: Option<&str>,
        catalog: &i18n::Catalog,
    ) -> String {
        if !is_default_session_title(raw) {
            return display_pane_title(raw, catalog); // 사용자 rename — 그대로 고정
        }
        // 프로젝트명은 App host가 cwd별로 미리 계산한 bounded immutable projection이다.
        if let Some(n) = session
            .and_then(|session| {
                self.session_cwds.get(&session).and_then(|cwd| {
                    self.session_project_names
                        .project_name(session, cwd.as_str())
                })
            })
            .filter(|t| !t.trim().is_empty())
        {
            return self.qualify_cwd_project_name(n);
        }
        // cwd 미탐지(pid 없음/lsof 지연) 폴백: OSC 타이틀 > 프로젝트명 > 기본.
        if let Some(t) = osc.map(str::trim).filter(|t| !t.is_empty()) {
            return t.to_owned();
        }
        if let Some(project) = self.project_name.as_deref() {
            return project.to_owned();
        }
        display_pane_title(raw, catalog)
    }

    /// 작업 설명이 아직 없는 에이전트 행에 표시할 안정적인 프로젝트 컨텍스트.
    /// App이 미리 계산한 프로젝트명을 우선하고, 없으면 현재 cwd의 폴더명을 쓴다.
    /// resolve_session_title과 같은 이유로 워크스페이스 자체 이름과 다르면 함께 밝힌다
    /// (qualify_cwd_project_name 주석 참고). 사용자 이름이 있는 행에서는 이 컨텍스트가
    /// 둘째 줄의 작업 설명 폴백으로 표시된다.
    fn session_project_context(&self, session: Option<SessionId>) -> Option<String> {
        session.and_then(|session| {
            let cwd = self.session_cwds.get(&session)?;
            let name = self
                .session_project_names
                .project_name(session, cwd)
                .filter(|name| !name.trim().is_empty())
                .or_else(|| {
                    Path::new(cwd)
                        .file_name()
                        .and_then(|name| name.to_str())
                        .filter(|name| !name.trim().is_empty())
                })?;
            Some(self.qualify_cwd_project_name(name))
        })
    }

    /// 세션의 현재 OSC 제목(있으면).
    fn session_osc_title(&self, session: Option<SessionId>) -> Option<String> {
        session
            .and_then(|s| self.sessions.get(&s))
            .and_then(|v| v.snapshot.as_ref())
            .and_then(|s| s.title.clone())
    }

    /// 모든 세션의 터미널 렌더 캐시를 비운다 — 테마 변경 시 stale galley(옛 폰트
    /// 아틀라스/색)가 재사용돼 글자가 깨지던 문제 해결(#7). 다음 프레임에 전 행 재구성.
    /// 이 세션에 걸린 선택을 해제한다 — 선택 중엔 화면이 freeze되므로, WorkspaceUi::send를
    /// 우회해 직접 WriteInput을 보내는 경로(app.rs의 트리 드롭·resume 주입)에서 화면이 멈춘
    /// 듯 보이지 않게 호출한다(codex).
    pub fn clear_selection(&mut self, session: SessionId) {
        if self.selection.is_some_and(|(s, _, _)| s == session) {
            self.selection = None;
        }
    }

    pub fn clear_render_caches(&mut self) {
        for view in self.sessions.values_mut() {
            view.render_cache.clear();
        }
    }

    #[allow(dead_code)]
    fn handle_events(&mut self, events: &[RuntimeEvent], catalog: &i18n::Catalog) {
        self.handle_events_with_attached_targets(events, catalog, &[]);
    }

    fn handle_events_with_attached_targets(
        &mut self,
        events: &[RuntimeEvent],
        catalog: &i18n::Catalog,
        attached_targets: &[AttachedPaneTarget],
    ) {
        for event in events {
            match event {
                RuntimeEvent::MuxUpdated { snapshot } => {
                    let split_acknowledged = self.split_drag.as_ref().is_some_and(|transaction| {
                        matches!(
                            transaction.phase,
                            SplitDragPhase::Committed { admitted: true }
                        ) && mux_split_ratio(snapshot, &transaction.tab, &transaction.path)
                            .is_some_and(|ratio| (ratio - transaction.ratio).abs() <= 0.0001)
                    });
                    let split_tab_disappeared =
                        self.split_drag.as_ref().is_some_and(|transaction| {
                            !snapshot.tabs.iter().any(|tab| tab.id == transaction.tab)
                        });
                    let split_path_disappeared =
                        self.split_drag.as_ref().is_some_and(|transaction| {
                            snapshot.tabs.iter().any(|tab| tab.id == transaction.tab)
                                && mux_split_ratio(snapshot, &transaction.tab, &transaction.path)
                                    .is_none()
                        });
                    // 사라진 세션의 캐시 정리
                    let alive = mux_sessions(snapshot);
                    self.sessions.retain(|id, _| alive.contains(id));
                    self.sent_sizes.retain(|id, _| alive.contains(id));
                    self.pending_resize_target
                        .retain(|id, _| alive.contains(id));
                    self.failed_resize_targets
                        .retain(|id, _| alive.contains(id));
                    self.resize_retry.retain(|id, _| alive.contains(id));
                    self.split_final_resize_sessions
                        .retain(|id| alive.contains(id));
                    self.resize_delivery_rollbacks
                        .retain(|_, rollback| alive.contains(&rollback.session));
                    self.split_final_resize_pending
                        .retain(|_, (id, _, _)| alive.contains(id));
                    self.session_project_names =
                        self.session_project_names.retain_live_sessions(&alive);
                    self.last_output_copy_pending
                        .retain(|id| alive.contains(id));
                    // 검색 중인 세션이 사라지면 검색 바를 닫는다.
                    if self
                        .search
                        .as_ref()
                        .is_some_and(|s| !alive.contains(&s.session))
                    {
                        self.search = None;
                    }
                    // hidden(active tab 밖) 세션의 마지막 스냅샷은 버린다 —
                    // §14.4 hidden render cache drop. tab 복귀 시 worker가
                    // 전환 즉시 push하므로(emit_mux_and_watched) 공백은 짧다 (codex 리뷰)
                    let mut visible = visible_mux_sessions(snapshot);
                    for target in attached_targets {
                        if find_attached_pane(snapshot, target).is_some() {
                            visible.insert(target.session);
                        }
                    }
                    self.split_final_resize_sessions
                        .retain(|session| visible.contains(session));
                    // 선택된 세션이 hidden 되면 선택을 해제한다 — 안 그러면 freeze(선택 중
                    // snapshot 갱신 스킵)가 재표시돼도 안 풀려 stuck 된다(codex).
                    if self
                        .selection
                        .is_some_and(|(s, _, _)| !visible.contains(&s))
                    {
                        self.selection = None;
                    }
                    for (id, view) in self.sessions.iter_mut() {
                        if !visible.contains(id) {
                            view.snapshot = None;
                            view.snapshot_gen = view.snapshot_gen.wrapping_add(1);
                            view.pending_snapshot = None;
                            view.initial_presentation = None;
                            view.resize_presentation = None;
                            view.resize_request = None;
                            view.resize_desired = None;
                            self.sent_sizes.remove(id);
                            view.render_cache.clear();
                        }
                    }
                    if split_acknowledged {
                        self.split_final_resize_sessions = visible_mux_sessions(snapshot);
                        self.pending_resize_target.retain(|session, _| {
                            !self.split_final_resize_sessions.contains(session)
                        });
                        self.staged_terminal_resizes.clear();
                        self.split_drag = None;
                        self.staged_split_commit_pass = None;
                    } else if split_tab_disappeared || split_path_disappeared {
                        self.split_drag = None;
                        self.staged_split_commit_pass = None;
                    }
                    self.mux = Some(Arc::clone(snapshot));
                }
                RuntimeEvent::Viewport {
                    session,
                    snapshot,
                    bracketed_paste,
                }
                | RuntimeEvent::ViewportTracked {
                    session,
                    snapshot,
                    bracketed_paste,
                    ..
                } => {
                    // hidden 전환 뒤 도착한 stale Viewport가 캐시를 되살리지 않도록
                    // 현재 active tab의 visible 세션만 snapshot을 저장한다.
                    let attached_visible = attached_targets.iter().any(|target| {
                        target.session == *session
                            && self
                                .mux
                                .as_deref()
                                .and_then(|mux| find_attached_pane(mux, target))
                                .is_some()
                    });
                    // 출력 시각은 **가시성과 무관하게** 기록한다. 멈춤 감지는 "자리를
                    // 비운 사이 멈춘 세션"을 찾는 게 목적이라 안 보이는 세션이야말로
                    // 대상이다 — 게이트 안에 두면 지금 보고 있는 pane만 판정됐다
                    // (2026-08-08 리뷰).
                    if self.session_alive(*session) {
                        self.sessions.entry(*session).or_default().last_output_at =
                            Some(deppy_core::time::unix_secs_i64());
                    }
                    if self.session_visible(*session) || attached_visible {
                        // 이 세션에 선택이 걸려 있으면 화면(snapshot)을 얼린다 — claude/codex
                        // 작업 중엔 화면이 매 프레임 갱신돼 예전엔 선택이 즉시 무효화됐다(#3).
                        // 좌표 기준 선택이라 그냥 유지만 하면 갱신된 화면의 '다른 텍스트'를
                        // 복사할 수 있어(codex), 선택 중엔 뷰를 정지시켜 선택·복사·표시가 항상
                        // 일치하게 한다(표준 터미널 동작). 클릭으로 선택 해제하면 최신으로 갱신.
                        let frozen = self.selection.is_some_and(|(s, _, _)| s == *session);
                        let view = self.sessions.entry(*session).or_default();
                        if !view.accepts_resize_viewport(
                            event.viewport().and_then(|(_, _, _, stamp)| stamp),
                            (snapshot.cols, snapshot.rows),
                            std::time::Instant::now(),
                        ) {
                            continue;
                        }
                        view.bracketed_paste = *bracketed_paste;
                        let now = std::time::Instant::now();
                        if view.buffer_initial_snapshot(Arc::clone(snapshot), now) {
                            // split seed는 first nonblank 또는 original deadline까지 보류.
                        } else if view.buffer_resize_snapshot(Arc::clone(snapshot), now) {
                            // resize target이 quiet window를 통과할 때까지 last stable 화면 유지.
                        } else if frozen {
                            // 선택 중엔 표시 snapshot을 얼리되, 최신본은 pending에 보관해
                            // 해제 시 catch-up한다(codex — 안 그러면 화면이 선택 당시에 멈춤).
                            view.pending_snapshot = Some(Arc::clone(snapshot));
                        } else {
                            view.install_snapshot(Arc::clone(snapshot));
                        }
                        if let Some(stamp) = event.viewport().and_then(|(_, _, _, stamp)| stamp) {
                            self.finish_resize_viewport(*session, stamp);
                        }
                    }
                }
                // SessionExited(런타임 종료) / SessionRestored(재시작 시 아카이브 복원,
                // PR-A2)는 exit_code + 결과 상태 배지 부기는 동일하지만, PR-3부터는
                // restored_readonly만 갈라 하단 배너/재실행 버튼 문구를 구분한다.
                // 완료 알림 차이(복원은 재발화 안 함)는 process_ws_notifications 몫.
                RuntimeEvent::SessionExited { session, exit_code } => {
                    self.apply_session_exit(*session, *exit_code, false);
                }
                RuntimeEvent::SessionRestored { session, exit_code } => {
                    self.apply_session_exit(*session, *exit_code, true);
                }
                RuntimeEvent::SpawnFailed { kind, message } => {
                    if *kind == SpawnKind::Shell {
                        self.resolve_pending_shell_spawn(None);
                    }
                    self.error_is_pressure = false;
                    self.error = Some(crate::ui::render_message(catalog, message));
                }
                RuntimeEvent::SessionInputSubmitted { session, at_micros } => {
                    if *at_micros > 0 && self.session_alive(*session) {
                        let view = self.sessions.entry(*session).or_default();
                        view.last_submission_at_micros = Some(
                            view.last_submission_at_micros
                                .map_or(*at_micros, |previous| previous.max(*at_micros)),
                        );
                    }
                }
                RuntimeEvent::SessionStatusChanged { session, status } => {
                    if self.session_alive(*session) {
                        self.note_status_flash(*session, *status);
                        self.sessions.entry(*session).or_default().status = Some(*status);
                    }
                }
                RuntimeEvent::SessionStatusViewChanged { session, view } => {
                    if self.session_alive(*session) {
                        self.note_status_flash(*session, view.status);
                        let entry = self.sessions.entry(*session).or_default();
                        entry.status = Some(view.status);
                        entry.status_view = Some(view.clone());
                    }
                }
                RuntimeEvent::PtyInputPressure { session, pressure } => {
                    // queued=0은 해소 신호(2026-07-09) — 경고 대신 상태를 걷어낸다.
                    if pressure.queued_messages == 0 && pressure.queued_bytes == 0 {
                        if let Some(entry) = self.sessions.get_mut(session) {
                            entry.input_pressure = None;
                        }
                        // 압력 경고일 때만 알림 대기를 걷는다 — spawn 실패 등 무관 오류 보존.
                        if self.error_is_pressure {
                            self.error = None;
                            self.error_is_pressure = false;
                        }
                        continue;
                    }
                    if self.session_alive(*session) {
                        self.sessions.entry(*session).or_default().input_pressure =
                            Some(pressure.clone());
                    }
                    // 같은 압박 에피소드의 수치 갱신은 세션 상태에만 반영한다. 알림 문자열을
                    // 매 이벤트마다 다시 만들면 값이 달라질 때마다 새 알림이 쌓인다.
                    if !self.error_is_pressure && self.error.is_none() {
                        self.error_is_pressure = true;
                        self.error = Some(catalog.t(
                            "workspace.input_pressure",
                            &[
                                ("queued", &format_bytes(pressure.queued_bytes as u64)),
                                ("max", &format_bytes(pressure.max_bytes as u64)),
                            ],
                        ));
                    }
                }
                RuntimeEvent::ShellSpawned { session } => {
                    if self
                        .mux
                        .as_deref()
                        .is_some_and(|mux| visible_split_contains_session(mux, *session))
                    {
                        self.sessions
                            .entry(*session)
                            .or_default()
                            .arm_initial_presentation(std::time::Instant::now());
                    }
                    self.resolve_pending_shell_spawn(Some(*session));
                }
                // Launch correlation is app-owned lifecycle state (approval listener/runtime host),
                // not terminal rendering state. The app consumes this event before forwarding the
                // same batch here, so the workspace leaf intentionally performs no action.
                RuntimeEvent::AgentSpawned { .. } | RuntimeEvent::AgentSpawnResolved { .. } => {}
                RuntimeEvent::ResourceUsage { .. }
                | RuntimeEvent::ScrollbackLimitApplied { .. } => {}
                RuntimeEvent::ResizeApplied { session, stamp } => {
                    if let Some(view) = self.sessions.get_mut(session) {
                        view.observe_resize_applied(*stamp, std::time::Instant::now());
                    }
                }
                RuntimeEvent::ResizeFailed {
                    session,
                    token,
                    reason,
                } => {
                    if let Some(view) = self.sessions.get_mut(session) {
                        let target = view
                            .resize_request
                            .as_ref()
                            .filter(|request| request.token == *token)
                            .map(|request| request.target);
                        view.resize_failed(*token, *reason);
                        if view.resize_request.is_none()
                            && let Some(target) = target
                        {
                            self.failed_resize_targets.insert(*session, target);
                            self.resize_delivery_rollbacks
                                .retain(|_, rollback| rollback.session != *session);
                            self.split_final_resize_pending
                                .retain(|_, (id, _, _)| id != session);
                            self.split_final_resize_sessions.remove(session);
                        }
                    }
                }
                // 유지보수 결과의 count와 후속 UI는 App controller가 소유한다. 터미널
                // leaf는 세션/mux/오류 상태를 바꾸지 않고 이벤트를 소비만 한다.
                RuntimeEvent::UnattachedSessionsInspected { .. }
                | RuntimeEvent::UnattachedSessionsKilled { .. } => {}
                RuntimeEvent::DurableEventBarrierReached { .. } => {}
                // 동결/재개 상태는 App(WorkspaceRuntime)에서 추적한다 — 이 뷰 캐시는 무관.
                RuntimeEvent::SessionFreezeChanged { .. } => {}
                RuntimeEvent::ScrollbackSearchResult {
                    session,
                    query,
                    result,
                } => {
                    // 늦게 도착한 stale 결과(쿼리가 이미 바뀜)는 버린다.
                    if let Some(search) = self.search.as_mut()
                        && search.session == *session
                        && search.query == *query
                    {
                        search.matches = result.matches.clone();
                        search.total_lines = result.total_lines;
                        search.capped = result.capped;
                        search.current = 0;
                        search.scroll_to_current = !search.matches.is_empty();
                    }
                }
                RuntimeEvent::EnvironmentApplied { .. } | RuntimeEvent::InputAdmitted { .. } => {}
                RuntimeEvent::LastOutputExtracted {
                    session,
                    text,
                    truncated,
                } => {
                    // copy 요청이 없으면 stale 응답(세션 소멸/교체) — 무시.
                    if !self.last_output_copy_pending.remove(session) {
                        continue;
                    }
                    if text.is_empty() {
                        // 마크 없음(복원 세션·훅 없는 셸·alt screen) 또는 출력 없는 명령 —
                        // 조용한 실패 금지, 1회 알림.
                        self.stage_notice(catalog.t("shell.no_output_marks", &[]), "");
                        continue;
                    }
                    if *truncated {
                        tracing::debug!("마지막 출력이 64KB 상한으로 잘림 — 뒤쪽만 유지");
                    }
                    self.pending_copy = Some(text.clone());
                }
            }
        }
    }

    /// 파일 트리가 이번 프레임 ⌘V/⌘C를 소비했음을 알린다 — 같은 제스처가 터미널로도
    /// 흘러 경로 삽입 붙여넣기/선택 복사가 이중 실행되는 것을 막는다(§과제②③).
    /// App이 사이드바 렌더 직후·show() 이전에 호출한다.
    pub fn suppress_clipboard_shortcuts_this_frame(&mut self, paste: bool, copy: bool) {
        self.suppress_paste_request |= paste;
        self.suppress_copy_request |= copy;
    }

    fn prepare_frame(
        &mut self,
        ctx: &egui::Context,
        events: &[RuntimeEvent],
        catalog: &i18n::Catalog,
        input_enabled: bool,
    ) {
        self.prepare_frame_with_native_input(
            ctx,
            events,
            catalog,
            input_enabled,
            crate::native_key_monitor::drain,
        );
    }

    fn prepare_frame_with_native_input(
        &mut self,
        ctx: &egui::Context,
        events: &[RuntimeEvent],
        catalog: &i18n::Catalog,
        input_enabled: bool,
        drain_native_input: impl FnOnce() -> crate::native_key_monitor::NativeKeyDownBatch,
    ) {
        self.prepare_frame_with_native_input_for_target(
            ctx,
            events,
            catalog,
            input_enabled,
            None,
            drain_native_input,
        );
    }

    fn prepare_frame_with_native_input_for_target(
        &mut self,
        ctx: &egui::Context,
        events: &[RuntimeEvent],
        catalog: &i18n::Catalog,
        input_enabled: bool,
        attached_target: Option<&AttachedPaneTarget>,
        drain_native_input: impl FnOnce() -> crate::native_key_monitor::NativeKeyDownBatch,
    ) {
        if let Some(target) = attached_target {
            self.prepare_frame_with_native_input_for_targets(
                ctx,
                events,
                catalog,
                input_enabled,
                std::slice::from_ref(target),
                input_enabled.then_some(target),
                drain_native_input,
            );
        } else {
            self.prepare_frame_with_native_input_for_targets(
                ctx,
                events,
                catalog,
                input_enabled,
                &[],
                None,
                drain_native_input,
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_frame_with_native_input_for_targets(
        &mut self,
        ctx: &egui::Context,
        events: &[RuntimeEvent],
        catalog: &i18n::Catalog,
        input_enabled: bool,
        attached_targets: &[AttachedPaneTarget],
        attached_input_owner: Option<&AttachedPaneTarget>,
        drain_native_input: impl FnOnce() -> crate::native_key_monitor::NativeKeyDownBatch,
    ) {
        self.frame_counters = renderer_egui::RenderCounters::default();
        self.terminal_focus_claimed = false;
        self.set_prepared_attached_input_owner(attached_input_owner, ctx.cumulative_pass_nr());
        // AppKit local monitor는 winit/egui가 IME 처리 중 숨길 수 있는 원본 key-down을
        // 보존한다. 매 프레임 먼저 비워 두어 검색창/설정창에서 친 키가 나중에 터미널로
        // 이월되지 않게 하고, 실제 전송은 terminal_keyboard_active pane만 수행한다.
        if input_enabled {
            let native_key_downs = drain_native_input();
            self.native_printable_key_downs = native_key_downs.printable;
            #[cfg(test)]
            self.native_printable_key_downs
                .append(&mut self.test_native_key_downs);
            self.native_clipboard_paste_requested = native_key_downs.clipboard_paste;
            self.native_clipboard_copy_requested = native_key_downs.clipboard_copy;
        } else {
            self.native_printable_key_downs.clear();
            #[cfg(test)]
            self.test_native_key_downs.clear();
            self.native_clipboard_paste_requested = false;
            self.native_clipboard_copy_requested = false;
            self.flush_pending_ime_submit();
        }
        // 파일 트리 ⌘V/⌘C 소비 프레임 — 요청을 이번 프레임 확정값으로 옮긴다(이월 없음).
        self.paste_suppressed = std::mem::take(&mut self.suppress_paste_request);
        self.copy_suppressed = std::mem::take(&mut self.suppress_copy_request);
        self.handle_events_with_attached_targets(events, catalog, attached_targets);
        // 「마지막 출력 복사」 — handle_events에는 Context가 없어 여기서 수행한다.
        if input_enabled && let Some(text) = self.pending_copy.take() {
            ctx.copy_text(text);
        }
    }

    #[allow(dead_code)]
    pub(crate) fn prepare_attached_panes(
        &mut self,
        ctx: &egui::Context,
        events: &[RuntimeEvent],
        catalog: &i18n::Catalog,
        visible_targets: &[AttachedPaneTarget],
        input_target: Option<&AttachedPaneTarget>,
    ) {
        self.prepare_attached_panes_with_native_input(
            ctx,
            events,
            catalog,
            visible_targets,
            input_target,
            crate::native_key_monitor::drain,
        );
    }

    fn prepare_attached_panes_with_native_input(
        &mut self,
        ctx: &egui::Context,
        events: &[RuntimeEvent],
        catalog: &i18n::Catalog,
        visible_targets: &[AttachedPaneTarget],
        input_target: Option<&AttachedPaneTarget>,
        drain_native_input: impl FnOnce() -> crate::native_key_monitor::NativeKeyDownBatch,
    ) {
        let visible_targets = &visible_targets[..visible_targets
            .len()
            .min(crate::ui::cross_workspace::HARD_MAX_CROSS_WORKSPACE_PANES)];
        let input_requested = input_target.is_some();
        let input_target = input_target.filter(|target| visible_targets.contains(target));
        if input_requested && input_target.is_none() {
            let _ = drain_native_input();
            self.prepare_frame_with_native_input_for_targets(
                ctx,
                events,
                catalog,
                false,
                visible_targets,
                None,
                crate::native_key_monitor::NativeKeyDownBatch::default,
            );
            return;
        }
        self.prepare_frame_with_native_input_for_targets(
            ctx,
            events,
            catalog,
            input_target.is_some(),
            visible_targets,
            input_target,
            drain_native_input,
        );
    }

    fn set_prepared_attached_input_owner(&mut self, owner: Option<&AttachedPaneTarget>, pass: u64) {
        self.prepared_attached_input_consumed = false;
        match (self.prepared_attached_input_owner.as_mut(), owner) {
            (Some(current), Some(owner)) => current.clone_from(owner),
            (None, Some(owner)) => self.prepared_attached_input_owner = Some(owner.clone()),
            (_, None) => self.prepared_attached_input_owner = None,
        }
        self.prepared_attached_input_pass = owner.map(|_| pass);
    }

    fn take_prepared_attached_input(&mut self, target: &AttachedPaneTarget, pass: u64) -> bool {
        if self.prepared_attached_input_owner.is_none() {
            return false;
        }
        if self.prepared_attached_input_pass != Some(pass) {
            self.discard_prepared_attached_input();
            return false;
        }
        if self.prepared_attached_input_consumed
            || self.prepared_attached_input_owner.as_ref() != Some(target)
        {
            return false;
        }
        self.prepared_attached_input_consumed = true;
        self.prepared_attached_input_owner = None;
        self.prepared_attached_input_pass = None;
        true
    }

    fn discard_prepared_attached_input_for(&mut self, target: &AttachedPaneTarget) {
        if self.prepared_attached_input_owner.as_ref() != Some(target) {
            return;
        }
        self.discard_prepared_attached_input();
    }

    fn discard_prepared_attached_input(&mut self) {
        self.prepared_attached_input_owner = None;
        self.prepared_attached_input_pass = None;
        self.prepared_attached_input_consumed = false;
        self.native_printable_key_downs.clear();
        self.native_clipboard_paste_requested = false;
        self.native_clipboard_copy_requested = false;
    }

    /// 홈 대시보드가 중앙 표면을 차지한 프레임에도 런타임 이벤트와 비동기 붙여넣기
    /// 결과를 계속 소비한다. 렌더만 생략하고 WorkspaceUi의 수명주기 상태는 동일하게 유지한다.
    pub fn update_hidden(
        &mut self,
        ctx: &egui::Context,
        events: &[RuntimeEvent],
        catalog: &i18n::Catalog,
    ) {
        self.update_hidden_with_native_input(
            ctx,
            events,
            catalog,
            crate::native_key_monitor::drain,
        );
    }

    fn update_hidden_with_native_input(
        &mut self,
        ctx: &egui::Context,
        events: &[RuntimeEvent],
        catalog: &i18n::Catalog,
        drain_native_input: impl FnOnce() -> crate::native_key_monitor::NativeKeyDownBatch,
    ) {
        let _ = drain_native_input();
        self.prepare_frame_with_native_input(ctx, events, catalog, false, || {
            crate::native_key_monitor::NativeKeyDownBatch::default()
        });
        self.cancel_active_split_drag(ctx);
        self.flush_command_repaint(ctx);
    }

    /// warm(비활성) 워크스페이스의 상태 선반영 — 렌더는 하지 않는다.
    ///
    /// warm은 RuntimeEvent를 pending_events에 쌓아두고 재활성 시 한 번에 replay하므로
    /// regex status와 mux 구조가 warm 진입 시점에 얼어붙었다(hook 기반 신호만 DB를 거쳐
    /// 계속 갱신). 그 사이 fleet 카드·사이드바는 warm workspace_ui를 그대로 읽으므로
    /// 종료된 pane이 계속 실행 중으로, 새로 생긴 pane은 아예 없는 것으로 보였다.
    ///
    /// 여기서는 표시 상태(mux 스냅샷·세션 status·종료 결과)와 shell spawn 완료를
    /// 즉시 반영한다. 나머지(뷰포트 스냅샷·렌더 캐시·선택/검색 정리 등 렌더 상태)는
    /// 재활성 시 replay가 담당한다. 표시 상태는 last-write-wins라 중복 적용이 안전하고,
    /// shell spawn 완료는 App이 replay에서 제외한다.
    ///
    /// 상태 변화 플래시(note_status_flash)는 일부러 걸지 않는다. 여기서 걸면 보이지도
    /// 않는 워크스페이스에서 타이머가 소진되고, 여기서 status를 이미 반영했으므로
    /// 재활성 replay의 `prev != new` 판정도 거짓이 되어 결국 플래시는 뜨지 않는다 —
    /// 즉 warm 중 일어난 전이의 플래시는 사라진다(병렬 리뷰 medium). 플래시는 "방금
    /// 일어난 일"의 주의 신호라 몇 분~몇 시간 전 전이를 재활성 시점에 몰아 띄우는 건
    /// 노이즈고, warm 워크스페이스의 그 사건들은 이미 알림 센터가 받아 둔다
    /// (process_ws_notifications는 warm에서도 돈다).
    pub fn apply_warm_events(&mut self, events: &[RuntimeEvent], catalog: &i18n::Catalog) {
        for event in events {
            match event {
                RuntimeEvent::ResizeApplied { session, stamp } => {
                    if let Some(view) = self.sessions.get_mut(session) {
                        view.observe_resize_applied(*stamp, std::time::Instant::now());
                    }
                }
                RuntimeEvent::ResizeFailed {
                    session,
                    token,
                    reason,
                } => {
                    if let Some(view) = self.sessions.get_mut(session) {
                        let target = view
                            .resize_request
                            .as_ref()
                            .filter(|request| request.token == *token)
                            .map(|request| request.target);
                        view.resize_failed(*token, *reason);
                        if view.resize_request.is_none()
                            && let Some(target) = target
                        {
                            self.failed_resize_targets.insert(*session, target);
                            self.resize_delivery_rollbacks
                                .retain(|_, rollback| rollback.session != *session);
                            self.split_final_resize_pending
                                .retain(|_, (id, _, _)| id != session);
                            self.split_final_resize_sessions.remove(session);
                        }
                    }
                }

                RuntimeEvent::MuxUpdated { snapshot } => {
                    let alive = mux_sessions(snapshot);
                    self.sessions.retain(|id, _| alive.contains(id));
                    self.mux = Some(Arc::clone(snapshot));
                }
                RuntimeEvent::SessionInputSubmitted { session, at_micros } => {
                    if *at_micros > 0 && self.session_alive(*session) {
                        let view = self.sessions.entry(*session).or_default();
                        view.last_submission_at_micros = Some(
                            view.last_submission_at_micros
                                .map_or(*at_micros, |previous| previous.max(*at_micros)),
                        );
                    }
                }
                RuntimeEvent::SessionStatusChanged { session, status } => {
                    if self.session_alive(*session) {
                        self.sessions.entry(*session).or_default().status = Some(*status);
                    }
                }
                RuntimeEvent::SessionStatusViewChanged { session, view } => {
                    if self.session_alive(*session) {
                        let entry = self.sessions.entry(*session).or_default();
                        entry.status = Some(view.status);
                        entry.status_view = Some(view.clone());
                    }
                }
                RuntimeEvent::SessionExited { session, exit_code } => {
                    self.apply_session_exit(*session, *exit_code, false);
                }
                RuntimeEvent::SessionRestored { session, exit_code } => {
                    self.apply_session_exit(*session, *exit_code, true);
                }
                // warm 워크스페이스는 화면을 그리지 않으므로 snapshot·캐시는 받지
                // 않는다. 다만 **출력이 왔다는 사실**은 기록해야 멈춤 감지가 동작한다 —
                // 물러난 워크스페이스야말로 조용히 죽은 세션이 생기는 곳이다
                // (2026-08-08 리뷰: 여기 arm이 없어 warm 세션은 영영 안 잡혔다).
                RuntimeEvent::Viewport { session, .. }
                | RuntimeEvent::ViewportTracked { session, .. } => {
                    if self.session_alive(*session) {
                        self.sessions.entry(*session).or_default().last_output_at =
                            Some(deppy_core::time::unix_secs_i64());
                    }
                }
                RuntimeEvent::SpawnFailed { kind, message } => {
                    if *kind == SpawnKind::Shell {
                        self.resolve_pending_shell_spawn(None);
                    }
                    self.error_is_pressure = false;
                    self.error = Some(crate::ui::render_message(catalog, message));
                }
                RuntimeEvent::ShellSpawned { session } => {
                    if self
                        .mux
                        .as_deref()
                        .is_some_and(|mux| visible_split_contains_session(mux, *session))
                    {
                        self.sessions
                            .entry(*session)
                            .or_default()
                            .arm_initial_presentation(std::time::Instant::now());
                    }
                    self.resolve_pending_shell_spawn(Some(*session));
                }
                RuntimeEvent::DurableEventBarrierReached { .. } => {}
                _ => {}
            }
        }
    }

    fn resolve_pending_shell_spawn(&mut self, session: Option<SessionId>) {
        let Some(index) = self
            .pending_spawn_cwds
            .iter()
            .position(|pending| matches!(pending, PendingShellSpawn::Awaiting { .. }))
        else {
            return;
        };
        let cwd = match &mut self.pending_spawn_cwds[index] {
            PendingShellSpawn::Awaiting { cwd } => cwd.take(),
            PendingShellSpawn::CwdWritePending { .. } => unreachable!(),
        };
        match (session, cwd) {
            (Some(session), Some(cwd)) => {
                self.pending_spawn_cwds[index] =
                    PendingShellSpawn::CwdWritePending { session, cwd };
            }
            _ => {
                self.pending_spawn_cwds.remove(index);
            }
        }
        self.flush_pending_spawn_cd_writes();
    }

    fn flush_pending_spawn_cd_writes(&mut self) {
        loop {
            let Some((index, session, bytes)) =
                self.pending_spawn_cwds.iter().enumerate().find_map(
                    |(index, pending)| match pending {
                        PendingShellSpawn::CwdWritePending { session, cwd } => Some((
                            index,
                            *session,
                            cd_paste_bytes(
                                std::path::Path::new(cwd),
                                self.session_shell_kind(*session),
                                false,
                            ),
                        )),
                        PendingShellSpawn::Awaiting { .. } => None,
                    },
                )
            else {
                return;
            };
            match self.queue_protocol_intent(RuntimeCommand::WriteInput { session, bytes }) {
                Ok(()) => {
                    self.pending_spawn_cwds.remove(index);
                }
                Err(WorkspaceProtocolErrorCode::Busy) => return,
                Err(code) => {
                    // spawn 자체는 이미 끝났으니 여기서 밀리면 재시도 여지가 없다 — Busy를
                    // 뺀 나머지는 report_protocol_queue_rejection 공통 규칙(진짜 유실만
                    // 배너)을 그대로 따른다.
                    self.pending_spawn_cwds.remove(index);
                    self.report_protocol_queue_rejection(code, "spawn_cd_write");
                }
            }
        }
    }

    pub fn show_with_input(
        &mut self,
        ui: &mut egui::Ui,
        config: &TerminalConfig,
        events: &[RuntimeEvent],
        catalog: &i18n::Catalog,
        input_enabled: bool,
    ) -> WorkspaceSurfaceOutput {
        // App은 첨부 pane보다 먼저 호출한다. 독립 Workspace 렌더에서도 같은 경계를 지킨다.
        self.handle_terminal_search_keys(ui.ctx());
        self.prepare_frame(ui.ctx(), events, catalog, input_enabled);
        request_terminal_os_drag_feedback_repaint(
            ui.ctx(),
            input_enabled,
            ui.input(|input| !input.raw.hovered_files.is_empty()),
        );
        self.reconcile_explicit_terminal_focus();
        self.stage_unadmitted_split_commit_for_pass(
            ui.ctx(),
            ui.ctx().cumulative_pass_nr(),
            ui.is_sizing_pass(),
        );

        // 탭바 제거 (2026-07-05): 셸 전환은 좌측 사이드바 세션 목록이 담당하고,
        // 새 셸/분할/닫기는 각 pane 헤더가 담당한다 — 셸 수만큼 탭이 늘어나
        // 상단이 넘치던 문제 해소.
        // 오류는 App logic이 take_error_notice로 알림 센터에 전달한다.
        // 이 위치에는 배너를 만들지 않아 터미널 높이와 출력 배치를 유지한다.

        let Some(mux) = self.mux.clone() else {
            self.reconcile_active_split_drag(ui.ctx(), input_enabled, None);
            // 세션이 없어도 이력 보조 탭은 유효하다 — 탭이 열려 있으면 예전처럼
            // 「새 셸」 프롬프트만 남기고 끝내지 않고 탭 스트립과 본문 rect를 만든다.
            let output = if !self.aux_tabs.is_empty() {
                self.show_session_less_aux_tabs(ui, catalog, input_enabled)
            } else if input_enabled {
                self.show_new_session_prompt(ui, catalog);
                WorkspaceSurfaceOutput::default()
            } else {
                self.show_disabled_empty_surface(ui)
            };
            self.flush_command_repaint(ui.ctx());
            return output;
        };
        // mux 포커스가 바뀐 프레임: stale 조합/스크롤 잔여분 리셋 (세션 간 이월 방지)
        if sync_runtime_focus_intent(
            &mut self.last_focused_pane,
            &mut self.pending_focus,
            self.explicit_pending_focus.as_ref(),
            mux.focused_pane.clone(),
        ) {
            // An ordinary focus acknowledgement must not force the old pane's
            // detached IME Enter out before its delayed Commit arrives.
            self.flush_active_ime_submit();
            self.preedit.clear();
            self.scroll_residual = 0.0;
            // 포커스가 옮겨간 pane 세션을 잠깐 강조(pane 전체 2초 플래시).
            if let Some(session) = mux
                .focused_pane
                .as_ref()
                .and_then(|pid| {
                    mux.tabs
                        .iter()
                        .flat_map(|tab| &tab.panes)
                        .find(|p| &p.id == pid)
                })
                .and_then(|pane| pane.session_id)
            {
                self.session_flash.insert(
                    session,
                    (std::time::Instant::now() + FOCUS_FLASH, FOCUS_FLASH),
                );
            }
        }
        // 만료된 플래시 정리(무한 성장 방지).
        let now = std::time::Instant::now();
        self.session_flash.retain(|_, (until, _)| now < *until);
        let Some(active_tab) = mux
            .active_tab
            .as_ref()
            .and_then(|id| mux.tabs.iter().find(|tab| &tab.id == id))
        else {
            self.reconcile_active_split_drag(ui.ctx(), input_enabled, None);
            // 세션이 없어도 이력 보조 탭은 유효하다 — 탭이 열려 있으면 예전처럼
            // 「새 셸」 프롬프트만 남기고 끝내지 않고 탭 스트립과 본문 rect를 만든다.
            let output = if !self.aux_tabs.is_empty() {
                self.show_session_less_aux_tabs(ui, catalog, input_enabled)
            } else if input_enabled {
                self.show_new_session_prompt(ui, catalog);
                WorkspaceSurfaceOutput::default()
            } else {
                self.show_disabled_empty_surface(ui)
            };
            self.flush_command_repaint(ui.ctx());
            return output;
        };

        self.reconcile_active_split_drag(ui.ctx(), input_enabled, Some(&active_tab.id));

        // (출력/상태 폴링 제거 — 2026-07-04 상시 리페인트 원인 조사)
        // 예전엔 "가시+실행 세션 = 50ms 폴링"으로 출력을 끌어왔다(wake가 Viewport를
        // 깨우지 않던 시절의 안전망) → 가시 idle에서 20fps 리페인트로 CPU ~10%를 상시
        // 소모했다. 이제 worker의 wake가 Viewport(dirty 게이트)·상태 이벤트 모두를
        // 깨우므로 폴링이 불필요하다: 출력/상태가 있을 때만 프레임이 돈다.

        if input_enabled {
            self.close_confirm_dialog(ui.ctx(), catalog);
        }

        let rect = ui.available_rect_before_wrap();
        let layout = &active_tab.layout;
        let layout_metrics = terminal_layout_metrics(layout);
        self.aux_tab_pane = (!self.aux_tabs.is_empty())
            .then(|| {
                aux_tab_owner_pane(
                    layout,
                    self.pending_focus.as_ref(),
                    mux.focused_pane.as_ref(),
                )
            })
            .flatten();
        let embedded_headers = keeps_embedded_pane_header(layout);
        let tab_id = active_tab.id.clone();
        let mut split_path = Vec::new();
        let pane_output = self.render_node(
            ui,
            rect,
            egui::Rect::from_min_max(
                rect.min,
                rect.max + egui::vec2(0.0, self.composer_height_expansion),
            ),
            layout,
            &layout_metrics,
            0,
            &mux,
            config,
            &tab_id,
            &mut split_path,
            embedded_headers,
            catalog,
            PaneRenderMode::Local { input_enabled },
        );

        // 응답(MuxUpdated/Viewport)을 다음 프레임에서 수신하도록 보장
        self.flush_command_repaint(ui.ctx());
        WorkspaceSurfaceOutput {
            focus_requested: pane_output.focus_requested,
            local_focus_claimed: pane_output.local_focus_claimed,
            document_drop_paths: pane_output.document_drop_paths,
            aux_tab_intent: pane_output.aux_tab_intent,
            aux_body_rect: pane_output.aux_body_rect,
            aux_search_toggle_requested: pane_output.aux_search_toggle_requested,
        }
    }

    #[allow(clippy::too_many_arguments)]
    #[allow(dead_code)]
    pub fn show_attached_pane(
        &mut self,
        ui: &mut egui::Ui,
        config: &TerminalConfig,
        events: &[RuntimeEvent],
        catalog: &i18n::Catalog,
        target: &AttachedPaneTarget,
        external_workspace_label: &str,
        availability: AttachedPaneAvailability,
        input_enabled: bool,
    ) -> AttachedPaneOutput {
        self.prepare_attached_panes(
            ui.ctx(),
            events,
            catalog,
            std::slice::from_ref(target),
            input_enabled.then_some(target),
        );
        self.show_prepared_attached_pane(
            ui,
            config,
            catalog,
            target,
            external_workspace_label,
            external_workspace_label,
            availability,
            None,
        )
        .surface
    }

    #[allow(clippy::too_many_arguments)]
    #[allow(dead_code)]
    pub(crate) fn show_prepared_attached_pane(
        &mut self,
        ui: &mut egui::Ui,
        config: &TerminalConfig,
        catalog: &i18n::Catalog,
        target: &AttachedPaneTarget,
        external_workspace_label: &str,
        attached_display_title: &str,
        availability: AttachedPaneAvailability,
        header_context: Option<AttachedPaneHeaderContext>,
    ) -> PreparedAttachedPaneOutput {
        let rect = ui.available_rect_before_wrap();
        let mut attached_ui = ui.new_child(egui::UiBuilder::new().max_rect(rect).id_salt((
            "attached_surface",
            &target.workspace_id,
            target.session.0,
        )));
        attached_ui.set_clip_rect(rect.intersect(ui.clip_rect()));
        let ui = &mut attached_ui;
        let mux = self.mux.clone();
        let pane = mux
            .as_deref()
            .and_then(|snapshot| find_attached_pane(snapshot, target));
        let target_selected = self
            .selection
            .is_some_and(|(session, _, _)| session == target.session);
        let target_has_snapshot = self.sessions.get(&target.session).is_some_and(|view| {
            view.snapshot.is_some() || (!target_selected && view.pending_snapshot.is_some())
        });
        let input_enabled = if availability == AttachedPaneAvailability::Available
            && pane.is_some()
            && target_has_snapshot
        {
            self.take_prepared_attached_input(target, ui.ctx().cumulative_pass_nr())
        } else {
            self.discard_prepared_attached_input_for(target);
            false
        };
        let header_height = TERMINAL_PANE_HEADER_HEIGHT.min(rect.height().max(0.0));
        let header = egui::Rect::from_min_max(
            rect.min,
            egui::pos2(rect.right(), rect.top() + header_height),
        );
        let body = egui::Rect::from_min_max(egui::pos2(rect.left(), header.bottom()), rect.max);
        let (detach_requested, reorder_requested) = self.render_attached_pane_header(
            ui,
            header,
            target,
            external_workspace_label,
            attached_display_title,
            catalog,
            header_context,
        );
        let mut surface = AttachedPaneOutput {
            detach_requested,
            target_present: pane.is_some(),
            ..Default::default()
        };

        if let Some(message_key) = availability.message_key() {
            self.render_attached_placeholder(
                ui,
                body,
                catalog.t(message_key, &[("workspace", external_workspace_label)]),
            );
        } else if let (Some(mux), Some(_)) = (mux.as_deref(), pane) {
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(body));
            child.set_clip_rect(body.intersect(ui.clip_rect()));
            surface.focus_requested = self
                .render_pane(
                    &mut child,
                    self.composer_height_expansion,
                    &target.pane,
                    mux,
                    config,
                    false,
                    catalog,
                    Some(&target.tab),
                    PaneRenderMode::Attached {
                        input_enabled,
                        workspace_id: &target.workspace_id,
                        workspace_label: external_workspace_label,
                        session: target.session,
                    },
                )
                .focus_requested;
        } else {
            self.render_attached_placeholder(
                ui,
                body,
                catalog.t(
                    "workspace.cross_pane.input_unavailable",
                    &[("workspace", external_workspace_label)],
                ),
            );
        }

        self.flush_command_repaint(ui.ctx());
        PreparedAttachedPaneOutput {
            surface,
            reorder_requested,
        }
    }

    #[allow(dead_code)]
    #[allow(clippy::too_many_arguments)]
    fn render_attached_pane_header(
        &self,
        ui: &mut egui::Ui,
        header: egui::Rect,
        _target: &AttachedPaneTarget,
        _external_workspace_label: &str,
        attached_display_title: &str,
        catalog: &i18n::Catalog,
        header_context: Option<AttachedPaneHeaderContext>,
    ) -> (bool, Option<AttachedPaneReorder>) {
        let tokens = crate::ui::designall::tokens(ui.visuals());
        let identity_style = attached_identity_style(
            header_context
                .map(|context| context.identity_color)
                .unwrap_or(egui::Color32::TRANSPARENT),
        );
        ui.painter()
            .rect_filled(header, 0.0, identity_style.header_fill);
        // 하단 구분선 없음 — 일반 pane 헤더와 같은 규칙(2026-08-22).
        if identity_style.top_line.color != egui::Color32::TRANSPARENT {
            ui.painter().hline(
                header.x_range(),
                pane_header_top_line_y(
                    header.top(),
                    identity_style.top_line.width,
                    ui.ctx().pixels_per_point(),
                ),
                identity_style.top_line,
            );
        }

        let reorder_requested = header_context.and_then(|header_context| {
            let response = ui.interact(
                header,
                egui::Id::new(("attached_pane_header", header_context.attachment_id)),
                egui::Sense::click_and_drag(),
            );
            if response.drag_started() {
                response.dnd_set_drag_payload(header_context.attachment_id);
            }
            release_typed_dnd_payload::<crate::ui::cross_workspace::AttachmentId>(&response).map(
                |attachment_id| {
                    AttachedPaneReorder::new(*attachment_id, header_context.destination_index)
                },
            )
        });

        let close_rect = egui::Rect::from_center_size(
            egui::pos2(header.right() - 15.0, header.center().y),
            egui::vec2(24.0, 24.0),
        );
        let detach = ui
            .put(
                close_rect,
                egui::Button::new(egui::RichText::new("×").size(16.0)).frame(false),
            )
            .on_hover_text(catalog.t("workspace.cross_pane.detach", &[]))
            .clicked();
        let text_rect = egui::Rect::from_min_max(
            egui::pos2(header.left() + 8.0, header.top()),
            egui::pos2(close_rect.left() - 4.0, header.bottom()),
        );
        let title_color = if identity_style.top_line.color != egui::Color32::TRANSPARENT
            && ui.rect_contains_pointer(header)
        {
            identity_style.top_line.color
        } else {
            tokens.muted_text
        };
        let mut title_job = egui::text::LayoutJob::single_section(
            attached_display_title.to_owned(),
            egui::TextFormat {
                font_id: egui::FontId::proportional(12.0),
                color: title_color,
                ..Default::default()
            },
        );
        title_job.wrap = egui::text::TextWrapping {
            max_width: text_rect.width().max(0.0),
            max_rows: 1,
            break_anywhere: true,
            overflow_character: Some('…'),
        };
        let title_galley = ui.painter().layout_job(title_job);
        ui.painter().with_clip_rect(text_rect).galley(
            egui::pos2(
                text_rect.left(),
                text_rect.center().y - title_galley.size().y / 2.0,
            ),
            title_galley,
            title_color,
        );
        (detach, reorder_requested)
    }

    #[allow(dead_code)]
    fn render_attached_placeholder(&self, ui: &mut egui::Ui, rect: egui::Rect, message: String) {
        let tokens = crate::ui::designall::tokens(ui.visuals());
        ui.painter().rect_filled(rect, 0.0, tokens.app_background);
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(rect)
                .layout(egui::Layout::top_down(egui::Align::Center)),
        );
        child.centered_and_justified(|ui| {
            ui.add_enabled(false, egui::Label::new(message));
        });
    }

    fn show_new_session_prompt(&mut self, ui: &mut egui::Ui, catalog: &i18n::Catalog) {
        ui.centered_and_justified(|ui| {
            if ui
                .button(catalog.t("workspace.new_shell", &[]))
                .on_hover_text(catalog.t("workspace.start_shell_prompt", &[]))
                .clicked()
            {
                self.new_session_requested = Some(NewSessionRequest::NewTab);
            }
        });
    }

    /// 세션이 하나도 없는 워크스페이스에서 보조 탭이 열려 있을 때의 화면.
    ///
    /// 이력 데이터는 workspace-scoped라 세션이 없어도 유효하다(핸드오프 기본값 3).
    /// 세션 탭 자리에는 「세션 없음」 탭을 두고, 그 탭을 고르면 본문이 기존 「새 셸」
    /// 진입점으로 돌아간다 — 세션이 없다는 사실과 만드는 길이 함께 보여야 한다.
    fn show_session_less_aux_tabs(
        &mut self,
        ui: &mut egui::Ui,
        catalog: &i18n::Catalog,
        input_enabled: bool,
    ) -> WorkspaceSurfaceOutput {
        let rect = ui.available_rect_before_wrap();
        let header_height = TERMINAL_PANE_HEADER_HEIGHT.min(rect.height().max(0.0) * 0.5);
        let header = egui::Rect::from_min_max(
            rect.min,
            egui::pos2(rect.right(), rect.top() + header_height),
        );
        let body = egui::Rect::from_min_max(egui::pos2(rect.left(), header.bottom()), rect.max);
        let font = egui::FontId::proportional(13.0);
        let tokens = crate::ui::designall::tokens(ui.visuals());
        let empty_label = catalog.t("workspace.tab.no_session", &[]);
        let empty_width = ui
            .painter()
            .layout_no_wrap(empty_label.clone(), font.clone(), egui::Color32::WHITE)
            .size()
            .x;
        // 세션 탭에는 닫을 pane이 없다 — 폭 0 rect를 닫기 자리로 넘겨 보조 탭이
        // 라벨 바로 뒤에서 시작하게 한다(같은 accent 경계 규칙 재사용).
        let label_left = header.left() + PANE_HEADER_TITLE_LEFT;
        let pseudo_close = egui::Rect::from_min_max(
            egui::pos2(label_left + empty_width, header.center().y),
            egui::pos2(label_left + empty_width, header.center().y),
        );
        let placements = layout_aux_tabs(
            header,
            pseudo_close,
            header.right() - 4.0,
            &self.aux_tabs,
            |label| {
                ui.painter()
                    .layout_no_wrap(label.to_owned(), font.clone(), egui::Color32::WHITE)
                    .size()
                    .x
            },
        );
        let active_kind = placements.iter().find(|p| p.active).map(|p| p.kind);
        let any_active = active_kind.is_some();

        let style = pane_header_style(self.workspace_accent, true);
        let accent_range = Some(match placements.iter().find(|p| p.active) {
            Some(placement) => egui::Rangef::new(
                placement.geometry.tab.left(),
                placement.geometry.tab.right().min(header.right()),
            ),
            None => egui::Rangef::new(
                header.left(),
                pane_header_active_boundary(header, pseudo_close),
            ),
        });
        paint_pane_header_base(ui, header, style, accent_range);
        paint_tab_divider(
            ui,
            header,
            pane_header_active_boundary(header, pseudo_close),
            style,
        );

        let empty_clip = egui::Rect::from_min_max(
            egui::pos2(label_left, header.top()),
            egui::pos2(pseudo_close.left().min(header.right()), header.bottom()),
        );
        let empty_response = ui.interact(
            egui::Rect::from_min_max(
                header.min,
                egui::pos2(
                    placements
                        .first()
                        .map_or(pane_header_active_boundary(header, pseudo_close), |p| {
                            p.geometry.tab.left()
                        }),
                    header.bottom(),
                ),
            ),
            ui.id().with("workspace_session_less_tab"),
            egui::Sense::click(),
        );
        paint_tab_label(
            ui.painter(),
            empty_clip,
            header.center().y,
            font.clone(),
            if ui.rect_contains_pointer(empty_response.rect) {
                style.line_stroke(ui.visuals()).color
            } else if any_active {
                tokens.muted_text
            } else {
                tokens.text
            },
            empty_label,
        );

        let mut output = WorkspaceSurfaceOutput::default();
        if empty_response.clicked()
            && let Some(kind) = active_kind
        {
            output.aux_tab_intent = Some((kind, PaneAuxTabIntent::ShowSession));
        }
        for (index, placement) in placements.iter().enumerate() {
            if let Some(intent) = self.render_aux_tab(
                ui,
                header,
                placement.geometry,
                &placement.label,
                placement.active,
                ui.id().with(("workspace_session_less_aux", index)),
                catalog,
                &font,
                placement.kind,
                style.line_stroke(ui.visuals()).color,
            ) {
                output.aux_tab_intent = Some((placement.kind, intent));
            }
        }

        let blank_left = placements
            .last()
            .map_or(pane_header_active_boundary(header, pseudo_close), |p| {
                p.geometry.tab.right()
            });
        if header.right() > blank_left {
            let blank = ui.interact(
                egui::Rect::from_min_max(egui::pos2(blank_left, header.top()), header.max),
                ui.id().with("session_less_new_session_strip"),
                egui::Sense::click(),
            );
            blank.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::WidgetType::Button,
                    input_enabled,
                    catalog.t("workspace.new_shell", &[]),
                )
            });
            if blank.clicked() {
                if input_enabled {
                    self.new_session_requested = Some(NewSessionRequest::NewTab);
                } else {
                    output.focus_requested = true;
                }
            }
        }

        if any_active {
            output.aux_body_rect = Some(body);
        } else {
            let mut child = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(body)
                    .id_salt("workspace_session_less_body"),
            );
            child.set_clip_rect(body.intersect(ui.clip_rect()));
            if input_enabled {
                self.show_new_session_prompt(&mut child, catalog);
            } else {
                output.focus_requested |=
                    self.show_disabled_empty_surface(&mut child).focus_requested;
            }
        }
        output
    }

    fn show_disabled_empty_surface(&self, ui: &mut egui::Ui) -> WorkspaceSurfaceOutput {
        let response = ui.interact(
            ui.available_rect_before_wrap(),
            ui.id().with("disabled_empty_workspace_surface"),
            egui::Sense::click(),
        );
        WorkspaceSurfaceOutput {
            focus_requested: response.clicked(),
            ..Default::default()
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render_pane_header(
        &mut self,
        ui: &mut egui::Ui,
        header: egui::Rect,
        pane: &runtime::PaneSnapshot,
        focused: bool,
        config: &TerminalConfig,
        catalog: &i18n::Catalog,
        input_enabled: bool,
    ) -> PaneRenderOutput {
        let mut output = PaneRenderOutput::default();
        let osc = self.session_osc_title(pane.session_id);
        let full_title =
            self.resolve_session_title(&pane.title, pane.session_id, osc.as_deref(), catalog);
        let font = egui::FontId::proportional(13.0);
        let full_title_width = ui
            .painter()
            .layout_no_wrap(full_title.clone(), font.clone(), egui::Color32::WHITE)
            .size()
            .x;
        let toolbar_icons = [
            TerminalToolbarIcon::Search,
            TerminalToolbarIcon::NewTerminal,
            TerminalToolbarIcon::SplitColumns,
            TerminalToolbarIcon::SplitRows,
        ];
        let title_left = header.left() + PANE_HEADER_TITLE_LEFT;

        // 보조 탭(이력·Git)은 이 프레임의 **소유 pane** 헤더에만 붙는다. 라벨을 먼저 재서
        // 세션 제목이 쓸 수 있는 폭에서 미리 빼둔다 — 뒤늦게 겹치는 일이 없게.
        // 여기서 뺄 폭은 **모든 탭의 합**이다 — 탭이 둘이면 둘 다 세션 제목보다 우선한다.
        let owns_aux_tab = self
            .aux_tab_pane
            .as_ref()
            .is_some_and(|owner| owner == &pane.id);
        let aux_tabs: Vec<PaneAuxTab> = if owns_aux_tab {
            self.aux_tabs.clone()
        } else {
            Vec::new()
        };
        let aux_reserved_width: f32 = aux_tabs
            .iter()
            .map(|tab| {
                let natural = ui
                    .painter()
                    .layout_no_wrap(tab.label.clone(), font.clone(), egui::Color32::WHITE)
                    .size()
                    .x;
                pane_aux_tab_width(pane_aux_tab_label_width(
                    header.width(),
                    natural,
                    aux_tabs.len(),
                )) + PANE_AUX_TAB_RIGHT_PAD
            })
            .sum();

        // 우측 도구 4개를 모두 표시하던 기존 제목 폭을 기준으로 실제 글자 수를 구한 뒤
        // 10자를 더 허용한다. 추가 폭이 필요하면 기존 규칙대로 왼쪽 도구부터 숨긴다.
        let full_toolbar_width = PANE_HEADER_TOOLBAR_BUTTON * toolbar_icons.len() as f32
            + PANE_HEADER_TOOLBAR_GAP * toolbar_icons.len().saturating_sub(1) as f32;
        let original_title_width =
            (header.right() - 4.0 - full_toolbar_width - aux_reserved_width - 24.0 - title_left)
                .max(0.0);
        let full_char_count = full_title.chars().count();
        let original_char_capacity = if full_title_width > 0.0 {
            ((full_char_count as f32 * original_title_width / full_title_width).floor() as usize)
                .min(full_char_count)
        } else {
            full_char_count
        };
        let title_char_limit = (original_char_capacity + 10).min(full_char_count);
        let title = if title_char_limit < full_char_count {
            let mut shortened: String = full_title.chars().take(title_char_limit).collect();
            shortened.push('…');
            shortened
        } else {
            full_title
        };
        let title_width = ui
            .painter()
            .layout_no_wrap(title.clone(), font.clone(), egui::Color32::WHITE)
            .size()
            .x;

        let buttons =
            pane_header_buttons(header, title_width, toolbar_icons.len(), aux_reserved_width);
        let center_y = header.center().y;
        let close = buttons.close;
        let placements = layout_aux_tabs(header, close, buttons.toolbar_left, &aux_tabs, |label| {
            ui.painter()
                .layout_no_wrap(label.to_owned(), font.clone(), egui::Color32::WHITE)
                .size()
                .x
        });
        let active_kind = placements.iter().find(|p| p.active).map(|p| p.kind);
        // 헤더 chrome(제목 밝기·검색 라우팅)은 레이아웃 결과(placements)가 아니라
        // 실제 활성 상태(aux_tabs, ground truth)를 따라야 한다 — 헤더가 극단적으로
        // 좁으면 `layout_aux_tabs`가 활성 탭까지 접어(빈 Vec) 돌려줄 수 있는데, 본문
        // 게이트는 이미 이 목록으로 문서를 그리고 있어(레이아웃과 무관) 헤더만 다른
        // 기준을 쓰면 "세션이 선택된 것처럼" 어긋나 보인다(2026-08-22 리뷰).
        let aux_active = any_aux_tab_active(&aux_tabs);
        let tokens = crate::ui::designall::tokens(ui.visuals());
        let style = pane_header_style(self.workspace_accent, focused);
        // 상단 accent는 **선택된 탭**만 덮는다. 보조 탭이 붙으면 이 선의 범위가 곧
        // 탭 선택 표시라, 별도 선택 위젯을 새로 만들지 않고 같은 chrome을 나눠 쓴다.
        let accent_range = Some(match placements.iter().find(|p| p.active) {
            Some(placement) => egui::Rangef::new(
                placement.geometry.tab.left(),
                placement.geometry.tab.right().min(header.right()),
            ),
            None => egui::Rangef::new(header.left(), pane_header_active_boundary(header, close)),
        });
        paint_pane_header_base(ui, header, style, accent_range);
        paint_tab_divider(
            ui,
            header,
            pane_header_active_boundary(header, close),
            style,
        );

        let header_response = ui.interact(
            egui::Rect::from_min_max(
                header.min,
                egui::pos2(pane_header_active_boundary(header, close), header.bottom()),
            ),
            egui::Id::new(("terminal_pane_header", &pane.id)),
            egui::Sense::click(),
        );
        if header_response.clicked() {
            self.terminal_focus_claimed = true;
            output.local_focus_claimed = Some(pane.id.clone());
            output.focus_requested |= !input_enabled;
            self.request_pane_focus(pane.id.clone());
            // 보조 탭이 활성인 동안 세션 탭(헤더의 남은 영역)을 누르면 터미널로 돌아간다.
            // 보조 탭·보조 닫기는 **나중에** 등록돼 이 응답을 가져가므로 여기 오지 않는다.
            if let Some(kind) = active_kind {
                output.aux_tab_intent = Some((kind, PaneAuxTabIntent::ShowSession));
            }
        }
        if input_enabled {
            self.pane_context_menu(&header_response, &pane.id, config, catalog);
        }

        // Cover the vacant strip through the right edge, including empty toolbar margins.
        // Actual toolbar buttons register later and retain priority over this background.
        let blank_left = placements
            .last()
            .map_or(pane_header_active_boundary(header, close), |p| {
                p.geometry.tab.right()
            });
        if header.right() > blank_left {
            // Auxiliary bodies fence terminal input, while the selector remains a header action.
            let launcher_enabled =
                (input_enabled || aux_active) && !super::popup::modal_input_blocked(ui.ctx());
            let blank = ui.interact(
                egui::Rect::from_min_max(egui::pos2(blank_left, header.top()), header.max),
                egui::Id::new(("terminal_new_session_strip", &pane.id)),
                egui::Sense::click(),
            );
            blank.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::WidgetType::Button,
                    launcher_enabled,
                    catalog.t("workspace.new_shell", &[]),
                )
            });
            if input_enabled {
                self.pane_context_menu(&blank, &pane.id, config, catalog);
            }
            if blank
                .on_hover_text(catalog.t("workspace.new_shell", &[]))
                .clicked()
            {
                self.terminal_focus_claimed = true;
                output.local_focus_claimed = Some(pane.id.clone());
                self.request_pane_focus(pane.id.clone());
                if launcher_enabled {
                    self.new_session_requested =
                        Some(NewSessionRequest::SplitRight(pane.id.clone()));
                } else {
                    output.focus_requested = true;
                }
            }
        }

        // 포커스 점은 없앴다(2026-08-11 사용자: 상태 표시와 색이 달라 헷갈린다).
        // 사이드바에서 같은 크기·같은 자리의 점이 **세션 상태**(실행/대기/완료/오류)를
        // 나르는데, 이 점만 accent 청록으로 「포커스됨」을 뜻해 같은 기호가 두 뜻을
        // 가졌다. 포커스는 이미 셋이 말한다 — 헤더 배경(style.background), 상단 accent
        // 선(style.active_stroke), 제목 밝기(text vs muted_text). 네 번째 신호를
        // 지우면서 팔레트 충돌도 함께 사라진다.

        let title_right = (close.left() - 3.0).max(title_left);
        let title_clip = egui::Rect::from_min_max(
            egui::pos2(title_left, header.top()),
            egui::pos2(title_right, header.bottom()),
        );
        // 보조 탭이 활성이면 세션 탭은 포커스된 pane이라도 선택 해제 상태로 읽혀야 한다.
        let title_color = if ui.rect_contains_pointer(header_response.rect) {
            style.line_stroke(ui.visuals()).color
        } else if focused && !aux_active {
            tokens.text
        } else {
            tokens.muted_text
        };
        paint_tab_label(
            ui.painter(),
            title_clip,
            center_y,
            font.clone(),
            title_color,
            title,
        );
        let close_response = ui.interact(
            close,
            egui::Id::new(("terminal_close_tab", &pane.id)),
            egui::Sense::click(),
        );
        // 닫기는 되돌리기 어려운 동작이라 hover에서 error 톤을 준다(브라우저 탭·에디터의
        // 관례). 원래는 tokens.success였는데 "닫기"가 초록으로 밝아지는 건 의미가 반대고,
        // success 색을 조정할 때 이 버튼이 따라 바뀌는 결합도 생긴다(2026-08-07).
        let close_color = if close_response.hovered() || close_response.has_focus() {
            tokens.error
        } else {
            tokens.text
        };
        paint_close_glyph(ui.painter(), close.center(), close_color);
        let close_clicked = close_response
            .on_hover_text(catalog.t("workspace.close_pane", &[]))
            .clicked();
        if close_clicked {
            if input_enabled {
                self.request_close_pane(pane.id.clone());
            } else {
                self.terminal_focus_claimed = true;
                output.focus_requested = true;
                output.local_focus_claimed = Some(pane.id.clone());
                self.request_pane_focus(pane.id.clone());
            }
        }

        let first_toolbar = toolbar_icons.len() - buttons.toolbar.len();
        for (index, (icon, rect)) in toolbar_icons[first_toolbar..]
            .iter()
            .copied()
            .zip(buttons.toolbar.iter().copied())
            .enumerate()
        {
            let response = terminal_toolbar_button(
                ui,
                rect,
                egui::Id::new(("terminal_toolbar", &pane.id, first_toolbar + index)),
                icon,
            );
            let tooltip = match icon {
                TerminalToolbarIcon::Search => catalog.t("shortcuts.action.terminal_search", &[]),
                TerminalToolbarIcon::NewTerminal => catalog.t("workspace.new_shell", &[]),
                TerminalToolbarIcon::SplitColumns => catalog.t("workspace.split_horizontal", &[]),
                TerminalToolbarIcon::SplitRows => catalog.t("workspace.split_vertical", &[]),
            };
            if response.on_hover_text(tooltip).clicked() {
                if search_click_targets_aux_search(icon, aux_active) {
                    // 보조 본문(이력·Git)이 활성이면 Search는 터미널 검색이 아니라
                    // 보조 검색을 토글한다 — `input_enabled` 게이트는 건드리지 않고
                    // (입력 소유권 fail-closed 계약 유지) 그 앞에 별도 경로만 더한다.
                    // 실제 토글은 App(`aux_search.toggle()`)이 한다.
                    output.aux_search_toggle_requested = true;
                } else if input_enabled {
                    if !focused {
                        self.request_pane_focus(pane.id.clone());
                    }
                    self.activate_terminal_toolbar(icon, &pane.id, config);
                } else {
                    self.terminal_focus_claimed = true;
                    output.focus_requested = true;
                    output.local_focus_claimed = Some(pane.id.clone());
                    self.request_pane_focus(pane.id.clone());
                    // 분할 포커스 전환 중에도 검색은 첫 클릭에 연다. PTY 실행 도구는
                    // 기존 입력 소유권 게이트를 유지한다.
                    if matches!(icon, TerminalToolbarIcon::Search) {
                        self.activate_terminal_toolbar(icon, &pane.id, config);
                    }
                }
            }
        }

        // 보조 탭은 헤더·닫기·도구를 모두 등록한 **뒤**에 올린다. egui는 겹칠 때 나중에
        // 등록된 위젯이 클릭을 가져가므로, 이 순서가 곧 "보조 탭 > 세션 헤더" 우선순위다.
        for (index, placement) in placements.iter().enumerate() {
            if let Some(intent) = self.render_aux_tab(
                ui,
                header,
                placement.geometry,
                &placement.label,
                placement.active,
                egui::Id::new(("terminal_pane_aux", &pane.id, index)),
                catalog,
                &font,
                placement.kind,
                style.line_stroke(ui.visuals()).color,
            ) {
                output.aux_tab_intent = Some((placement.kind, intent));
            }
        }
        output
    }

    /// 보조 탭 한 벌 — 라벨(=활성화)과 닫기(=탭 제거)를 각각 독립 히트박스로 올린다.
    /// 닫기는 탭 뒤에 등록돼 겹칠 때 우선한다. 세션 헤더와 세션 없는 스트립이 이 하나를
    /// 공유하므로 두 화면의 동작이 갈라지지 않는다.
    #[allow(clippy::too_many_arguments)]
    fn render_aux_tab(
        &mut self,
        ui: &mut egui::Ui,
        header: egui::Rect,
        geometry: PaneAuxTabGeometry,
        label: &str,
        active: bool,
        id: egui::Id,
        catalog: &i18n::Catalog,
        font: &egui::FontId,
        kind: PaneAuxTabKind,
        hover_color: egui::Color32,
    ) -> Option<PaneAuxTabIntent> {
        let tokens = crate::ui::designall::tokens(ui.visuals());
        let mut intent = None;
        let tab_response = ui.interact(geometry.tab, id.with("tab"), egui::Sense::click());
        let label_color = if ui.rect_contains_pointer(tab_response.rect) {
            hover_color
        } else if active {
            tokens.text
        } else {
            tokens.muted_text
        };
        let label_clip = egui::Rect::from_min_max(
            egui::pos2(geometry.label_left, header.top()),
            egui::pos2(
                (geometry.label_left + geometry.label_width).min(header.right()),
                header.bottom(),
            ),
        );
        paint_tab_label(
            ui.painter(),
            label_clip,
            header.center().y,
            font.clone(),
            label_color,
            label.to_owned(),
        );
        if tab_response
            .on_hover_text(catalog.t(kind.hint_key(), &[]))
            .clicked()
        {
            intent = Some(PaneAuxTabIntent::Activate);
        }

        if let Some(aux_close) = geometry.close {
            let close_response = ui.interact(aux_close, id.with("close"), egui::Sense::click());
            // 세션 X와 달리 error 톤을 쓰지 않는다 — UI 탭만 닫을 뿐 세션은 그대로다.
            let close_color = if close_response.hovered() || close_response.has_focus() {
                tokens.text
            } else {
                tokens.muted_text
            };
            paint_close_glyph(ui.painter(), aux_close.center(), close_color);
            if close_response
                .on_hover_text(catalog.t(kind.close_key(), &[]))
                .clicked()
            {
                intent = Some(PaneAuxTabIntent::Close);
            }
        }
        intent
    }

    fn activate_terminal_toolbar(
        &mut self,
        icon: TerminalToolbarIcon,
        pane: &runtime::MuxPaneId,
        config: &TerminalConfig,
    ) {
        match icon {
            TerminalToolbarIcon::Search => {
                if let Some(session) = self
                    .mux
                    .as_ref()
                    .and_then(|mux| {
                        mux.tabs
                            .iter()
                            .flat_map(|tab| &tab.panes)
                            .find(|candidate| &candidate.id == pane)
                    })
                    .and_then(|pane| pane.session_id)
                {
                    self.open_search_for_session(session);
                }
            }
            TerminalToolbarIcon::NewTerminal => {
                self.new_session_requested = Some(NewSessionRequest::NewTab)
            }
            TerminalToolbarIcon::SplitColumns => self.send(RuntimeCommand::SplitPane {
                pane: pane.clone(),
                direction: SplitDirection::Horizontal,
                scrollback_lines: config.scrollback_lines as usize,
            }),
            TerminalToolbarIcon::SplitRows => self.send(RuntimeCommand::SplitPane {
                pane: pane.clone(),
                direction: SplitDirection::Vertical,
                scrollback_lines: config.scrollback_lines as usize,
            }),
        }
    }

    /// layout 트리를 rect 분할로 재귀 렌더한다.
    #[allow(clippy::too_many_arguments)]
    fn render_node(
        &mut self,
        ui: &mut egui::Ui,
        rect: egui::Rect,
        sizing_rect: egui::Rect,
        node: &LayoutNode,
        layout_metrics: &[TerminalLayoutMetric],
        metric_index: usize,
        mux: &MuxSnapshot,
        config: &TerminalConfig,
        tab_id: &runtime::MuxTabId,
        path: &mut Vec<u8>,
        embedded_headers: bool,
        catalog: &i18n::Catalog,
        mode: PaneRenderMode<'_>,
    ) -> PaneRenderOutput {
        match node {
            LayoutNode::Pane(pane_id) => {
                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect));
                // max_rect는 배치만 제한한다 — 이전 크기의 스냅샷이 이웃 pane을
                // 덮어 그리지 않게 페인터 클립도 pane 영역으로 줄인다
                child.set_clip_rect(rect.intersect(ui.clip_rect()));
                self.render_pane(
                    &mut child,
                    sizing_rect.height() - rect.height(),
                    pane_id,
                    mux,
                    config,
                    embedded_headers,
                    catalog,
                    None,
                    mode,
                )
                // 포커스 표시는 각 pane 헤더의 accent top line이 담당한다.
            }
            LayoutNode::Split {
                direction,
                ratio,
                first,
                second,
            } => {
                let first_metric_index = metric_index + 1;
                let second_metric_index =
                    first_metric_index + layout_metrics[first_metric_index].subtree_len;
                let first_min = layout_metrics[first_metric_index].min_size;
                let second_min = layout_metrics[second_metric_index].min_size;
                // 목업처럼 pane을 붙이고 1px 구분선만 둔다 (기존 4px 투명 gap 제거).
                // 리사이즈 잡기는 split_handle이 히트영역을 ±2px 확장해 보장한다.
                let gap = terminal_split_gap(rect, *direction);
                // egui는 이전 pass의 widget rect로 현재 drag owner를 먼저 확정한다. 그 owner를
                // child보다 먼저 읽어 transaction/fence가 같은 프레임의 pane settlement보다
                // 앞서게 한다. 실제 interact 등록은 hit 우선권을 위해 계속 child 뒤에 둔다.
                if mode.input_enabled()
                    && ui.ctx().is_being_dragged(split_handle_id(tab_id, path))
                    && let Some(pointer) = ui.input(|input| input.pointer.interact_pos())
                {
                    let requested_ratio = match direction {
                        SplitDirection::Horizontal => {
                            (pointer.x - rect.min.x) / (rect.width() - gap)
                        }
                        SplitDirection::Vertical => {
                            (pointer.y - rect.min.y) / (rect.height() - gap)
                        }
                    };
                    let ratio = terminal_split_ratio(
                        rect,
                        *direction,
                        requested_ratio,
                        first_min,
                        second_min,
                    );
                    self.begin_split_drag(tab_id.clone(), path.to_vec(), ratio);
                }
                // 드래그 중이면 로컬 미리보기 ratio 사용 (릴리즈 시에만 명령 전송)
                let requested_ratio = self.split_preview_ratio(tab_id, path, *ratio);
                // 저장된 ratio가 오래된 10% 규칙이나 remote snapshot에서 왔더라도 현재
                // rect와 subtree의 실제 minimum으로 다시 제한한다. 창 축소도 같은 경로라
                // divider를 드래그하지 않아도 모든 leaf가 가능한 한 50px를 유지한다.
                let ratio =
                    terminal_split_ratio(rect, *direction, requested_ratio, first_min, second_min);
                let (first_rect, second_rect, gap_rect) =
                    terminal_split_rects(rect, *direction, ratio);
                let sizing_ratio = terminal_split_ratio(
                    sizing_rect,
                    *direction,
                    requested_ratio,
                    first_min,
                    second_min,
                );
                let (first_sizing_rect, second_sizing_rect, _) =
                    terminal_split_rects(sizing_rect, *direction, sizing_ratio);
                path.push(0);
                let mut output = self.render_node(
                    ui,
                    first_rect,
                    first_sizing_rect,
                    first,
                    layout_metrics,
                    first_metric_index,
                    mux,
                    config,
                    tab_id,
                    path,
                    embedded_headers,
                    catalog,
                    mode,
                );
                path.pop();
                path.push(1);
                output.merge(self.render_node(
                    ui,
                    second_rect,
                    second_sizing_rect,
                    second,
                    layout_metrics,
                    second_metric_index,
                    mux,
                    config,
                    tab_id,
                    path,
                    embedded_headers,
                    catalog,
                    mode,
                ));
                path.pop();
                // 핸들은 자식 pane들 **뒤에** 등록 — egui 히트테스트는 나중 등록이
                // 우선이라, ±2px 확장 히트영역이 터미널 선택 드래그에 밀리지 않는다
                // (codex 리뷰: 가장자리에서 리사이즈 대신 선택이 잡히는 문제).
                if mode.input_enabled() {
                    self.split_handle(ui, rect, gap_rect, *direction, tab_id, path);
                } else {
                    ui.painter().rect_filled(
                        gap_rect,
                        0.0,
                        crate::ui::designall::tokens(ui.visuals()).separator,
                    );
                }
                output
            }
        }
    }

    /// split 경계 드래그 핸들 — 드래그 중엔 split_drag로 로컬 미리보기만 갱신하고,
    /// 릴리즈 시 1회 ResizeSplit을 보낸다 (드래그 내내 DB 저장/이벤트 폭주 방지).
    #[allow(clippy::too_many_arguments)]
    fn split_handle(
        &mut self,
        ui: &mut egui::Ui,
        rect: egui::Rect,
        gap_rect: egui::Rect,
        direction: SplitDirection,
        tab_id: &runtime::MuxTabId,
        path: &[u8],
    ) {
        // 1px 경계는 잡기 어려우니 히트 영역만 양쪽 2px씩 확장 (시각 폭은 그대로)
        let hit_rect = terminal_split_hit_rect(rect, gap_rect, direction);
        let id = split_handle_id(tab_id, path);
        let resp = ui.interact(hit_rect, id, egui::Sense::drag());
        let cursor = match direction {
            SplitDirection::Horizontal => egui::CursorIcon::ResizeHorizontal,
            SplitDirection::Vertical => egui::CursorIcon::ResizeVertical,
        };
        let resp = resp.on_hover_cursor(cursor);
        let tokens = crate::ui::designall::tokens(ui.visuals());
        let color = if resp.hovered() || resp.dragged() {
            tokens.accent
        } else {
            tokens.separator
        };
        ui.painter().rect_filled(gap_rect, 0.0, color);
        if resp.drag_stopped()
            && self
                .split_drag
                .as_ref()
                .is_some_and(|transaction| &transaction.tab == tab_id && transaction.path == path)
        {
            self.commit_split_drag(ui.ctx().cumulative_pass_nr());
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render_pane(
        &mut self,
        ui: &mut egui::Ui,
        extra_height: f32,
        pane_id: &runtime::MuxPaneId,
        mux: &MuxSnapshot,
        config: &TerminalConfig,
        embedded_header: bool,
        catalog: &i18n::Catalog,
        tab_scope: Option<&runtime::MuxTabId>,
        mode: PaneRenderMode<'_>,
    ) -> PaneRenderOutput {
        let Some(pane) = mux.tabs.iter().find_map(|tab| {
            (tab_scope.is_none_or(|tab_id| &tab.id == tab_id))
                .then(|| {
                    tab.panes.iter().find(|pane| {
                        &pane.id == pane_id
                            && match mode {
                                PaneRenderMode::Local { .. } => true,
                                PaneRenderMode::Attached { session, .. } => {
                                    pane.session_id == Some(session)
                                }
                            }
                    })
                })
                .flatten()
        }) else {
            return PaneRenderOutput::default();
        };
        let input_enabled = mode.input_enabled();
        let mut render_output = PaneRenderOutput::default();
        let focused = mux.focused_pane.as_ref() == Some(pane_id);
        let surface_focused = if mode.is_local() {
            focused
        } else {
            input_enabled
        };
        let show_archived_notice = pane
            .session_id
            .and_then(|session| self.sessions.get(&session))
            .is_some_and(|view| view.restored_readonly && view.exit_code.is_some());
        let pane_layout =
            terminal_pane_layout_for_state(ui.max_rect(), embedded_header, show_archived_notice);
        let sizing_layout = terminal_pane_layout_for_state(
            egui::Rect::from_min_max(
                ui.max_rect().min,
                ui.max_rect().max + egui::vec2(0.0, extra_height),
            ),
            embedded_header,
            show_archived_notice,
        );
        let extra_height = sizing_layout.content.height() - pane_layout.content.height();
        let pane_rect = pane_layout.surface;
        let tokens = crate::ui::designall::tokens(ui.visuals());
        ui.painter()
            .rect_filled(pane_rect, 0.0, tokens.app_background);
        if embedded_header && mode.is_local() {
            let header_output = self.render_pane_header(
                ui,
                pane_layout.header,
                pane,
                focused,
                config,
                catalog,
                input_enabled,
            );
            render_output.merge(header_output);
            if self.has_pending_close_confirmation() || self.new_session_requested.is_some() {
                super::popup::set_pending_modal(ui.ctx(), true);
            }
            // 보조 탭이 활성이면 이 pane의 본문은 App이 그린다. 터미널 표면·입력·drop·
            // 컨텍스트 메뉴를 전부 건너뛰어(fail-closed) 숨은 PTY로 입력이 새지 않게 한다.
            if self
                .aux_tab_pane
                .as_ref()
                .is_some_and(|owner| owner == pane_id)
                && self.aux_tabs.iter().any(|tab| tab.active)
            {
                render_output.aux_body_rect = Some(pane_layout.surface);
                return render_output;
            }
        }
        // pane 전체 배경 interact — 터미널 위젯보다 먼저 등록해 터미널 밖 영역과
        // "세션 없음"/"연결 중"(스냅샷 지연) 상태에서도 우클릭 메뉴·드롭이 동작한다
        // (codex P2). 터미널 위에서는 나중에 등록되는 터미널 위젯이 입력을 받는다.
        let pane_resp = ui.interact(
            pane_rect,
            pane_interaction_id(mode, "pane_bg", pane_id),
            egui::Sense::click(),
        );
        if pane_resp.clicked() {
            if mode.is_local() {
                self.terminal_focus_claimed = true;
                render_output.local_focus_claimed = Some(pane_id.clone());
                render_output.focus_requested |= !input_enabled;
                self.request_pane_focus(pane_id.clone());
            } else {
                render_output.focus_requested = true;
            }
        }
        if mode.is_local() && input_enabled {
            self.pane_context_menu(&pane_resp, pane_id, config, catalog);
        }

        // 제목/닫기/검색/새 셸/분할은 각 leaf의 얇은 헤더에 있고, 본문은 그 아래를
        // 카드 외곽 여백 없이 채운다.
        // 텍스트는 셀 위치를 아는 draw 이후에 삽입 마커를, 파일은
        // 문서 탭으로 열린다는 별도 표시를 그린다.
        let mut drop_feedback = None;
        if mode.is_local() && input_enabled && pane.session_id.is_some() {
            // OS 파일 드롭 — 이 pane 위에서 놓으면 App이 문서 탭으로 열 경로 intent를
            // 올린다. 텍스트 드롭만 기존처럼 터미널 입력으로 보낸다.
            // winit 0.30이 macOS draggingUpdated:를 구현하지 않아 드래그 중 egui
            // 포인터가 갱신되지 않는다 — file_tree.rs의 os_drag_pointer_pos로 신뢰
            // 가능한 위치를 구하고, 실패(kittest 등)하면 egui 포인터로 폴백한다
            // (2026-08-14, OS 드롭 라우팅 수정 — 이전엔 터미널 위 OS 드롭을 받는 핸들러가
            // 아예 없었다). dock_rect(컴포저)·사이드바 패널과 이 pane_rect는 egui
            // Panel 레이아웃으로 서로 겹치지 않으므로, 같은 포인터 위치를 각자 자기
            // rect로만 판정해도 한 드롭이 두 곳에 들어가지 않는다.
            let os_drag_active = ui.input(|i| !i.raw.hovered_files.is_empty());
            let os_dropped: Vec<std::path::PathBuf> = ui.input(|i| {
                i.raw
                    .dropped_files
                    .iter()
                    .map(|file| file.path().to_path_buf())
                    .collect()
            });
            let os_drag_pos = (os_drag_active || !os_dropped.is_empty())
                .then(|| {
                    crate::ui::file_tree::os_drag_pointer_pos(ui.ctx())
                        .or_else(|| ui.input(|i| i.pointer.latest_pos()))
                })
                .flatten();
            let os_over_pane = os_drag_pos.is_some_and(|pos| pane_rect.contains(pos));

            drop_feedback = classify_terminal_drop_feedback(
                pane_resp
                    .dnd_hover_payload::<std::path::PathBuf>()
                    .is_some()
                    || pane_resp
                        .dnd_hover_payload::<crate::ui::file_tree::FileTreeDragPayload>()
                        .is_some(),
                pane_resp
                    .dnd_hover_payload::<TerminalTextDragPayload>()
                    .is_some(),
                os_drag_active && os_over_pane,
            );
            if let Some(session) = pane.session_id {
                if let Some(paths) = release_file_dnd_paths(&pane_resp) {
                    render_output.document_drop_paths.extend(paths);
                    render_output.local_focus_claimed = Some(pane_id.clone());
                    if !focused {
                        self.request_pane_focus(pane_id.clone());
                    }
                }
                if let Some(text) = release_typed_dnd_payload::<TerminalTextDragPayload>(&pane_resp)
                {
                    let bytes = terminal_text_paste_bytes(
                        &text.text,
                        self.session_bracketed_paste(session),
                    );
                    self.send(RuntimeCommand::WriteInput { session, bytes });
                }
                if !os_dropped.is_empty() && os_over_pane {
                    render_output.document_drop_paths.extend(os_dropped);
                    render_output.local_focus_claimed = Some(pane_id.clone());
                    if !focused {
                        self.request_pane_focus(pane_id.clone());
                    }
                }
            }
        }
        // pane 본문을 터미널 작업면 색으로 먼저 덮는다. 덮지 않으면 CentralPanel
        // 배경(앱 크롬)이 비쳐 터미널 둘레에 밝은 여백 띠로 보인다(2026-08-07 사용자).
        // 예전에는 크롬이 충분히 어두워 티가 안 났지만 크롬을 한 단 올리면서 드러났다.
        //
        // **content가 아니라 surface를 칠한다.** content는 surface에서 좌우 3px·상하 6px
        // 안쪽으로 들어간 rect라(TERMINAL_STREAM_*_PADDING), content만 칠하면 그 패딩 링이
        // 크롬 색으로 그대로 남는다. 거기에 더해 그리드 폭이 셀 단위로 떨어져 우측에
        // 최대 한 셀만큼 더 남는다 — surface를 칠하면 둘 다 덮인다.
        //
        // 렌더러가 아니라 여기서 칠하는 이유는 pane 폭을 아는 쪽이 호출부이기 때문이다 —
        // 렌더러에서 available 폭을 그대로 쓰면 무제한 ui에서 화면 전체를 차지한다.
        ui.painter()
            .rect_filled(pane_layout.surface, 0.0, renderer_egui::TERMINAL_SURFACE_BG);

        if let Some(notice_rect) = pane_layout.archived_notice
            && let Some(session) = pane.session_id
        {
            let presentation = self
                .archived_resume_presentation
                .get(&session)
                .copied()
                .unwrap_or(crate::agent_resume::ArchivedResumePresentation::Checking);
            let (message_key, button_key, action_enabled) = match presentation {
                crate::agent_resume::ArchivedResumePresentation::Exact => (
                    "workspace.exited.app_restart",
                    "workspace.exited.respawn_continue",
                    true,
                ),
                crate::agent_resume::ArchivedResumePresentation::RecentInCwd => (
                    "workspace.exited.resume_recent",
                    "workspace.exited.respawn_continue",
                    true,
                ),
                crate::agent_resume::ArchivedResumePresentation::Unsupported => (
                    "workspace.exited.resume_unsupported",
                    "workspace.exited.respawn_new",
                    true,
                ),
                crate::agent_resume::ArchivedResumePresentation::Unavailable => (
                    "workspace.exited.resume_unavailable",
                    "workspace.exited.respawn_new",
                    false,
                ),
                crate::agent_resume::ArchivedResumePresentation::Checking => (
                    "workspace.exited.resume_checking",
                    "status.detecting",
                    false,
                ),
            };
            let style = archived_agent_notice_style();
            ui.painter().rect_filled(notice_rect, 0.0, style.fill);
            ui.painter().hline(
                notice_rect.x_range(),
                notice_rect.top() + style.separator.width * 0.5,
                style.separator,
            );

            let content_rect = egui::Rect::from_min_max(
                egui::pos2(notice_rect.left() + 14.0, notice_rect.top() + 5.0),
                egui::pos2(notice_rect.right() - 12.0, notice_rect.bottom() - 5.0),
            );
            if content_rect.is_positive() {
                let mut notice_ui = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(content_rect)
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                );
                notice_ui.set_clip_rect(content_rect.intersect(ui.clip_rect()));
                notice_ui.spacing_mut().item_spacing.x = 12.0;
                let button =
                    egui::Button::new(egui::RichText::new(catalog.t(button_key, &[])).size(13.0))
                        .fill(style.button_fill)
                        .stroke(style.button_stroke)
                        .corner_radius(egui::CornerRadius::same(style.button_corner_radius))
                        .min_size(egui::vec2(0.0, style.button_height));
                // cross-workspace 첨부 뷰(Attached)는 이 pane의 session이 다른
                // 워크스페이스 런타임 소속이다. 그 pane을 소유한 로컬 뷰에서만
                // 기존과 동일하게 실행 버튼을 활성화한다.
                if notice_ui
                    .add_enabled(mode.is_local() && action_enabled, button)
                    .clicked()
                {
                    self.respawn_archived_request = Some(session);
                }
                let message_width = notice_ui.available_width().max(0.0);
                notice_ui.add_sized(
                    egui::vec2(message_width, style.button_height),
                    egui::Label::new(
                        egui::RichText::new(catalog.t(message_key, &[]))
                            .size(14.0)
                            .color(style.text),
                    )
                    .truncate()
                    .halign(egui::Align::LEFT),
                );
            }
        }
        let pane_feedback_painter =
            matches!(drop_feedback, Some(TerminalDropFeedback::DocumentOpen))
                .then(|| ui.painter().clone());
        let mut terminal_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(pane_layout.content)
                .layout(egui::Layout::top_down(egui::Align::LEFT)),
        );
        terminal_ui.set_clip_rect(pane_layout.content.intersect(ui.clip_rect()));
        terminal_ui.spacing_mut().item_spacing.y = 0.0;
        let ui = &mut terminal_ui;
        let Some(session) = pane.session_id else {
            ui.label(catalog.t("workspace.no_session", &[]));
            return render_output;
        };

        if let Some(id) = &pane.persistent_session_id {
            crate::ui::cloud_answer::contents(
                ui,
                &self.cloud_answers,
                id,
                &mut self.selected_cloud_answer,
                catalog,
            );
        }

        // 터미널 폰트는 UI 배율(zoom_factor)로 같이 커지므로 font_size를 배율로 역보정해
        // 물리 크기를 유지한다(UI만 스케일, 터미널 독립 — 2026-07-13). cell_size·draw가
        // 같은 값을 써야 격자/선택이 일치한다.
        let metrics = renderer_egui::CellMetrics {
            font_size: config.font_size / self.ui_scale,
            line_height: config.line_height,
        };
        let cell = renderer_egui::cell_size(ui.ctx(), metrics);
        let avail = ui.available_size();
        // 복원 화면이 도착하기 전에는 크기를 먼저 보내지 않는다. 화면이 준비되면
        // 이전 열 수에 고정하지 않고 단일·분할 pane 각각의 실제 가용 폭을 사용한다.
        let layout_ready = self.sent_sizes.contains_key(&session)
            || self
                .sessions
                .get(&session)
                .is_some_and(|view| view.snapshot.is_some());
        if layout_ready {
            let cols = renderer_egui::grid_cols_for_available(avail.x, cell.x);
            let rows = renderer_egui::grid_rows_for_available(avail.y + extra_height, cell.y);
            let rows =
                u32::from(rows).min(runtime::TERMINAL_CELL_COUNT_MAX / u32::from(cols)) as u16;
            // 기존 디바운스와 실제 적용 세대 확인을 거쳐 최종 크기만 표시한다.
            self.stage_terminal_resize_for_pass(
                ui.ctx().cumulative_pass_nr(),
                ui.is_sizing_pass(),
                session,
                cols,
                rows,
            );
        }

        if let Some(after) =
            self.settle_session_resize_presentation(session, std::time::Instant::now())
        {
            ui.ctx().request_repaint_after(after);
        }
        let selected = self.selection.is_some_and(|(s, _, _)| s == session);
        let (exit_code, bracketed, restored_readonly, snapshot) = {
            let view = self.sessions.entry(session).or_default();
            if let Some(after) = view.settle_initial_presentation(std::time::Instant::now()) {
                ui.ctx().request_repaint_after(after);
            }
            // 선택이 없으면(freeze 해제) freeze 중 보관한 최신본으로 catch-up한다 — 새
            // Viewport가 안 와도 화면이 선택 당시에 멈추지 않게(codex).
            if !selected && let Some(pending) = view.pending_snapshot.take() {
                view.install_snapshot(pending);
            }
            let Some(snapshot) = view.snapshot.clone() else {
                let message = match mode {
                    PaneRenderMode::Local { .. } => catalog.t("workspace.connecting", &[]),
                    PaneRenderMode::Attached {
                        workspace_label, ..
                    } => catalog.t(
                        "workspace.cross_pane.input_unavailable",
                        &[("workspace", workspace_label)],
                    ),
                };
                ui.label(message);
                return render_output;
            };
            (
                view.exit_code,
                view.bracketed_paste,
                view.restored_readonly,
                snapshot,
            )
        };

        if !layout_ready {
            // 위 initial gate/catch-up이 지금 첫 snapshot을 설치했다면 다음 프레임에
            // 행 크기를 예약한다. 새 출력이 없어도 최초 Resize가 누락되지 않게 한다.
            ui.ctx().request_repaint();
        }

        // 런타임의 focused pane과 현재 native UI의 논리적 키보드 소유 상태를 draw 전에
        // 확정한다. renderer가 이 값을 바탕으로 같은 프레임에 egui 공식 IME 소유권까지
        // 동기화하므로, 기존 egui owner를 “요청할지”의 선행 조건으로 쓰지 않는다.
        let terminal_refocus_pending =
            mode.is_local() && self.pending_focus.as_ref() == Some(pane_id);
        let search_input_focused = self.search.as_ref().is_some_and(|search| {
            search.session == session
                && ui.memory(|memory| {
                    memory.has_focus(ui.make_persistent_id(("terminal_search_input", session)))
                })
        });
        let terminal_input_owner = input_enabled
            && if mode.is_local() {
                terminal_input_owner(pane_id, focused, self.pending_focus.as_ref())
            } else {
                true
            };
        let any_blocking_window_visible = super::popup::background_input_blocked(ui.ctx());
        let terminal_keyboard_active = terminal_input_owner
            && !search_input_focused
            && terminal_keyboard_input_allowed(
                ui.ctx().text_edit_focused(),
                ui.ctx().any_popup_open(),
                any_blocking_window_visible,
                terminal_refocus_pending,
            );
        let text_edit_focused_before_draw = ui.ctx().text_edit_focused();
        // A pending terminal refocus may supersede stale TextEdit focus for
        // ordinary keys. IME events in that batch still belong to TextEdit.
        let text_edit_ime_batch = text_edit_focused_before_draw
            && ui.input(|input| {
                input
                    .raw
                    .events
                    .iter()
                    .any(|event| matches!(event, egui::Event::Ime(_)))
            });
        let terminal_ime_active = terminal_keyboard_active && !text_edit_ime_batch;
        let consumed_detached_commit = if terminal_ime_active && self.detached_ime_submit.is_some()
        {
            // Sending the old session's bytes may request a repaint. Release
            // egui's input read lock before resolving that queued Commit.
            let events = ui.input(|input| input.raw.events.clone());
            self.resolve_detached_ime_submit(&events, ui.ctx())
        } else {
            // A TextEdit or popup may own the keyboard for longer than the
            // fallback deadline. It must not strand the old pane's Enter.
            if !terminal_ime_active && self.detached_ime_submit.is_some() {
                self.resolve_detached_ime_submit(&[], ui.ctx());
            }
            None
        };
        // Both a still-queued submit and an already-flushed submit can own a
        // late AppKit Commit. Decide ownership before text reconciliation so
        // its paired Key/Text events cannot be attributed to the new pane.
        let consumed_flushed_commit = if terminal_ime_active && consumed_detached_commit.is_none() {
            let events = ui.input(|input| input.raw.events.clone());
            self.consume_flushed_ime_commit(&events)
        } else {
            None
        };
        let consumed_old_commit = consumed_detached_commit.or(consumed_flushed_commit);
        let consumed_old_key = consumed_old_commit
            .as_ref()
            .and_then(|(index, committed, at)| {
                let character = committed.chars().last().filter(char::is_ascii)?;
                if self
                    .native_printable_key_downs
                    .iter()
                    .any(|key| key.character == character && !key.observed_before(*at))
                {
                    return None;
                }
                ui.input(|input| {
                    [index.checked_sub(1), index.checked_add(1)]
                        .into_iter()
                        .flatten()
                        .find(|candidate| {
                            input
                                .raw
                                .events
                                .get(*candidate)
                                .and_then(input_mapper::ime_terminator_key_char)
                                == Some(character)
                        })
                })
            });
        let consumed_old_text_echo =
            consumed_old_commit
                .as_ref()
                .and_then(|(index, committed, _)| {
                    ui.input(|input| {
                        paired_old_ime_text_echo_index(
                            &input.raw.events,
                            *index,
                            committed,
                            cfg!(target_os = "macos"),
                        )
                    })
                });
        // A Commit normally follows Return in the same or next frame. If the
        // platform never delivers it, release queued input instead of losing
        // every subsequent key. A Commit already in this frame still wins.
        if terminal_ime_active
            && self.pending_ime_submit.as_ref().is_some_and(|deferred| {
                deferred.owner == session
                    && deferred.started.elapsed() >= std::time::Duration::from_secs(2)
            })
            && !ui.input(|input| {
                input
                    .raw
                    .events
                    .iter()
                    .any(|event| matches!(event, egui::Event::Ime(egui::ImeEvent::Commit(_))))
            })
        {
            // Commit never arrived. Preserve the last visible syllable as a
            // focus switch does, then release the queued Enter and later keys.
            self.flush_pending_ime_submit();
            self.preedit.clear();
        }
        let preedit_for_frame = (terminal_ime_active && !text_edit_focused_before_draw)
            .then(|| ui.input(|input| self.preedit.for_frame(session, &input.raw.events)));
        let preedit = preedit_for_frame
            .as_ref()
            .and_then(TerminalPreeditState::view);
        // 이 세션의 선택 영역 (정규화)
        let selection_range = self
            .selection
            .and_then(|(s, a, b)| (s == session).then_some((a.min(b), a.max(b))));
        let output = {
            let view = self.sessions.entry(session).or_default();
            renderer_egui::draw_with_preedit_in_viewport(
                ui,
                &snapshot,
                metrics,
                &mut view.render_cache,
                preedit,
                terminal_ime_active,
                selection_range,
                view.snapshot_gen,
                extra_height > 0.0,
            )
        };
        // B1 실측: 이 프레임에 그린 pane들의 렌더 비용을 합산한다 (visible pane 전부).
        self.frame_counters += output.counters;

        match drop_feedback {
            Some(TerminalDropFeedback::TerminalInsert) => {
                Self::paint_drop_insertion_marker(
                    ui,
                    output.origin,
                    output.cell_size,
                    snapshot.cursor.col,
                    snapshot.cursor.row,
                );
            }
            Some(TerminalDropFeedback::DocumentOpen) => {
                let painter = pane_feedback_painter
                    .expect("document drop feedback painter")
                    .with_clip_rect(pane_rect);
                let style = PaneDropFeedbackStyle {
                    label_fill: tokens.input_background,
                    label_text: tokens.text,
                    ..pane_drop_feedback_style(tokens)
                };
                painter.rect_filled(pane_rect, 0.0, tokens.accent.gamma_multiply(0.10));
                if let Some(label) = layout_pane_drop_feedback_label(
                    &painter,
                    pane_rect,
                    catalog.t("workspace.drop.open_document", &[]),
                    style,
                ) {
                    painter.rect_filled(label.rect, 4.0, style.label_fill);
                    painter.galley(
                        label.rect.min + egui::vec2(6.0, 3.0),
                        label.galley,
                        style.label_text,
                    );
                }
            }
            None => {}
        }

        // 현재 표시 중인 스냅샷을 기준으로 하므로 리사이즈·복원 뒤의 과거 위치도 잡는다.
        // 별도 행을 예약하지 않아 버튼 표시/숨김 때문에 PTY 크기가 다시 바뀌지 않는다.
        let scroll_bottom_button = if snapshot.scroll_offset > 0 && !snapshot.is_alt_screen {
            let mut bounds = pane_layout.content;
            if exit_code.is_some() && !restored_readonly {
                bounds.max.y -= 18.0; // 기존 종료 문구와 겹치지 않는다.
            }
            render_scroll_bottom_button(
                ui,
                bounds,
                pane_interaction_id(mode, "scroll_bottom", pane_id),
                catalog,
            )
        } else {
            None
        };
        // 버튼이 처음 나타난 프레임에도 그 아래 URL 클릭/텍스트 선택으로 새지 않게 한다.
        let over_scroll_bottom = scroll_bottom_button.as_ref().is_some_and(|button| {
            ui.input(|input| {
                input
                    .pointer
                    .latest_pos()
                    .is_some_and(|pos| button.rect.contains(pos))
            })
        });
        if scroll_bottom_button
            .as_ref()
            .is_some_and(egui::Response::clicked)
        {
            self.terminal_focus_claimed = true;
            render_output.focus_requested |= !input_enabled || !mode.is_local();
            if mode.is_local() {
                render_output.local_focus_claimed = Some(pane_id.clone());
                self.request_pane_focus(pane_id.clone());
            }
            self.scroll_session_to_bottom(session);
            if input_enabled {
                request_terminal_focus(&output.response);
            }
        }

        // 선택된 텍스트 위에서 시작한 드래그는 terminal-internal DnD payload가 된다.
        // 그 외의 마우스 드래그는 기존 셀 선택 동작을 유지한다.
        if input_enabled && !over_scroll_bottom && !egui::DragAndDrop::has_any_payload(ui.ctx()) {
            let cell_at = |pos: egui::Pos2| -> usize {
                let col = ((pos.x - output.origin.x) / output.cell_size.x)
                    .floor()
                    .clamp(0.0, snapshot.cols.saturating_sub(1) as f32)
                    as usize;
                let row = ((pos.y - output.origin.y) / output.cell_size.y)
                    .floor()
                    .clamp(0.0, snapshot.rows.saturating_sub(1) as f32)
                    as usize;
                row * snapshot.cols as usize + col
            };
            // 폴더/URL hover·단일 클릭 — 이동 가능한 폴더 단어 위에서는 커서를 손가락으로
            // 바꾸고 클릭하면 그 폴더로 cd, URL 위에서는 클릭하면 기본 브라우저로 연다
            // (2026-07-14/2026-07-17 사용자). 파일은 커서를 바꾸지 않는다(복사용 선택과
            // 혼동 방지 — 열기는 우클릭 메뉴). alt screen(TUI)은 cd 주입 금지라 통째로 비활성.
            if mode.is_local()
                && !snapshot.is_alt_screen
                && let Some(pos) = output.response.hover_pos()
                && let Some((s, e)) = word_range_at(&snapshot, cell_at(pos))
            {
                let word = renderer_egui::selection_text(&snapshot, s, e);
                // URL은 cwd 해석이 필요 없는 문자열 판정이라 폴더보다 먼저 본다.
                if let Some(url) = extract_url(&word) {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    if terminal_primary_pointer_clicked(&output.response) && surface_focused {
                        // 더블클릭이 clicked를 두 번 발화 — 같은 URL 연속 열기를 막는다
                        // (last_dir_click과 동일 관례).
                        let duplicate = self.last_url_click.as_ref().is_some_and(|(u, at)| {
                            u == url && at.elapsed() < std::time::Duration::from_millis(800)
                        });
                        if !duplicate {
                            self.last_url_click = Some((url.to_owned(), std::time::Instant::now()));
                            self.request_open_url(url);
                        }
                    }
                } else if matches!(
                    self.resolve_path_cached(session, &word),
                    Some(PathClick::Dir(_))
                ) {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    // 포커스된 pane에서만 cd — 비포커스 pane을 포커스하려는 클릭이
                    // cd까지 주입하면 안 된다 (codex 리뷰 MEDIUM). 첫 클릭은 포커스만,
                    // 포커스된 뒤의 클릭이 이동한다.
                    if terminal_primary_pointer_clicked(&output.response) && surface_focused {
                        // 실제 경로 판정은 App host가 완료한 immutable cache만 사용한다.
                        // cwd가 바뀌면 set_session_cwds가 pending/cache를 무효화한다.
                        if let Some(PathClick::Dir(path)) = self.resolve_path_cached(session, &word)
                        {
                            // 더블클릭은 clicked를 두 번 발화 — 같은 경로 연속 cd를 막는다.
                            let duplicate = self.last_dir_click.as_ref().is_some_and(|(p, at)| {
                                *p == path && at.elapsed() < std::time::Duration::from_millis(800)
                            });
                            if !duplicate {
                                self.last_dir_click =
                                    Some((path.clone(), std::time::Instant::now()));
                                // file tree의 "이 폴더로 이동"과 같은 헬퍼 — 셸별
                                // cd 문법/인용(PowerShell -LiteralPath, cmd /d)을 공유.
                                let bytes = cd_paste_bytes(
                                    &path,
                                    self.session_shell_kind(session),
                                    bracketed,
                                );
                                self.send(RuntimeCommand::WriteInput { session, bytes });
                                // cd로 셸 cwd가 바뀐다 — 방금 만든 해석 캐시도 무효.
                                self.invalidate_path_resolution();
                            }
                        }
                    }
                }
            }
            if (output.response.double_clicked() || output.response.triple_clicked())
                && let Some(pos) = output.response.interact_pointer_pos()
            {
                // 더블클릭 → 커서가 놓인 **행 전체** 선택 (2026-08-17 사용자 요청).
                // 예전에는 단어를 잡았는데, 터미널에서 복사하고 싶은 단위는 명령 한 줄이나
                // 출력 한 줄인 경우가 압도적이라 행으로 바꿨다.
                //
                // 트리플클릭도 **같은 행 선택**으로 받는다(멱등). egui의
                // `double_clicked()`는 count==2일 때만 참이라, 3연클릭은 아래 else-if
                // 사슬 끝의 `terminal_primary_pointer_clicked` 분기로 떨어져 **방금 만든
                // 선택을 지웠다**. 다른 터미널(iTerm2 등)이 트리플클릭=행 선택이라 이어
                // 클릭하는 사용자가 많다(2026-08-18 리뷰). 창 안에서 4번째 이상 연타해도
                // egui가 count를 3으로 유지하므로 선택이 계속 살아 있다.
                //
                // 선택 위에서 드래그하면 행 전체가 DnD 페이로드가 된다(다른 pane에 끌어다
                // 놓으면 그 세션 입력으로 들어간다). 그래서 더블클릭 직후 드래그로 **일부만
                // 다시 잡을 수는 없다** — 먼저 한 번 클릭해 선택을 지워야 한다. DnD를
                // 살리기로 한 사용자 결정이다(2026-08-18).
                //
                // URL 열기는 단일 클릭(위 hover/click 블록)이 담당한다 — 여기서도 열면
                // 이중 발화된다(2026-07-17). 파일 열기는 우클릭 메뉴, 폴더 진입은 단일 클릭.
                if let Some((s, e)) = line_range_at(&snapshot, cell_at(pos)) {
                    self.selection = Some((session, s, e));
                }
            } else if output.response.drag_started()
                // anchor(선택 시작 셀)는 "누른 지점"으로 잡는다. interact_pointer_pos()는
                // drag_started 발화 시점의 현재 포인터라 egui의 6px 드래그 임계값(≈1셀)만큼
                // 밀려 선택이 1칸 오른쪽에서 시작됐다(2026-07-23 사용자). press_origin이
                // 실제 누른 픽셀이다. head(끝점)는 아래 dragged 분기가 현재 포인터를 따른다.
                && let Some(pos) = ui
                    .input(|i| i.pointer.press_origin())
                    .or_else(|| output.response.interact_pointer_pos())
            {
                let idx = cell_at(pos);
                if let Some((start, end)) = selection_range
                    && selection_range_contains(start, end, idx)
                {
                    let text = renderer_egui::selection_text(&snapshot, start, end);
                    if pane_allows_terminal_dnd(mode) && !text.is_empty() {
                        output
                            .response
                            .dnd_set_drag_payload(TerminalTextDragPayload { text });
                    }
                } else {
                    self.selection = Some((session, idx, idx));
                    self.drag_autoscroll_residual = 0.0;
                }
            } else if output.response.dragged()
                && let Some(pos) = output.response.interact_pointer_pos()
                && let Some((s, anchor, _)) = self.selection
                && s == session
            {
                // 드래그 중 스크롤로 도착한 새 화면은 freeze로 pending에 보관돼 있다 —
                // 즉시 반영하고, 화면 좌표 기반 선택 앵커를 스크롤량만큼 이동시켜
                // 같은 텍스트를 계속 가리키게 한다. 끝점은 포인터(화면 위치)를 따른다.
                let mut anchor = anchor;
                {
                    let view = self.sessions.entry(session).or_default();
                    if view.pending_snapshot.as_ref().is_some_and(|p| {
                        (p.cols, p.rows) == (snapshot.cols, snapshot.rows)
                            && p.scroll_offset != snapshot.scroll_offset
                    }) && let Some(pending) = view.pending_snapshot.take()
                    {
                        // scroll_offset 증가(과거로) = 내용이 아래로 이동 → 앵커도 아래로
                        let delta_rows = pending.scroll_offset - snapshot.scroll_offset;
                        anchor = shift_selection_cell(
                            anchor,
                            delta_rows,
                            snapshot.cols as usize,
                            snapshot.rows as usize,
                        );
                        view.summary = last_line_summary(&pending);
                        view.snapshot = Some(pending);
                        view.snapshot_gen = view.snapshot_gen.wrapping_add(1);
                    }
                }
                self.selection = Some((session, anchor, cell_at(pos)));
                // 포인터가 pane 세로 경계를 벗어나면 초과 거리에 비례한 속도로
                // 오토스크롤한다 (iTerm2 관례 — T4). cell_at의 clamp가 끝점을
                // 마지막/첫 행(좌우는 열 경계)에 붙잡아 선택이 계속 확장된다.
                let rect = output.response.rect;
                let rate =
                    drag_autoscroll_rate(pos.y, rect.top(), rect.bottom(), output.cell_size.y);
                if rate != 0.0 {
                    // 속도(행/초)×dt 누적 → 정수 행만 전송. dt는 정지 프레임 폭주 대비 clamp.
                    let dt = ui.input(|i| i.stable_dt).min(0.1);
                    self.drag_autoscroll_residual += rate * dt;
                    let step = self.drag_autoscroll_residual.trunc() as i32;
                    if step != 0 {
                        self.drag_autoscroll_residual -= step as f32;
                        // send()가 아니라 선택 보존 경로 — send의 Scroll 선택 해제(휠 UX)와
                        // 충돌하면 첫 스크롤 직후 선택·오토스크롤이 함께 죽는다(A3 버그 #1).
                        self.send_keep_selection(RuntimeCommand::Scroll {
                            session,
                            delta: step,
                        });
                    }
                    // egui는 이벤트 드리븐 — 포인터가 안 움직여도 매 프레임 이어가도록
                    // 예약한다. 버튼 릴리즈 시 이 분기에 안 들어와 예약이 끊긴다(idle 0).
                    // 드래그 중에만 다음 프레임을 즉시 요청한다. 입력이 끝나면 예약이
                    // 남지 않아 idle periodic repaint가 생기지 않는다.
                    ui.ctx().request_repaint();
                } else {
                    self.drag_autoscroll_residual = 0.0;
                }
            } else if terminal_primary_pointer_clicked(&output.response) {
                self.selection = None; // 단순 클릭은 선택 해제 (더블클릭 아님)
            }
        }

        // 포커스 pane 파란 테두리는 사용자 요청으로 제거(2026-07-04) — 단일 pane 사용 시
        // 항상 보여 거슬림. 다중 pane에서 포커스 식별이 다시 필요해지면 "pane 2개 이상일
        // 때만 표시" 조건으로 복원할 것.
        // (egui 포커스는 클릭 시에만 요청한다 — 매 프레임 request_focus는 다른 창의 입력 포커스를 뺏는다.)
        // pending 포커스는 pane이 실제로 그려진 이 시점에 1회 소비한다 —
        // 매 프레임 요청은 다른 창 입력을 뺏고, "연결 중" 단계에서 소비하면
        // 요청이 유실된다 (리뷰 반영).
        if input_enabled
            && mode.is_local()
            && focused
            // Share the same ownership gate as preview, candidate output and
            // input admission. Keep automatic refocus pending for a TextEdit
            // IME batch; explicit pane clicks below still claim focus directly.
            && !text_edit_ime_batch
            && self.pending_focus.as_ref() == Some(pane_id)
        {
            let app_armed = self.explicit_pending_focus.as_ref() == Some(pane_id);
            self.pending_focus = None;
            self.explicit_pending_focus = None;
            self.explicit_pending_focus_observed = false;
            self.terminal_focus_claimed |= app_armed;
            // 검색을 클릭한 뒤 늦게 확정된 pane 포커스가 TextEdit 입력을 뺏지 않는다.
            if !search_input_focused {
                request_terminal_focus(&output.response);
            }
        }
        if !over_scroll_bottom && terminal_primary_pointer_clicked(&output.response) {
            // Input ownership may still belong to a sibling primary/attached surface in this
            // frame. Report the click independently from `input_enabled` so App can surrender
            // any deferred Agents TextEdit focus before the next key event.
            self.terminal_focus_claimed = true;
            if !input_enabled || !mode.is_local() {
                render_output.focus_requested = true;
            }
            if mode.is_local() {
                render_output.local_focus_claimed = Some(pane_id.clone());
                // Queue the exact primary pane even while a sibling attached surface owns input.
                // App switches the surface owner in this frame; FIFO FocusPane then selects the
                // clicked split before its first keyboard event.
                self.request_pane_focus(pane_id.clone());
            }
            if input_enabled {
                request_terminal_focus(&output.response);
            }
        }

        // 파일 트리에서 드래그한 경로를 터미널 위에 드롭 → 문서 intent로 전달.
        // hover 테두리는 위 pane 배경 경로가 pane_rect에 그린다.
        if mode.is_local()
            && input_enabled
            && let Some(paths) = release_file_dnd_paths(&output.response)
        {
            render_output.document_drop_paths.extend(paths);
            render_output.local_focus_claimed = Some(pane_id.clone());
            if !focused {
                self.request_pane_focus(pane_id.clone());
            }
        }
        if mode.is_local()
            && input_enabled
            && let Some(text) =
                release_typed_dnd_payload::<TerminalTextDragPayload>(&output.response)
        {
            let bytes = terminal_text_paste_bytes(&text.text, bracketed);
            self.send(RuntimeCommand::WriteInput { session, bytes });
            if mode.is_local() && !focused {
                self.request_pane_focus(pane_id.clone());
            }
        }
        // 터미널 위 우클릭도 같은 메뉴 (터미널 위젯이 topmost라 배경 interact가 못 받음)
        // 팝업 항목의 클릭 프레임은 PTY 입력 소유권이 일시 해제된다. 메뉴는 계속 처리한다.
        if mode.is_local() {
            self.pane_context_menu(&output.response, pane_id, config, catalog);
        } else {
            output.response.context_menu(|ui| {
                if let Some((selected, a, b)) = self.selection
                    && selected == session
                {
                    let text = renderer_egui::selection_text(&snapshot, a.min(b), a.max(b));
                    self.environment_selection_menu(ui, session, &text, catalog);
                }
                if ui
                    .button(catalog.t("workspace.open_environment", &[]))
                    .clicked()
                {
                    self.open_environment_requested =
                        Some(self.environment_open_request(Some(session), None));
                    ui.close();
                }
            });
        }

        // 터미널 텍스트 검색 (T3): 매치 하이라이트 + 우상단 검색 바 + 스크롤 이동.
        // input_enabled는 PTY 키 전송 소유권이다. 분할 클릭의 press/release 때 false여도
        // 검색창은 계속 등록해야 포커스·버튼 클릭과 Area 표시 수명이 유지된다.
        if mode.is_local()
            && self.render_terminal_search(
                ui,
                session,
                output.response.rect,
                output.origin,
                output.cell_size,
                &snapshot,
                catalog,
            )
        {
            self.terminal_focus_claimed = true;
            render_output.focus_requested |= !input_enabled;
            render_output.local_focus_claimed = Some(pane_id.clone());
            if !focused {
                self.request_pane_focus(pane_id.clone());
            }
        }

        // 검색 TextEdit/팝업 같은 overlay가 renderer 뒤에서 포커스를 가져갈 수도 있으므로
        // 이벤트를 소비하는 바로 이 시점에 egui의 공식 IME 소유권을 다시 확인한다.
        let terminal_owns_ime_events = ui
            .ctx()
            .memory(|memory| memory.owns_ime_events(output.response.id));
        let any_blocking_window_visible = super::popup::background_input_blocked(ui.ctx());
        let terminal_accepts_ime_events = terminal_accepts_ime_events(
            terminal_ime_active,
            terminal_owns_ime_events,
            self.preedit.is_active_for(session)
                || self
                    .pending_ime_submit
                    .as_ref()
                    .is_some_and(|deferred| deferred.for_session(session)),
            renderer_egui::frame_has_active_preedit(ui.ctx()),
            ui.ctx().text_edit_focused(),
            ui.ctx().any_popup_open(),
            any_blocking_window_visible,
        );
        let native_clipboard_copy_requested = if terminal_input_owner {
            std::mem::take(&mut self.native_clipboard_copy_requested)
        } else {
            false
        };
        let copy_selection = self
            .selection
            .filter(|(selection_session, _, _)| *selection_session == session);
        let should_copy_selection = ui.input(|input| {
            terminal_should_copy_selection(
                native_clipboard_copy_requested,
                &input.raw.events,
                terminal_keyboard_active,
                self.copy_suppressed,
                copy_selection.is_some(),
            )
        });
        if should_copy_selection && let Some((_, start, end)) = copy_selection {
            ui.ctx().copy_text(renderer_egui::selection_text(
                &snapshot,
                start.min(end),
                start.max(end),
            ));
        }
        if terminal_accepts_ime_events {
            let mut pending: Vec<u8> = Vec::new();
            // macOS/winit은 한 번의 IME 종료 키를 `Ime::Commit`과 일반 `Text` 양쪽으로
            // 전달하거나, 반대로 `Text`를 생략할 수 있다. AppKit/egui에서 관찰한 실제
            // printable key-down 수를 한도 삼아 두 텍스트 경로와 fallback을 한 번에
            // 조정해야 공백·쉼표의 중복과 첫 문장부호 누락을 동시에 막을 수 있다.
            let preedit_active_before_input = self.preedit.is_active_for(session);
            let mut preedit_at_submit = if preedit_active_before_input {
                self.preedit.text.clone()
            } else {
                String::new()
            };
            let mut native_key_downs = std::mem::take(&mut self.native_printable_key_downs);
            if let Some((_, committed, submitted_at)) = &consumed_old_commit {
                // Those AppKit physical terminators belong to the old Commit,
                // not to this pane's fallback reconciliation.
                let mut old_ascii: Vec<char> = committed.chars().filter(char::is_ascii).collect();
                native_key_downs.retain(|key| {
                    if key.observed_before(*submitted_at)
                        && let Some(index) = old_ascii.iter().position(|c| *c == key.character)
                    {
                        old_ascii.remove(index);
                        false
                    } else {
                        true
                    }
                });
            }
            let native_clipboard_paste_requested =
                std::mem::take(&mut self.native_clipboard_paste_requested);
            let ime_reconciliation = ui.input(|input| {
                let mut filtered_events = consumed_old_commit.as_ref().map(|(index, _, _)| {
                    let mut events = input.raw.events.clone();
                    events[*index] = egui::Event::Copy;
                    if let Some(echo) = consumed_old_text_echo {
                        events[echo] = egui::Event::Copy;
                    }
                    if let Some(key) = consumed_old_key {
                        events[key] = egui::Event::Copy;
                    }
                    events
                });
                reconcile_ime_text_events(
                    &native_key_downs,
                    filtered_events
                        .as_mut()
                        .map_or(input.raw.events.as_slice(), |events| events.as_slice()),
                    preedit_active_before_input || consumed_old_commit.is_some(),
                    self.pending_ime_submit
                        .as_ref()
                        .is_some_and(|deferred| deferred.for_session(session)),
                )
            });
            if let Some(preedit) = preedit_for_frame {
                self.preedit = preedit;
            }
            let mut deferred_submit = self
                .pending_ime_submit
                .take()
                .filter(|deferred| deferred.for_session(session));
            let had_deferred_before_frame = deferred_submit.is_some();
            let previous_deferred_bytes = deferred_submit
                .as_ref()
                .map_or(0, |deferred| deferred.after_submit.len());
            let mut image_paste_trigger = (native_clipboard_paste_requested
                && !self.paste_suppressed)
                .then_some(ClipboardPasteTrigger::NativeKeyDown);
            let mut text_paste_bytes: Option<Vec<u8>> = None;
            let mut composition_active = preedit_active_before_input || had_deferred_before_frame;
            let mut deferred_fallback_insert_at = None;
            let mut following_fallback_insert_at = None;
            ui.input(|input| {
                let modifiers = input.modifiers;
                for (event_index, event) in input.raw.events.iter().enumerate() {
                    if consumed_old_commit
                        .as_ref()
                        .is_some_and(|(index, _, _)| *index == event_index)
                        || consumed_old_text_echo == Some(event_index)
                        || consumed_old_key == Some(event_index)
                    {
                        continue;
                    }
                    if let egui::Event::Ime(egui::ImeEvent::Preedit { text, .. }) = event {
                        // Empty preedit often precedes Commit; the composition is still
                        // active until that Commit has reached the PTY byte stream.
                        composition_active |= !text.is_empty();
                        if !text.is_empty() {
                            preedit_at_submit.clone_from(text);
                            self.flushed_ime_commit = None;
                        }
                        continue;
                    }
                    if let Some(text) = ime_reconciliation.event_text[event_index].as_ref() {
                        if matches!(event, egui::Event::Ime(egui::ImeEvent::Commit(_))) {
                            pending.extend(text.as_bytes());
                            if let Some(mut deferred) = deferred_submit.take() {
                                deferred_fallback_insert_at = Some(pending.len());
                                following_fallback_insert_at = Some(
                                    pending.len()
                                        + deferred.before_submit.len()
                                        + deferred.independent_before_submit.len()
                                        + if had_deferred_before_frame {
                                            previous_deferred_bytes
                                        } else {
                                            1
                                        },
                                );
                                pending.append(&mut deferred.before_submit);
                                pending.append(&mut deferred.independent_before_submit);
                                pending.append(&mut deferred.after_submit);
                            }
                            composition_active = false;
                        } else {
                            append_ordered_terminal_bytes(
                                &mut pending,
                                &mut deferred_submit,
                                text.as_bytes(),
                            );
                        }
                        continue;
                    }
                    if matches!(event, egui::Event::Paste(_)) {
                        // 파일 트리가 이번 ⌘V를 소비 — 같은 제스처의 텍스트 붙여넣기 스킵.
                        if self.paste_suppressed {
                            continue;
                        }
                        text_paste_bytes = input_mapper::map_event(event, bracketed, &modifiers);
                        continue;
                    }
                    // Cmd+C(macOS)/Ctrl+C(그 외)의 Copy 이벤트: 선택이 있으면 복사가
                    // 우선 — 이벤트를 소비해 ^C 전송(비macOS 매핑)을 막는다 (2026-07-05)
                    if matches!(event, egui::Event::Copy) && copy_selection.is_some() {
                        // 선택이 있으면 Copy는 PTY 제어 바이트가 아니다. 실제 clipboard
                        // 쓰기는 네이티브/지연 이벤트를 합친 위 단일 경로에서 수행한다.
                        continue;
                    }
                    if is_clipboard_paste_shortcut(event) {
                        if !self.paste_suppressed {
                            image_paste_trigger.get_or_insert(ClipboardPasteTrigger::EguiShortcut);
                        }
                        continue;
                    }
                    // macOS ⌘ 조합 키는 앱 단축키 영역 — PTY로 보내지 않는다. 전역 단축키
                    // 소비(take_triggered_action)는 input.events에서만 지워지는데 이 루프는
                    // input.raw.events(별도 clone)를 읽으므로, 여기서 막지 않으면 ⌘↓ 등이
                    // 화살표 CSI로 새어 들어간다(기존 ⌘⌥←/→ 워크스페이스 전환도 동일 누수
                    // — 2026-07-17). mac_cmd만 본다: 비macOS의 command(=Ctrl)는 Ctrl+C 등
                    // PTY 몫이라 건드리지 않는다.
                    if let egui::Event::Key {
                        modifiers: key_modifiers,
                        ..
                    } = event
                        && key_modifiers.mac_cmd
                    {
                        continue;
                    }
                    // Shift+화살표 → 마우스 드래그처럼 선택 확장. 터미널로는 안 보낸다.
                    // alt-screen(vim/less 등 TUI)에선 앱이 shift+화살표를 쓰므로 가로채지
                    // 않고 그대로 통과시킨다. command 조합(비macOS에선 Ctrl+Shift+화살표 —
                    // 프롬프트 점프 ⌘⇧↑/↓의 기본 chord)은 앱 단축키 몫이라 선택 확장으로
                    // 겹쳐 실행하지 않는다 (macOS는 위 mac_cmd 가드가 이미 걸렀다).
                    if !snapshot.is_alt_screen
                        && let egui::Event::Key {
                            key,
                            pressed: true,
                            modifiers: m,
                            ..
                        } = event
                        && m.shift
                        && !m.command
                        && matches!(
                            key,
                            egui::Key::ArrowLeft
                                | egui::Key::ArrowRight
                                | egui::Key::ArrowUp
                                | egui::Key::ArrowDown
                        )
                    {
                        let cols = snapshot.cols as usize;
                        let max_idx = (cols * snapshot.rows as usize).saturating_sub(1);
                        // 앵커: 기존 선택이 있으면 유지, 없으면 커서 위치에서 시작.
                        let (anchor, end) = match self.selection {
                            Some((s, a, e)) if s == session => (a, e),
                            _ => {
                                let cur = snapshot.cursor.row as usize * cols
                                    + snapshot.cursor.col as usize;
                                (cur, cur)
                            }
                        };
                        let new_end = match key {
                            egui::Key::ArrowRight => (end + 1).min(max_idx),
                            egui::Key::ArrowLeft => end.saturating_sub(1),
                            egui::Key::ArrowDown => (end + cols).min(max_idx),
                            egui::Key::ArrowUp => end.saturating_sub(cols),
                            _ => end,
                        };
                        self.selection = Some((session, anchor, new_end));
                        continue;
                    }
                    // Enter can reach egui before the IME Commit, even in another frame.
                    // Keep it and any following keys in session-owned order until Commit.
                    if let egui::Event::Key {
                        key: egui::Key::Enter,
                        pressed: true,
                        modifiers: m,
                        ..
                    } = event
                    {
                        let submit = if m.shift { b'\n' } else { b'\r' };
                        if composition_active && !m.ctrl && !m.alt {
                            let deferred =
                                deferred_submit.get_or_insert_with(|| PendingImeSubmit {
                                    owner: session,
                                    started: std::time::Instant::now(),
                                    preedit_at_submit: preedit_at_submit.clone(),
                                    before_submit: Vec::new(),
                                    independent_before_submit: Vec::new(),
                                    after_submit: Vec::new(),
                                });
                            deferred.after_submit.push(submit);
                            continue;
                        }
                        // Shift+Enter → 줄바꿈(LF); ordinary Enter remains CR.
                        if m.shift {
                            pending.push(submit);
                            continue;
                        }
                    }
                    if let Some(bytes) = input_mapper::map_event(event, bracketed, &modifiers) {
                        append_ordered_terminal_bytes(&mut pending, &mut deferred_submit, &bytes);
                    }
                }
            });
            settle_ime_fallback(
                &mut pending,
                &mut deferred_submit,
                had_deferred_before_frame,
                deferred_fallback_insert_at,
                previous_deferred_bytes,
                following_fallback_insert_at,
                &ime_reconciliation,
            );
            if let Some(paste_trigger) = image_paste_trigger {
                if should_skip_paste_task(
                    paste_trigger,
                    text_paste_bytes.is_some(),
                    self.last_text_paste,
                    self.last_native_paste,
                ) {
                    // 같은 ⌘V 제스처의 native key-down 또는 Event::Paste에서 이미 처리했다.
                    // 뒤늦은 key-up fallback까지 돌리면 이미지/텍스트가 한 번 더 붙는다.
                    self.last_text_paste = None;
                    self.last_native_paste = None;
                } else {
                    // 파일/이미지 판별 + PNG 인코딩은 App host가 수행한다. render는
                    // operation/generation intent만 남긴다.
                    if paste_trigger == ClipboardPasteTrigger::NativeKeyDown {
                        self.last_native_paste = Some(std::time::Instant::now());
                    }
                    let text_fallback = text_paste_bytes.take();
                    self.request_terminal_clipboard(
                        session,
                        bracketed,
                        self.session_shell_kind(session),
                        text_fallback,
                    );
                }
            } else if let Some(bytes) = text_paste_bytes {
                self.last_text_paste = Some(std::time::Instant::now());
                append_ordered_terminal_bytes(&mut pending, &mut deferred_submit, &bytes);
            }
            if deferred_submit.as_ref().is_some_and(|deferred| {
                deferred.before_submit.len()
                    + deferred.independent_before_submit.len()
                    + deferred.after_submit.len()
                    > 2 * 1024 * 1024
            }) && let Some(mut deferred) = deferred_submit.take()
            {
                tracing::warn!("IME 제출 대기 입력이 2 MiB를 넘어 PTY에 전달됨");
                pending.append(&mut deferred.before_submit);
                pending.append(&mut deferred.independent_before_submit);
                pending.append(&mut deferred.after_submit);
            }
            self.pending_ime_submit = deferred_submit;
            if let Some(deferred) = &self.pending_ime_submit {
                ui.ctx().request_repaint_after(
                    std::time::Duration::from_secs(2).saturating_sub(deferred.started.elapsed()),
                );
            }
            if !pending.is_empty() {
                // 선택 해제는 send()가 WriteInput 공통 지점에서 처리한다.
                self.send(RuntimeCommand::WriteInput {
                    session,
                    bytes: pending,
                });
            }
        }

        // 마우스 휠 → 스크롤백 (focused pane만 — 비활성 pane은 Viewport가
        // push되지 않아(14.4) 스크롤해도 화면이 안 바뀐다)
        if input_enabled && surface_focused && !over_scroll_bottom && output.response.hovered() {
            let scroll_y = ui.input(|i| i.smooth_scroll_delta.y);
            self.scroll_residual += scroll_y / output.cell_size.y;
            let whole_rows = self.scroll_residual.trunc() as i32;
            if whole_rows != 0 {
                self.scroll_residual -= whole_rows as f32;
                // 드래그 중이면 선택을 보존한 채 스크롤한다 — 한 화면을 넘는 범위를 휠로
                // 이어 잡을 수 있어야 한다(`wheel_scroll_keeps_selection` 참고). 선택
                // 앵커는 아래 `dragged()` 분기가 스냅샷 도착 시 `shift_selection_cell`로
                // 보정하므로, 여기서는 해제만 피하면 된다.
                let keep = wheel_scroll_keeps_selection(
                    output.response.dragged(),
                    self.selection.is_some_and(|(s, _, _)| s == session),
                );
                let command = RuntimeCommand::Scroll {
                    session,
                    delta: whole_rows,
                };
                if keep {
                    self.send_keep_selection(command);
                } else {
                    self.send(command);
                }
            }
        }

        if let Some(code) = exit_code
            && !restored_readonly
        {
            let code = code
                .map(|c| c.to_string())
                .unwrap_or_else(|| catalog.t("workspace.exit_unknown", &[]));
            // renderer가 pane 최하단까지 쓰므로 상태를 새 행으로 배치하지 않고 overlay한다.
            // 종료 표시는 유지하면서 하단에 다시 한 행짜리 빈 띠가 생기는 회귀를 막는다.
            ui.painter().text(
                output.response.rect.left_bottom() + egui::vec2(6.0, -4.0),
                egui::Align2::LEFT_BOTTOM,
                catalog.t("workspace.exited", &[("code", code.as_str())]),
                egui::FontId::proportional(12.0),
                ui.visuals().weak_text_color(),
            );
        }

        // pane 전체 강조 플래시 — 포커스 이동(1초)·입력요청·작업완료(2초) 페이드(2026-07-12 사용자).
        if let Some(session) = pane.session_id
            && let Some(&(until, duration)) = self.session_flash.get(&session)
        {
            let now = std::time::Instant::now();
            if now < until {
                let remain = (until - now).as_secs_f32() / duration.as_secs_f32();
                let color = pane_flash_color(self.workspace_accent, remain);
                // 이 시점 `ui`는 pane 안쪽 여백(content)으로 클립된 terminal 자식 UI다.
                // painter().with_clip_rect는 기존 clip과 **교집합**이라(content∩surface=
                // content) 테두리 4변이 여전히 잘려 안 보였다(2026-07-23 사용자). layer_painter
                // 는 clip이 화면 전체라 pane 전체(surface)로 새로 clip해 테두리가 보인다.
                // 같은 layer라 그리드 뒤에 그려져 z-order상 맨 위다.
                // 상/하/좌는 경계 중앙(Middle), **가장 우측 세로줄만 안쪽**으로 그린다
                // (2026-07-23 사용자). rect_stroke는 4변을 같은 방식으로만 그려 per-edge가
                // 안 되므로 4개 선으로 나눠 그린다. Middle 변의 바깥 절반이 잘리지 않게
                // clip을 1px 넓힌다. 우측선은 right-1에 그려 2px가 pane 안에 들어온다.
                let stroke = egui::Stroke::new(2.0, color);
                let painter = ui
                    .ctx()
                    .layer_painter(ui.layer_id())
                    .with_clip_rect(pane_rect.expand(1.0));
                painter.hline(pane_rect.x_range(), pane_rect.top(), stroke);
                painter.hline(pane_rect.x_range(), pane_rect.bottom(), stroke);
                painter.vline(pane_rect.left(), pane_rect.y_range(), stroke);
                painter.vline(pane_rect.right() - 1.0, pane_rect.y_range(), stroke);
                ui.ctx().request_repaint(); // 페이드 애니메이션
            }
        }
        render_output
    }

    /// 실행 중 에이전트 pane 대상 목록 — 감지 워커가 채운 agent_info의 세션들.
    /// 표시 순서를 프레임마다 흔들지 않게 mux pane 순서로 정렬한다.
    fn agent_send_targets(
        &self,
    ) -> Vec<(
        SessionId,
        crate::agent_detect::AgentExecutionIdentity,
        String,
    )> {
        self.mux
            .iter()
            .flat_map(|mux| mux.tabs.iter().flat_map(|tab| &tab.panes))
            .filter_map(|pane| {
                let session = pane.session_id?;
                let execution = *self.agent_executions.get(&session)?;
                let line = self.agent_line_for(session)?;
                Some((session, execution, line))
            })
            .collect()
    }

    /// 대상×프리셋 서브메뉴를 그리고 선택을 돌려준다 — 선택 텍스트의
    /// 「에이전트로 보내기」가 사용한다.
    /// 반환: (보낼 세션들, 프리셋 — None이면 그대로 보내기).
    fn draw_agent_send_menu(
        &self,
        ui: &mut egui::Ui,
        title: String,
        targets: &[(
            SessionId,
            crate::agent_detect::AgentExecutionIdentity,
            String,
        )],
        catalog: &i18n::Catalog,
    ) -> Option<AgentSendChoice> {
        let mut choice: Option<AgentSendChoice> = None;
        ui.menu_button(title, |ui| {
            for (session, execution, agent_line) in targets {
                ui.label(egui::RichText::new(agent_line).small().weak());
                if ui
                    .button(catalog.t("workspace.menu.send_agent.raw", &[]))
                    .clicked()
                {
                    choice = Some((vec![(*session, *execution)], None));
                    ui.close();
                }
                // 빈 항목은 건너뛴다 — 설정에서 "추가"만 누르고 안 채운 경우 메뉴에
                // 빈 줄이 생긴다(2026-07-17 설정 UI 도입).
                for preset in self
                    .agent_send_presets
                    .iter()
                    .filter(|p| !p.trim().is_empty())
                {
                    if ui.button(format!("\"{preset}\"")).clicked() {
                        choice = Some((vec![(*session, *execution)], Some(preset.clone())));
                        ui.close();
                    }
                }
                ui.separator();
            }
            // 대상이 둘 이상일 때만 — 하나뿐이면 개별 전송과 같아 의미가 없다.
            if targets.len() > 1
                && ui
                    .button(catalog.t(
                        "workspace.menu.send_agent.all",
                        &[("count", &targets.len().to_string())],
                    ))
                    .clicked()
            {
                choice = Some((
                    targets
                        .iter()
                        .map(|(session, execution, _)| (*session, *execution))
                        .collect(),
                    None,
                ));
                ui.close();
            }
        });
        choice
    }

    /// 선택 본문을 대상 에이전트들에 주입하고 단일 대상이면 pane 포커스까지 옮긴다.
    fn dispatch_agent_prompt(&mut self, mut send_to: Vec<AgentPasteTarget>, body: &str) {
        send_to
            .retain(|(session, execution)| self.agent_executions.get(session) == Some(execution));
        if body.trim().is_empty() {
            return;
        }
        // At most 32 awaiting host dispatches and 1 MiB total prompt bytes, before retention.
        let retained = self
            .selected_agent_prompts
            .iter()
            .map(|intent| intent.prompt.len())
            .sum::<usize>();
        if self
            .selected_agent_prompts
            .len()
            .saturating_add(send_to.len())
            > 32
            || body
                .len()
                .checked_mul(send_to.len())
                .and_then(|bytes| bytes.checked_add(retained))
                .is_none_or(|bytes| bytes > 1024 * 1024)
        {
            self.report_protocol_queue_rejection(
                WorkspaceProtocolErrorCode::PayloadTooLarge,
                "selected_agent_prompt",
            );
            return;
        }
        let prompt: std::sync::Arc<str> = body.into();
        for &(session, execution) in &send_to {
            self.selected_agent_prompts.push_back(SelectedAgentPrompt {
                session,
                execution,
                prompt: prompt.clone(),
            });
        }
        // No Enter is generated. Exact host admission owns the shared paste encoder and receipt.
        if let Some((session, _)) = send_to.first().filter(|_| send_to.len() == 1)
            && let Some(pane) = self.pane_of_session(*session)
        {
            self.request_pane_focus(pane);
        }
    }

    /// 「에이전트로 보내기 ▸」 서브메뉴 — 실행 중 에이전트 pane마다 (그대로 보내기 +
    /// 프리셋들), 2개 이상이면 「모든 에이전트에게 (N)」까지. 대상이 없으면 아무것도
    /// 그리지 않는다(등록만 되고 실행 중이 아닌 에이전트는 대상이 아니다 — 2026-07-17).
    /// 그리기/선택은 draw_agent_send_menu, 주입/포커스는 dispatch_agent_prompt로 분리한다.
    fn send_to_agent_menu(&mut self, ui: &mut egui::Ui, selection: &str, catalog: &i18n::Catalog) {
        let targets = self.agent_send_targets();
        if targets.is_empty() {
            return;
        }
        let Some((send_to, preset)) = self.draw_agent_send_menu(
            ui,
            catalog.t("workspace.menu.send_agent", &[]),
            &targets,
            catalog,
        ) else {
            return;
        };
        let body = match &preset {
            None => selection.to_owned(),
            Some(preset) => format!("{preset}:\n{selection}"),
        };
        self.dispatch_agent_prompt(send_to, &body);
    }

    /// 「마지막 출력 복사」 — 드래그 선택 없이 OSC 133 C~D 범위를 워커에서 추출한다.
    /// 응답(LastOutputExtracted)은 handle_events가 clipboard copy로 예약한다.
    fn last_output_menu_items(
        &mut self,
        ui: &mut egui::Ui,
        session: SessionId,
        catalog: &i18n::Catalog,
    ) {
        if ui
            .button(catalog.t("workspace.menu.copy_last_output", &[]))
            .clicked()
        {
            self.last_output_copy_pending.insert(session);
            self.send(RuntimeCommand::ExtractLastOutput { session });
            ui.close();
        }
    }

    /// 세션이 붙어 있는 pane id (mux 스냅샷 조회).
    fn pane_of_session(&self, session: SessionId) -> Option<runtime::MuxPaneId> {
        self.mux
            .as_ref()?
            .tabs
            .iter()
            .flat_map(|tab| &tab.panes)
            .find_map(|pane| (pane.session_id == Some(session)).then(|| pane.id.clone()))
    }

    /// session이 지금 어느 pane에 붙어 있는지 — 워크트리 삭제 후 같은 cwd를 쓰던
    /// 형제 pane을 모두 찾을 때 쓴다(2026-07-18).
    pub fn pane_for_session(&self, session: runtime::SessionId) -> Option<runtime::MuxPaneId> {
        self.mux.as_ref().and_then(|mux| {
            mux.tabs
                .iter()
                .flat_map(|tab| &tab.panes)
                .find(|p| p.session_id == Some(session))
                .map(|p| p.id.clone())
        })
    }

    /// pane을 확인 없이 즉시 닫는다 — cwd가 이미 사라져 세션을 보존할 이유가 없을 때만
    /// 쓴다(워크트리 삭제 완료 후, 2026-07-18). `request_close_pane`과 달리 실행 중
    /// 세션이어도 확인창을 띄우지 않는다 — 지운 뒤라 "닫을지 말지"가 아니라 이미
    /// 죽은 셸을 치우는 것뿐이라 확인이 의미 없다.
    pub fn close_pane_now(&mut self, pane: runtime::MuxPaneId) {
        self.send(RuntimeCommand::ClosePane { pane });
    }

    /// pane 닫기 요청 — 실행 중 세션이면 확인을 거치고, 아니면 즉시 닫는다.
    /// (세션 상태를 모르면 보수적으로 확인을 띄운다 — 실수 즉사 방지가 목적.)
    /// 사이드바 컨텍스트 메뉴(App 경유)도 같은 경로를 쓴다.
    pub fn request_close_pane(&mut self, pane: runtime::MuxPaneId) {
        let running = self
            .mux
            .as_ref()
            .and_then(|mux| {
                mux.tabs
                    .iter()
                    .flat_map(|tab| &tab.panes)
                    .find(|p| p.id == pane)
            })
            .and_then(|p| p.session_id)
            .map(|s| {
                self.sessions
                    .get(&s)
                    .is_none_or(|view| view.exit_code.is_none())
            })
            .unwrap_or(false);
        if running {
            self.confirm_close = Some(pane);
            // 다이얼로그는 다음 프레임에 그려진다(show 초입 호출 순서) — 리페인트를
            // 예약해 × 클릭 직후 확인창이 바로 뜨게 한다 (codex).
            self.command_sent = true;
        } else {
            self.send(RuntimeCommand::ClosePane { pane });
        }
    }

    /// Publish pending ownership before App's global shortcuts/render dispatch.
    pub(crate) fn has_pending_close_confirmation(&self) -> bool {
        self.confirm_close.as_ref().is_some_and(|pane| {
            self.mux.as_ref().is_some_and(|mux| {
                mux.tabs
                    .iter()
                    .flat_map(|tab| &tab.panes)
                    .any(|item| &item.id == pane)
            })
        })
    }

    /// 닫기 확인 다이얼로그 (request_close_pane이 세팅) — 실행 중 세션 종료 경고.
    pub(crate) fn close_confirm_dialog(&mut self, ctx: &egui::Context, catalog: &i18n::Catalog) {
        let Some(pane) = self.confirm_close.clone() else {
            return;
        };
        // 대상 pane이 그 사이 사라졌으면(셸 exit 등) 조용히 정리
        let alive = self
            .mux
            .as_ref()
            .is_some_and(|m| m.tabs.iter().flat_map(|t| &t.panes).any(|p| p.id == pane));
        if !alive {
            self.confirm_close = None;
            return;
        }
        let target = self
            .mux
            .as_ref()
            .and_then(|mux| {
                mux.tabs
                    .iter()
                    .flat_map(|tab| &tab.panes)
                    .find(|p| p.id == pane)
            })
            .map(|p| self.resolve_session_title(&p.title, p.session_id, None, catalog))
            .unwrap_or_default();
        let choice = super::session_close_dialogs::session(ctx, &pane.0, &target, catalog);
        if let Some(choice) = choice {
            if choice == super::popup::ConfirmationChoice::Confirm {
                self.send(RuntimeCommand::ClosePane { pane });
            }
            self.confirm_close = None;
        }
    }

    /// pane 우클릭 메뉴 — 분할/닫기 (2026-07-05, 선택한 pane 단위 제어).
    /// 드롭 삽입 마커 — 떨어뜨린 텍스트가 들어갈 커서 자리를 "열린 슬롯"으로 보여준다.
    ///
    /// pane 외곽선 대신 쓰는 이유: 외곽선은 "이 pane이 받는다"까지만 말하고 **어디로
    /// 들어가는지**는 말하지 않는다. 실제 삽입 지점은 셸 입력줄의 커서다.
    ///
    /// 터미널은 고정 셀 격자라 Finder처럼 진짜로 행을 벌릴 수 없다(셸이 렌더링을 소유해서
    /// 우리가 밀면 그 아래가 전부 어긋난다). 그래서 커서 셀 위에 슬롯을 겹쳐 그려 같은
    /// 신호를 위치로 전달한다 — 왼쪽 세로 막대가 삽입선, 그 오른쪽 옅은 면이 들어갈 자리다.
    fn paint_drop_insertion_marker(
        ui: &egui::Ui,
        origin: egui::Pos2,
        cell: egui::Vec2,
        cursor_col: u16,
        cursor_row: u16,
    ) {
        let accent = ui.visuals().selection.bg_fill;
        let slot = egui::Rect::from_min_size(
            origin
                + egui::vec2(
                    f32::from(cursor_col) * cell.x,
                    f32::from(cursor_row) * cell.y,
                ),
            cell,
        );
        // 들어갈 자리 — 옅게 채워 "빈 칸이 열렸다"를 보여준다. 글자를 덮지 않을 만큼 옅게.
        ui.painter()
            .rect_filled(slot, 1.0, accent.gamma_multiply(0.30));
        // 삽입선 — Finder의 삽입 캐럿과 같은 역할. 셀 높이보다 살짝 키워 격자에 묻히지 않게.
        let bar = egui::Rect::from_min_max(
            egui::pos2(slot.left() - 1.0, slot.top() - 1.0),
            egui::pos2(slot.left() + 1.0, slot.bottom() + 1.0),
        );
        ui.painter().rect_filled(bar, 1.0, accent);
    }

    /// 세션 번호는 runtime마다 재사용하므로 원본 pane의 캐시에서 경로도 함께 캡처한다.
    fn environment_open_request(
        &self,
        session: Option<SessionId>,
        prefill: Option<super::environment::EnvironmentPrefill>,
    ) -> super::environment::EnvironmentOpenRequest {
        super::environment::EnvironmentOpenRequest {
            session,
            cwd: session.and_then(|session| self.session_cwds.get(&session).cloned()),
            prefill,
        }
    }

    fn environment_selection_menu(
        &mut self,
        ui: &mut egui::Ui,
        session: SessionId,
        text: &str,
        catalog: &i18n::Catalog,
    ) {
        use super::environment::{EnvironmentPrefill, EnvironmentSelectionKind as K};
        ui.menu_button(catalog.t("workspace.menu.add_to_environment", &[]), |ui| {
            for (kind, key) in [
                (K::ApiName, "workspace.menu.use_as_api_name"),
                (K::ApiValue, "workspace.menu.use_as_api_value"),
                (K::VariableName, "workspace.menu.use_as_env_name"),
                (K::VariableValue, "workspace.menu.use_as_env_value"),
            ] {
                if ui
                    .add_enabled(kind.accepts(text), egui::Button::new(catalog.t(key, &[])))
                    .clicked()
                {
                    self.open_environment_requested = Some(self.environment_open_request(
                        Some(session),
                        EnvironmentPrefill::from_selection(kind, text),
                    ));
                    ui.close();
                }
            }
        });
    }

    fn pane_context_menu(
        &mut self,
        resp: &egui::Response,
        pane_id: &runtime::MuxPaneId,
        config: &TerminalConfig,
        catalog: &i18n::Catalog,
    ) {
        resp.context_menu(|ui| {
            let session = self.mux.as_ref().and_then(|mux| {
                mux.tabs
                    .iter()
                    .flat_map(|tab| &tab.panes)
                    .find(|pane| &pane.id == pane_id)
                    .and_then(|pane| pane.session_id)
            });
            // 선택 텍스트(파일명 드래그)가 열 수 있는 파일이면 "열기" 항목을 맨 위에
            // (2026-07-14 사용자: 더블클릭 열기는 복사와 겹쳐 우클릭 메뉴로). 메뉴가
            // 열려 있는 동안만 평가되고, 해석은 resolve_path_cached의 TTL 캐시를 탄다.
            if let Some(sel_session) = session
                && let Some((s, a, b)) = self.selection
                && s == sel_session
            {
                let text = self
                    .sessions
                    .get(&sel_session)
                    .and_then(|view| view.snapshot.as_ref())
                    .map(|snap| renderer_egui::selection_text(snap, a.min(b), a.max(b)));
                if let Some(text) = text
                    && !text.trim().is_empty()
                    && !text.contains('\n')
                    && let Some(PathClick::OpenFile(path)) =
                        self.resolve_path_cached(sel_session, text.trim())
                {
                    let name = super::path_file_name_display(&path);
                    if ui
                        .button(catalog.t("workspace.open_file", &[("name", name.as_str())]))
                        .clicked()
                    {
                        self.request_open_path(path);
                        ui.close();
                    }
                    ui.separator();
                }
            }
            // 복사 + 에이전트로 보내기: 선택 텍스트가 있으면 표시
            // ("열기" 항목의 selection 판별 코드를 재사용).
            if let Some(sel_session) = session
                && let Some((s, a, b)) = self.selection
                && s == sel_session
            {
                let text = self
                    .sessions
                    .get(&sel_session)
                    .and_then(|view| view.snapshot.as_ref())
                    .map(|snap| renderer_egui::selection_text(snap, a.min(b), a.max(b)));
                if let Some(text) = text
                    && !text.trim().is_empty()
                {
                    if ui.button(catalog.t("workspace.menu.copy", &[])).clicked() {
                        ui.ctx().copy_text(text.clone());
                        ui.close();
                    }
                    // 터미널 화면에서 여러 행을 드래그하면 selection_text가 화면 행마다
                    // 개행을 넣는다. 문서형 명령의 들여쓰기/빈 행/줄 연속 `\`까지 그대로
                    // 복사하면 다시 붙였을 때 명령이 여러 조각으로 깨지므로, 선택 원문을
                    // 한 줄 명령으로 정리해 클립보드에 넣는 명시적 복사 동작을 제공한다.
                    if ui
                        .button(catalog.t("workspace.menu.copy_trimmed", &[]))
                        .clicked()
                    {
                        ui.ctx().copy_text(clean_terminal_selection_for_copy(&text));
                        ui.close();
                    }
                    // 선택 → 메모에 추가 (PR-4): 선택 원문을 그대로 워크스페이스 메모
                    // 끝에 붙인다. "복사"와 같은 selection 판별을 재사용하고, 개행
                    // 처리·상한 판정은 요청만 올려보내 App이 한다(leaf는 storage
                    // 상수를 못 본다).
                    if ui
                        .button(catalog.t("workspace.menu.add_to_note", &[]))
                        .clicked()
                    {
                        self.note_append_request = Some(text.clone());
                        ui.close();
                    }
                    // 선택 → 에이전트로 보내기 (2026-07-17 시나리오 ①): 에러 출력을
                    // 복사→pane 전환→붙여넣기→타이핑하던 흐름을 우클릭 두 번으로 줄인다.
                    // 대상은 **실행 중으로 감지된 에이전트 pane**(등록 목록이 아니라
                    // agent_info) — 없으면 이 메뉴 자체가 안 보인다.
                    self.send_to_agent_menu(ui, &text, catalog);
                    self.environment_selection_menu(ui, sel_session, &text, catalog);
                }
            }
            // 마지막 명령 출력 복사/전송 (셸 통합 2단계) — 드래그 선택 없이도 세션이
            // 있으면 표시. OSC 133 C~D 마크 범위를 워커에서 추출해 되받는다.
            if let Some(out_session) = session {
                self.last_output_menu_items(ui, out_session, catalog);
            }
            // 붙여넣기: 세션이 있으면 항상 표시. 드래그앤드롭 텍스트 붙여넣기(위 dnd_release_payload
            // 처리)와 동일한 경로(terminal_text_paste_bytes + session_bracketed_paste)로 주입한다.
            // send()가 WriteInput 공통 지점에서 선택 해제를 처리하므로 별도 clear_selection 불필요.
            if let Some(paste_session) = session
                && ui.button(catalog.t("workspace.menu.paste", &[])).clicked()
            {
                self.request_terminal_clipboard(
                    paste_session,
                    self.session_bracketed_paste(paste_session),
                    self.session_shell_kind(paste_session),
                    None,
                );
                ui.close();
            }
            if ui
                .button(catalog.t("workspace.split_horizontal", &[]))
                .clicked()
            {
                self.send(RuntimeCommand::SplitPane {
                    pane: pane_id.clone(),
                    direction: SplitDirection::Horizontal,
                    scrollback_lines: config.scrollback_lines as usize,
                });
                ui.close();
            }
            if ui
                .button(catalog.t("workspace.split_vertical", &[]))
                .clicked()
            {
                self.send(RuntimeCommand::SplitPane {
                    pane: pane_id.clone(),
                    direction: SplitDirection::Vertical,
                    scrollback_lines: config.scrollback_lines as usize,
                });
                ui.close();
            }
            ui.separator();
            // 스크롤백에서 맨 아래(라이브 화면)로 복귀 — 세션이 있는 pane에서만 노출.
            if let Some(session) = session
                && ui
                    .button(catalog.t("workspace.menu.scroll_bottom", &[]))
                    .clicked()
            {
                self.scroll_session_to_bottom(session);
                ui.close();
            }
            // 세션 폴더 진입 동선 (2026-07-18 사용자): 파일 트리를 이 세션의 현재
            // 폴더로 이동 / Finder로 열기. cwd 해석·라우팅은 App이 take해 수행한다.
            if let Some(session) = session {
                self.session_folder_menu_items(ui, session, catalog);
            }
            // (수동 상태 지정 U17b 서브메뉴는 사이드바와 함께 제거 — hook 감지 정착,
            // 2026-07-17 사용자. wire 명령 SetUserStatusOverride는 계약상 유지.)
            if ui.button(catalog.t("workspace.close_pane", &[])).clicked() {
                self.request_close_pane(pane_id.clone());
                ui.close();
            }
            ui.separator();
            // E4 ⑥: 프로젝트 화면에서 바로 환경변수·API 설정 진입 (에이전트에게 줄
            // 환경변수를 작업 중 즉시 등록하는 동선 — 사용자 시나리오).
            if ui
                .button(catalog.t("workspace.open_environment", &[]))
                .clicked()
            {
                self.open_environment_requested =
                    Some(self.environment_open_request(session, None));
                ui.close();
            }
        });
    }

    /// pane 우클릭의 환경설정 진입 요청을 소비한다 (E4 ⑥ — App이 프레임마다 확인).
    pub fn take_open_environment(&mut self) -> Option<super::environment::EnvironmentOpenRequest> {
        self.open_environment_requested.take()
    }

    pub fn take_new_session_requested(&mut self) -> Option<NewSessionRequest> {
        self.new_session_requested.take()
    }

    /// pane 우클릭의 세션 폴더 요청(트리 이동/Finder)을 소비한다 — App이 프레임마다
    /// 확인해 cwd 해석 후 라우팅한다(2026-07-18).
    pub fn take_session_folder_request(&mut self) -> Option<SessionFolderRequest> {
        self.session_folder_request.take()
    }

    /// pane 우클릭의 "메모에 추가" 요청을 소비한다 — App이 프레임마다 확인해 개행
    /// 처리·상한 판정 후 pending_note에 반영한다(PR-4, take_session_folder_request와
    /// 같은 one-shot 소비 패턴).
    pub fn take_note_append_request(&mut self) -> Option<String> {
        self.note_append_request.take()
    }

    /// pane 하단 「다시 실행」 클릭을 소비한다 — App이 프레임마다 확인해
    /// RuntimeCommand::RespawnArchivedAgent를 보낸다(PR-3, 위 take_* 계열과 같은
    /// one-shot 소비 패턴).
    pub fn take_respawn_archived_request(&mut self) -> Option<SessionId> {
        self.respawn_archived_request.take()
    }

    /// "메모에 추가"가 상한 초과로 거부됐음을 사용자에게 알린다. 판정 자체는 App
    /// 몫이지만(leaf는 storage 상수를 못 본다) 전달은 다른 오류와 같은 알림 경로를 쓴다.
    pub fn report_note_append_rejected(&mut self, message: String) {
        self.error_is_pressure = false;
        self.error = Some(message);
    }

    /// 「파일 트리를 이 폴더로 이동 / Finder에서 폴더 열기」 (2026-07-18 사용자) —
    /// pane_context_menu에서 분리해 kittest 대상(last_output_menu_items 관례).
    fn session_folder_menu_items(
        &mut self,
        ui: &mut egui::Ui,
        session: SessionId,
        catalog: &i18n::Catalog,
    ) {
        if ui
            .button(catalog.t("workspace.menu.reveal_in_tree", &[]))
            .clicked()
        {
            self.session_folder_request = Some(SessionFolderRequest::RevealInTree(session));
            ui.close();
        }
        if ui
            .button(catalog.t("sidebar.menu.open_folder", &[]))
            .clicked()
        {
            self.session_folder_request = Some(SessionFolderRequest::OpenInFinder(session));
            ui.close();
        }
    }

    /// command 전송 프레임만 한 번 더 그린다. 이후 상태 변화는 RuntimeEvent wake가
    /// 담당하며 pending spawn을 타이머로 폴링하지 않는다.
    fn flush_command_repaint(&mut self, ctx: &egui::Context) {
        if self.command_sent {
            self.command_sent = false;
            ctx.request_repaint();
        }
    }

    /// 최신 mux 스냅샷 (알림 센터가 pane 조회·제목에 사용).
    /// 사이드바 세션 목록용 항목 조립 (2026-07-05 — workspace 사이드바).
    pub fn session_entries(
        &self,
        catalog: &i18n::Catalog,
        agent_activity: &std::collections::HashMap<
            runtime::SessionId,
            crate::agent_transcript::AgentActivity,
        >,
        needs_input: &std::collections::HashSet<runtime::SessionId>,
        responses: &std::collections::HashSet<runtime::SessionId>,
        turn_done: &std::collections::HashMap<runtime::SessionId, i64>,
        hook_working: &std::collections::HashSet<runtime::SessionId>,
    ) -> Vec<crate::ui::file_tree::SessionEntry> {
        let Some(mux) = &self.mux else {
            return Vec::new();
        };
        mux.tabs
            .iter()
            .flat_map(|tab| tab.panes.iter().map(move |pane| (tab, pane)))
            .map(|(tab, pane)| {
                let regex_status = pane
                    .session_id
                    .and_then(|s| self.sessions.get(&s))
                    .and_then(|v| v.status);
                // transcript 기반 활동(옵션2)이 있으면 regex 상태를 덮어쓴다 — 단 '주의'
                // 상태(승인/오류/완료)는 transcript에 없는 신호라 regex를 우선한다.
                let activity = pane
                    .session_id
                    .and_then(|s| agent_activity.get(&s).copied());
                // hook이 보고한 needsInput = 가장 신뢰도 높은 승인 신호(최우선).
                let waiting = pane.session_id.is_some_and(|s| needs_input.contains(&s));
                let done = pane.session_id.is_some_and(|s| turn_done.contains_key(&s));
                // hook이 보고한 "작업 중"(v32) — transcript보다 즉시·정확(턴 경계).
                let working = pane.session_id.is_some_and(|s| hook_working.contains(&s));
                let merged = merge_agent_status(
                    regex_status,
                    activity,
                    waiting,
                    done,
                    working,
                    pane.session_id.is_some_and(|id| responses.contains(&id)),
                );
                // U17b: 수동 오버라이드가 있으면 최우선(status view의 user_override).
                let view = pane
                    .session_id
                    .and_then(|s| self.sessions.get(&s))
                    .and_then(|v| v.status_view.as_ref());
                let user_override = view.and_then(|v| v.user_override);
                let status = user_override.or(merged);
                let summary = pane
                    .session_id
                    .and_then(|s| self.sessions.get(&s))
                    .map(|v| v.summary.clone())
                    .unwrap_or_default();
                // summary와 같은 통과 패턴 — leaf가 SessionView 내부를 몰라도 되게.
                let last_output_at = pane
                    .session_id
                    .and_then(|s| self.sessions.get(&s))
                    .and_then(|v| v.last_output_at);
                let osc = self.session_osc_title(pane.session_id);
                let project_context = self.session_project_context(pane.session_id);
                // 사용자 이름 판별은 자동 프로젝트/OSC 제목으로 바꾸기 전에 한다.
                let title_is_custom = !is_default_session_title(&pane.title);
                // 이름 유무에 따라 같은 두 줄 안에서 작업과 모델 정보의 위치를 바꾼다.
                let info = pane.session_id.and_then(|s| self.agent_info.get(&s));
                // 요약/요청이 없다는 이유로 새 세션으로 단정하지 않는다.
                // transcript 미확인 세션도 기존 프로젝트/상태 설명을 유지한다.
                let (agent_line, status_label, status_line) = match info {
                    Some(d) => (
                        Some(agent_info_line(d)),
                        Some(session_status_label(status, catalog)),
                        Some(agent_activity_line(
                            d,
                            project_context.as_deref(),
                            status,
                            catalog,
                        )),
                    ),
                    None => (None, None, None),
                };
                crate::ui::file_tree::SessionEntry {
                    tab: tab.id.clone(),
                    pane: pane.id.clone(),
                    session: pane.session_id,
                    resumable: false,   // App이 restore_agents 기준으로 채운다
                    has_cwd: false,     // App이 session_cwds 기준으로 채운다 (PR-W)
                    in_worktree: false, // App이 session_cwds 기준으로 채운다 (2026-07-18)
                    title: self.resolve_session_title(
                        &pane.title,
                        pane.session_id,
                        osc.as_deref(),
                        catalog,
                    ),
                    title_is_custom,
                    agent_model: info
                        .filter(|_| title_is_custom)
                        .and_then(|d| d.model.clone()),
                    status,
                    summary,
                    focused: mux.focused_pane.as_ref() == Some(&pane.id),
                    attention: false, // App의 alert 추적이 채운다 (update_session_alerts)
                    pulse: None,
                    agent_line,
                    status_label,
                    status_line,
                    last_output_at,
                }
            })
            .collect()
    }

    /// 응답(Spawned/Failed)을 아직 못 받은 셸 spawn 수 — App의 suspend 보호가
    /// "spawn 진행 중 = live"로 판정하는 데 쓴다 (codex High race).
    pub fn pending_spawns(&self) -> u32 {
        self.pending_spawn_cwds
            .iter()
            .filter(|pending| matches!(pending, PendingShellSpawn::Awaiting { .. }))
            .count() as u32
    }

    pub fn spawn_shell(&mut self, scrollback_lines: usize) {
        self.send(RuntimeCommand::SpawnShell {
            cols: 80,
            rows: 24,
            scrollback_lines,
        });
    }

    /// 새 셸 + 스폰 완료 시 해당 폴더로 cd 1회 주입 — 사이드바 '같은 폴더에서 새 셀'.
    /// SpawnShell wire에 cwd 필드를 더하는 대신(계약 변경) ShellSpawned 응답에서
    /// cd를 주입한다 (자동 resume의 cd prefix와 같은 관례). cwd가 None이면 일반 스폰.
    pub fn spawn_shell_at(&mut self, scrollback_lines: usize, cwd: Option<String>) {
        if cwd.as_ref().is_some_and(|cwd| {
            cwd.is_empty() || cwd.len() > WORKSPACE_PATH_MAX_BYTES || cwd.as_bytes().contains(&0)
        }) {
            // cwd는 항상 앱이 자체 추적하는 실제 경로에서 오므로 이 분기는 사실상 도달
            // 불가한 내부 불변식 방어다 — 사용자가 직접 만든 값이 아니라 배너로 보여줘도
            // 이해도 대응도 못 한다. 진단용 로그만 남긴다.
            tracing::warn!(
                kind = "workspace",
                phase = "spawn_admission",
                error_code = "invalid_cwd",
                "spawn cwd failed validation"
            );
            return;
        }
        if let Err(code) = self.queue_protocol_intent_with_spawn_cwd(
            RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines,
            },
            cwd,
        ) {
            self.report_protocol_queue_rejection(code, "spawn_admission");
        }
    }

    /// 단축키용 현재 pane 닫기. 실행 중인 세션은 마우스 ×와 동일하게 확인창을 거친다.
    pub fn close_focused_pane(&mut self) {
        if let Some(pane) = self.mux.as_ref().and_then(|mux| mux.focused_pane.clone()) {
            self.request_close_pane(pane);
        }
    }

    /// 앱의 단축키(⌘↓)가 지정한 포커스 pane을 스크롤백 맨 아래로 되돌린다.
    pub fn scroll_focused_to_bottom(&mut self) {
        let session = self.mux.as_ref().and_then(|mux| {
            mux.focused_pane.as_ref().and_then(|pane| {
                mux.tabs
                    .iter()
                    .flat_map(|tab| &tab.panes)
                    .find(|p| &p.id == pane)
                    .and_then(|p| p.session_id)
            })
        });
        if let Some(session) = session {
            self.scroll_session_to_bottom(session);
        }
    }

    /// 클릭한 pane의 세션만 최신 출력으로 이동한다.
    fn scroll_session_to_bottom(&mut self, session: SessionId) {
        self.clear_selection(session);
        self.scroll_residual = 0.0;
        self.drag_autoscroll_residual = 0.0;
        self.send(RuntimeCommand::ScrollToBottom { session });
    }

    /// 단축키(⌘⇧↑/↓)용 포커스된 pane을 이전/다음 프롬프트 마크(OSC 133)로 점프한다.
    /// 마크 조회·델타 계산은 워커(세션) 소유 — scroll_focused_to_bottom과 동일 구조.
    pub fn scroll_focused_to_prompt(&mut self, direction: i8) {
        let session = self.mux.as_ref().and_then(|mux| {
            mux.focused_pane.as_ref().and_then(|pane| {
                mux.tabs
                    .iter()
                    .flat_map(|tab| &tab.panes)
                    .find(|p| &p.id == pane)
                    .and_then(|p| p.session_id)
            })
        });
        if let Some(session) = session {
            self.send(RuntimeCommand::ScrollToPrompt { session, direction });
        }
    }

    pub fn split_pane(
        &mut self,
        pane: runtime::MuxPaneId,
        direction: SplitDirection,
        scrollback_lines: usize,
    ) {
        self.send(RuntimeCommand::SplitPane {
            pane,
            direction,
            scrollback_lines,
        });
    }

    /// 단축키용 현재 pane 분할. UI 버튼과 같은 runtime 명령을 사용한다.
    pub fn split_focused_pane(&mut self, direction: SplitDirection, scrollback_lines: usize) {
        // focused_pane이 없으면(복원 직후·pane 미클릭·단일 pane) 활성 탭의 첫 pane으로
        // 폴백한다 — 안 그러면 분할 단축키/버튼이 조용히 아무 것도 안 해 "고장난 것처럼"
        // 보인다(사용자 보고 2026-07-12).
        if let Some(pane) = self.mux.as_deref().and_then(split_target_pane) {
            self.send(RuntimeCommand::SplitPane {
                pane,
                direction,
                scrollback_lines,
            });
        }
    }

    /// 활성 탭의 pane 벡터 순서로 포커스를 순환한다. 끝에서는 반대편으로 이어진다.
    pub fn focus_relative_pane(&mut self, delta: isize) {
        let next = self.mux.as_ref().and_then(|mux| {
            let active = mux.active_tab.as_ref()?;
            let tab = mux.tabs.iter().find(|tab| &tab.id == active)?;
            if tab.panes.len() < 2 {
                return None;
            }
            let focused = mux.focused_pane.as_ref()?;
            let current = tab.panes.iter().position(|pane| &pane.id == focused)?;
            let next = (current as isize + delta).rem_euclid(tab.panes.len() as isize) as usize;
            Some(tab.panes[next].id.clone())
        });
        if let Some(pane) = next {
            self.request_pane_focus(pane);
        }
    }

    pub fn mux(&self) -> Option<&Arc<MuxSnapshot>> {
        self.mux.as_ref()
    }

    /// 포커스된 pane의 세션 id — 워크스페이스 이름(현재 작업 폴더) 추적용.
    pub fn focused_session(&self) -> Option<SessionId> {
        let mux = self.mux.as_ref()?;
        let focused = mux.focused_pane.as_ref()?;
        mux.tabs
            .iter()
            .flat_map(|t| &t.panes)
            .find(|p| &p.id == focused)
            .and_then(|p| p.session_id)
    }

    pub fn cloud_agent_screen(&self, session: SessionId) -> Option<String> {
        self.sessions
            .get(&session)?
            .snapshot
            .as_ref()
            .map(|s| crate::cloud_agent::screen_text(s))
    }

    pub fn session_bracketed_paste(&self, session: SessionId) -> bool {
        self.sessions
            .get(&session)
            .is_some_and(|view| view.bracketed_paste)
    }

    pub fn session_shell_kind(&self, _session: SessionId) -> crate::ui::file_tree::ShellKind {
        self.shell_kind
    }

    /// 상태가 강조 대상(입력요청/작업종료)으로 **새로** 전이하면 그 세션 pane을 플래시한다.
    /// 반드시 sessions의 status를 갱신하기 **전에** 불러 직전 상태를 읽는다.
    fn note_status_flash(&mut self, session: SessionId, new_status: SessionStatus) {
        let prev = self.sessions.get(&session).and_then(|view| view.status);
        if is_flash_status(new_status) && prev != Some(new_status) {
            self.session_flash.insert(
                session,
                (std::time::Instant::now() + PANE_FLASH, PANE_FLASH),
            );
        }
    }

    /// `SessionExited`/`SessionRestored` 공통 부기 — exit_code·결과 상태 배지를
    /// 채우고, PR-3의 restored_readonly만 갈라 하단 배너/재실행 버튼 문구를 가른다
    /// (호출부 두 곳: 활성 워크스페이스 handle_events, warm apply_warm_events).
    fn apply_session_exit(&mut self, session: SessionId, exit_code: Option<u32>, restored: bool) {
        if !self.session_alive(session) {
            return;
        }
        self.split_final_resize_sessions.remove(&session);
        self.split_final_resize_pending
            .retain(|_, (id, _, _)| *id != session);
        self.resize_delivery_rollbacks
            .retain(|_, rollback| rollback.session != session);
        let view = self.sessions.entry(session).or_default();
        view.cancel_resize_request();
        view.exit_code = Some(exit_code);
        view.restored_readonly = restored;
        // 진행형 상태(⏳/✋)는 종료와 함께 무효. 결과 상태(✅/❌)는 유지하고,
        // 없으면 exit code로 채운다 — 알림(on_exit)과 tab 아이콘이 같은 결과를
        // 보여주도록(codex 리뷰 반영).
        if !matches!(
            view.status,
            Some(SessionStatus::Done) | Some(SessionStatus::Error)
        ) {
            view.status = Some(if exit_code == Some(0) {
                SessionStatus::Done
            } else {
                SessionStatus::Error
            });
        }
    }

    fn session_alive(&self, session: SessionId) -> bool {
        self.mux.as_ref().is_some_and(|mux| {
            mux.tabs
                .iter()
                .flat_map(|tab| &tab.panes)
                .any(|pane| pane.session_id == Some(session))
        })
    }

    fn session_visible(&self, session: SessionId) -> bool {
        self.mux.as_ref().is_some_and(|mux| {
            mux.active_tab
                .as_ref()
                .and_then(|active| mux.tabs.iter().find(|tab| &tab.id == active))
                .is_some_and(|tab| {
                    tab.panes
                        .iter()
                        .any(|pane| pane.session_id == Some(session))
                })
        })
    }

    fn send(&mut self, command: RuntimeCommand) {
        // 터미널에 입력/스크롤을 보내면 그 세션 선택을 해제한다 — 선택 중엔 화면이 freeze돼
        // (선택 정확성) 있어, 안 지우면 타이핑·스크롤해도 화면이 멈춘 듯 보인다(사용자:
        // 드래그 선택 후 스크롤이 안 내려감). 공통 지점이라 여기서 한 번에 처리한다.
        // 예외: 드래그 오토스크롤의 Scroll은 send_keep_selection으로 보낸다 — 이 해제와
        // 정면 충돌해 첫 스크롤 직후 선택·오토스크롤이 함께 죽었다(감사 A3 버그 #1).
        let touched = match &command {
            RuntimeCommand::WriteInput { session, .. } => Some(*session),
            RuntimeCommand::Scroll { session, .. } => Some(*session),
            _ => None,
        };
        if let Some(session) = touched
            && self.selection.is_some_and(|(s, _, _)| s == session)
        {
            self.selection = None;
        }
        self.send_keep_selection(command);
    }

    /// Runtime snapshot을 기다리지 않는 터미널 refocus를 시작한다. egui의 TextEdit state가
    /// 사라지는 데 한 프레임 더 걸려도, 그 사이 첫 printable key를 잃지 않는다.
    fn flush_pending_ime_submit(&mut self) {
        if let Some(detached) = self.detached_ime_submit.take() {
            self.send_deferred_ime_submit(detached, None);
        }
        self.flush_active_ime_submit();
    }

    fn flush_active_ime_submit(&mut self) {
        let Some(deferred) = self.pending_ime_submit.take() else {
            return;
        };
        self.send_deferred_ime_submit(deferred, None);
    }

    fn send_deferred_ime_submit(
        &mut self,
        mut deferred: PendingImeSubmit,
        committed: Option<&str>,
    ) {
        let mut bytes = Vec::new();
        let composed = committed.unwrap_or(&deferred.preedit_at_submit);
        if !composed.is_empty() {
            bytes.extend_from_slice(composed.as_bytes());
            if committed.is_none() {
                self.flushed_ime_commit = Some((composed.to_owned(), std::time::Instant::now()));
            }
        }
        let overlap = committed.map_or(0, |text| {
            (1..=text.len().min(deferred.before_submit.len()))
                .rev()
                .find(|&length| text.as_bytes().ends_with(&deferred.before_submit[..length]))
                .unwrap_or(0)
        });
        bytes.extend_from_slice(&deferred.before_submit[overlap..]);
        bytes.append(&mut deferred.independent_before_submit);
        bytes.append(&mut deferred.after_submit);
        if !bytes.is_empty() {
            self.send(RuntimeCommand::WriteInput {
                session: deferred.owner,
                bytes,
            });
        }
    }

    fn resolve_detached_ime_submit(
        &mut self,
        events: &[egui::Event],
        ctx: &egui::Context,
    ) -> Option<(usize, String, std::time::Instant)> {
        let deferred = self.detached_ime_submit.take()?;
        for (index, event) in events.iter().enumerate() {
            match event {
                egui::Event::Ime(egui::ImeEvent::Preedit { text, .. }) if !text.is_empty() => {
                    self.send_deferred_ime_submit(deferred, None);
                    return None;
                }
                egui::Event::Ime(egui::ImeEvent::Commit(text)) => {
                    if !deferred.preedit_at_submit.is_empty()
                        && text.starts_with(&deferred.preedit_at_submit)
                    {
                        let submitted_at = deferred.started;
                        self.send_deferred_ime_submit(deferred, Some(text));
                        return Some((index, text.clone(), submitted_at));
                    }
                    self.send_deferred_ime_submit(deferred, None);
                    return None;
                }
                _ => {}
            }
        }
        if deferred.started.elapsed() >= std::time::Duration::from_secs(2) {
            self.send_deferred_ime_submit(deferred, None);
        } else {
            ctx.request_repaint_after(
                std::time::Duration::from_secs(2).saturating_sub(deferred.started.elapsed()),
            );
            self.detached_ime_submit = Some(deferred);
        }
        None
    }

    fn consume_flushed_ime_commit(
        &mut self,
        events: &[egui::Event],
    ) -> Option<(usize, String, std::time::Instant)> {
        let (flushed, at) = self.flushed_ime_commit.as_ref()?;
        if flushed.is_empty() || at.elapsed() >= std::time::Duration::from_secs(2) {
            self.flushed_ime_commit = None;
            return None;
        }
        for (index, event) in events.iter().enumerate() {
            match event {
                egui::Event::Ime(egui::ImeEvent::Preedit { text, .. }) if !text.is_empty() => {
                    self.flushed_ime_commit = None;
                    return None;
                }
                egui::Event::Ime(egui::ImeEvent::Commit(text)) => {
                    let (flushed, at) = self.flushed_ime_commit.take()?;
                    return text
                        .starts_with(&flushed)
                        .then(|| (index, text.clone(), at));
                }
                _ => {}
            }
        }
        None
    }

    fn begin_terminal_refocus(&mut self, pane: runtime::MuxPaneId) {
        self.pending_focus = Some(pane);
        if let Some(deferred) = self.pending_ime_submit.take() {
            if deferred.preedit_at_submit.is_empty() {
                self.send_deferred_ime_submit(deferred, None);
            } else {
                if let Some(previous) = self.detached_ime_submit.replace(deferred) {
                    self.send_deferred_ime_submit(previous, None);
                }
            }
        }
        self.preedit.clear();
    }

    /// Runtime의 mux snapshot이 도착하기 전에도 입력을 새 pane으로 보낸다. 그렇지 않으면
    /// pane을 클릭하거나 검색을 닫은 직후의 첫 `.`, 공백, 한글 조합이 버려질 수 있다.
    fn request_pane_focus(&mut self, pane: runtime::MuxPaneId) {
        self.begin_terminal_refocus(pane.clone());
        self.send(RuntimeCommand::FocusPane { pane });
    }

    /// 선택을 해제하지 않는 send — 드래그 오토스크롤 전용(선택을 유지·확장하며
    /// 스크롤해야 한다). 휠/타이핑은 반드시 [`Self::send`]를 쓴다.
    fn send_keep_selection(&mut self, command: RuntimeCommand) -> bool {
        match self.queue_protocol_intent_owned(command, None) {
            Ok(_) => true,
            Err((WorkspaceProtocolErrorCode::Busy, command))
                if terminal_protocol_command(&command) =>
            {
                // Reserve entries append to the same deque: Focus/Resize/control boundaries
                // cannot be bypassed by later input. Existing adjacent input coalescing ran first.
                if self.protocol_intents.len() + self.protocol_inflight.len()
                    < TERMINAL_PROTOCOL_PRESSURE_CAP
                {
                    let (operation, generation) = self.next_protocol_operation();
                    self.protocol_intents.push_back(WorkspaceProtocolIntent {
                        operation,
                        generation,
                        command: *command,
                        spawn_cwd: None,
                    });
                    self.command_sent = true;
                    true
                } else {
                    self.report_protocol_queue_rejection(
                        WorkspaceProtocolErrorCode::DeliveryFailed,
                        "terminal_pressure_capacity",
                    );
                    false
                }
            }
            Err((code, _)) => {
                self.report_protocol_queue_rejection(code, "protocol_queue");
                false
            }
        }
    }

    /// queue_protocol_intent*의 동기 거부(2026-08-18, "terminal protocol request rejected"
    /// 배너 버그 수정)를 공통 처리한다. Busy(자연히 풀리는 큐 포화)와 InvalidCommand(사용자가
    /// 만들 수 없는 내부 계약 위반)는 배너를 띄워도 대응할 수 없어 tracing만 남긴다.
    /// PayloadTooLarge/DeliveryFailed만 정말 되돌릴 수 없이 사라진 요청이라
    /// protocol_request_lost를 세워 take_error_notice가 알림 문구를 만들게 한다.
    fn report_protocol_queue_rejection(
        &mut self,
        code: WorkspaceProtocolErrorCode,
        phase: &'static str,
    ) {
        match code {
            WorkspaceProtocolErrorCode::Busy => {
                tracing::debug!(
                    kind = "workspace",
                    phase = phase,
                    error_code = "busy",
                    "protocol queue saturated; caller may retry"
                );
            }
            WorkspaceProtocolErrorCode::InvalidCommand => {
                tracing::warn!(
                    kind = "workspace",
                    phase = phase,
                    error_code = "invalid_command",
                    "internal protocol command failed validation"
                );
            }
            WorkspaceProtocolErrorCode::PayloadTooLarge
            | WorkspaceProtocolErrorCode::DeliveryFailed => {
                self.error_is_pressure = false;
                self.protocol_request_lost = true;
                tracing::warn!(
                    kind = "workspace",
                    phase = phase,
                    error_code = ?code,
                    "terminal request was dropped"
                );
            }
        }
    }
}

/// File-tree/sidebar path insertion uses the same paste byte semantics as clipboard paste.
pub(crate) fn path_insert_paste_bytes(
    path: &Path,
    shell_kind: crate::ui::file_tree::ShellKind,
    bracketed_paste: bool,
) -> Vec<u8> {
    let raw = crate::ui::file_tree::shell_path_insert_bytes_for(path, shell_kind);
    input_mapper::paste_bytes(&raw, bracketed_paste)
}

/// 포커스 터미널에서 이 폴더로 이동 — `cd <quoted-path>` + 실행(Enter).
/// 붙여넣기(bracketed)로 명령을 넣은 뒤 CR을 브라켓 밖에 붙여 실행되게 한다(2026-07-08).
pub(crate) fn cd_paste_bytes(
    path: &Path,
    shell_kind: crate::ui::file_tree::ShellKind,
    bracketed_paste: bool,
) -> Vec<u8> {
    use crate::ui::file_tree::ShellKind;
    let quoted = crate::ui::file_tree::shell_quote_for(path, shell_kind);
    // 셸별 cd 문법 — PowerShell은 -LiteralPath로 와일드카드(`foo[bar]`) 해석을 막고,
    // cmd는 `/d`로 드라이브 변경까지 처리한다(codex Medium 2026-07-08).
    let cmd = match shell_kind {
        ShellKind::PowerShell => format!("Set-Location -LiteralPath {quoted}"),
        ShellKind::Cmd => format!("cd /d {quoted}"),
        ShellKind::Posix | ShellKind::Fish => format!("cd {quoted}"),
    };
    let mut bytes = input_mapper::paste_bytes(cmd.as_bytes(), bracketed_paste);
    bytes.push(b'\r'); // 실행 — bracketed paste 종료 뒤의 CR
    bytes
}

pub(crate) fn paths_insert_paste_bytes(
    paths: &[std::path::PathBuf],
    shell_kind: crate::ui::file_tree::ShellKind,
    bracketed_paste: bool,
) -> Vec<u8> {
    let mut raw = Vec::new();
    for path in paths {
        raw.extend(crate::ui::file_tree::shell_path_insert_bytes_for(
            path, shell_kind,
        ));
    }
    input_mapper::paste_bytes(&raw, bracketed_paste)
}

/// raw 제목이 아직 rename 안 된 기본 셸/에이전트 제목("workspace.spawn.shell 134" 등)인가.
/// 기본 제목이면 프로젝트명으로 대체 표시한다(resolve_session_title / 활동 패널의
/// warm·유휴 워크스페이스 행도 같은 규칙을 쓴다 — App::activity_session_name).
pub(crate) fn is_default_session_title(raw: &str) -> bool {
    let Some((prefix, suffix)) = raw.rsplit_once(' ') else {
        return false;
    };
    matches!(
        prefix,
        "workspace.spawn.shell" | "셸" | "workspace.spawn.agent" | "에이전트"
    ) && suffix.parse::<u64>().is_ok()
}

pub(crate) fn display_pane_title(raw: &str, catalog: &i18n::Catalog) -> String {
    let Some((prefix, suffix)) = raw.rsplit_once(' ') else {
        return match raw {
            "workspace.spawn.shell" | "셸" => catalog.t("workspace.spawn.shell", &[]),
            "workspace.spawn.agent" | "에이전트" => catalog.t("workspace.spawn.agent", &[]),
            _ => raw.to_owned(),
        };
    };
    let key = match prefix {
        "workspace.spawn.shell" | "셸" => Some("workspace.spawn.shell"),
        "workspace.spawn.agent" | "에이전트" => Some("workspace.spawn.agent"),
        _ => None,
    };
    if let Some(key) = key
        && suffix.parse::<u64>().is_ok()
    {
        return format!("{} {suffix}", catalog.t(key, &[]));
    }
    raw.to_owned()
}

/// cwd에서 뽑은 프로젝트명이 이 프로젝트명이 속한 워크스페이스 **자신의** 이름과 다를
/// 때 그 프로젝트명만 단독으로 보여주면, 세션이 실제로는 그대로인데도 "다른
/// 워크스페이스의 세션이 섞여 들어왔다"는 착각을 준다(2026-08-19 사용자 보고 —
/// Crawler 워크스페이스를 펼쳤더니 그 안의 세션이 「Design」으로 보였다. 실제로는
/// Crawler 세션이 cwd만 다른 프로젝트(Design이라는 이름의 다른 폴더)를 가리켰을 뿐
/// 세션이 섞인 게 아니었다 — 하필 그 폴더명이 다른 실제 워크스페이스 이름과 같아서
/// 착각이 생겼다). 두 이름이 같으면(가장 흔한 경우 — 워크스페이스 루트에서 그대로
/// 작업 중) 프로젝트명 그대로 보여 정보 중복이 없게 하고, 다르면 "프로젝트명
/// (워크스페이스명)"으로 소속을 함께 밝힌다 — 다른 워크스페이스 세션을 옆에 열 때 쓰는
/// attached_workspace_title(app.rs)과 같은 표기 관례라 사용자가 이미 본 패턴이다. cwd
/// 기반 프로젝트명 자체는 다른 폴더에서 띄운 세션을 구분하는 원래 목적대로 계속
/// 보여준다 — 워크스페이스 이름으로 완전히 대체하면 그 값어치가 없어진다.
/// (주의: 이 함수는 "작업 워크스페이스 목록"과 "환경 및 API 프로젝트 목록"의 독립을
/// 다루지 않는다 — 그건 closed_workspace_ids/hidden_env_project_ids의 별개 문제다.
/// 여기서 섞이는 건 같은 세션 표시줄 안의 두 이름(워크스페이스 자체 이름 vs cwd
/// 프로젝트명)일 뿐이다.)
///
/// 활성 워크스페이스(`WorkspaceUi::resolve_session_title`/`session_project_context`)와
/// warm·유휴 워크스페이스(`App::activity_session_name`, app.rs)가 **같은 화면
/// 문법**(사이드바 트리, 활동 패널, 폰 대시보드, OS 알림 모두 같은 세션 표시줄
/// 규칙을 공유한다)을 쓰므로 이 규칙을 leaf(workspace.rs)의 순수 자유 함수로 뽑아
/// 두 쪽이 같이 쓴다 — App은 leaf를 참조해도 되지만 leaf는 App을 참조하면 안 되므로
/// (「App::ui 안에서 IO 직접 호출 금지」와 같은 leaf/App 경계 방향) 위치는 leaf쪽이다.
/// App은 이미 계산해 갖고 있는 workspace_name 문자열만 넘기면 되는 순수 함수라
/// app.rs에서 가져다 쓰기도 쉽다.
pub(crate) fn qualify_project_name(project_name: &str, workspace_name: Option<&str>) -> String {
    match workspace_name.map(str::trim).filter(|own| !own.is_empty()) {
        Some(own) if own != project_name => format!("{project_name} ({own})"),
        _ => project_name.to_owned(),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TerminalTextDragPayload {
    text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TerminalDropFeedback {
    TerminalInsert,
    DocumentOpen,
}

fn classify_terminal_drop_feedback(
    typed_path_hovered: bool,
    terminal_text_hovered: bool,
    os_file_over_pane: bool,
) -> Option<TerminalDropFeedback> {
    if typed_path_hovered || os_file_over_pane {
        Some(TerminalDropFeedback::DocumentOpen)
    } else if terminal_text_hovered {
        Some(TerminalDropFeedback::TerminalInsert)
    } else {
        None
    }
}

fn request_terminal_os_drag_feedback_repaint(
    ctx: &egui::Context,
    input_enabled: bool,
    os_drag_active: bool,
) {
    if input_enabled && os_drag_active {
        // macOS winit 0.30은 draggingUpdated: 포인터 이동 이벤트를 주지 않는다.
        // OS drag가 실제로 진행 중일 때만 다음 frame을 요청해 AppKit 좌표를
        // 다시 샘플링한다. hovered_files가 비면 즉시 종료되어 idle 타이머가 남지 않는다.
        ctx.request_repaint();
    }
}

pub(crate) fn release_typed_dnd_payload<Payload>(response: &egui::Response) -> Option<Arc<Payload>>
where
    Payload: std::any::Any + Send + Sync,
{
    egui::DragAndDrop::has_payload_of_type::<Payload>(&response.ctx)
        .then(|| response.dnd_release_payload::<Payload>())
        .flatten()
}

fn release_file_dnd_paths(response: &egui::Response) -> Option<Vec<PathBuf>> {
    if let Some(group) =
        release_typed_dnd_payload::<crate::ui::file_tree::FileTreeDragPayload>(response)
    {
        Some(group.paths().to_vec())
    } else {
        release_typed_dnd_payload::<PathBuf>(response).map(|path| vec![path.as_ref().clone()])
    }
}

fn selection_range_contains(start: usize, end: usize, idx: usize) -> bool {
    start <= idx && idx <= end
}

/// 드래그 선택 오토스크롤 속도(행/초, RuntimeCommand::Scroll delta 부호 —
/// 양수=과거로). 포인터가 pane 세로 경계를 벗어난 거리에 비례해 빨라진다:
/// 1셀 초과당 8행/초, 최대 60행/초. 경계 안이면 0 (T4).
fn drag_autoscroll_rate(pointer_y: f32, top: f32, bottom: f32, cell_h: f32) -> f32 {
    // 위로 벗어남 = 양수(과거로), 아래로 벗어남 = 음수(최신으로)
    let overshoot = if pointer_y < top {
        top - pointer_y
    } else if pointer_y > bottom {
        bottom - pointer_y
    } else {
        return 0.0;
    };
    (overshoot / cell_h.max(1.0) * 8.0).clamp(-60.0, 60.0)
}

/// 휠 스크롤이 선택을 **보존**해야 하는가.
///
/// 평상시 휠은 선택을 해제한다(`send`) — 선택 중엔 화면이 freeze돼 있어서, 안 지우면
/// 스크롤해도 화면이 멈춘 듯 보이기 때문이다(2026-07 사용자 보고).
///
/// 그런데 **드래그하는 도중에는** 반대다. 한 화면에 안 들어오는 범위를 잡으려면 버튼을
/// 누른 채 휠로 화면을 옮기며 계속 끌 수 있어야 하는데, 여기서 선택이 풀리면 매번
/// 처음부터 다시 잡아야 한다(2026-08-18 사용자 요청). 포인터를 pane 밖으로 밀어내는
/// 기존 오토스크롤(`drag_autoscroll_rate`)과 같은 목적이고, 휠은 그보다 정밀하다.
///
/// 판정은 **드래그 중 + 그 세션의 선택이 살아 있음**이다.
///
/// `Response::dragged()`를 쓴다. 처음엔 "포인터가 멈춘 프레임엔 false가 된다"고 보고
/// `pointer.primary_down()`을 썼는데 **그건 사실이 아니다** — egui 0.35의
/// `interaction.rs`는 이전 프레임 값을 물려받고 릴리즈/Escape에서만 초기화하므로,
/// 버튼을 누른 채 가만히 있어도 `dragged()`는 계속 true다(2026-08-18 리뷰가 egui 단독
/// 프로젝트로 실측). 오히려 `primary_down`이 **더 넓어서** 문제였다 — 누른 직후
/// 클릭/드래그 판정 유예 구간에도 참이라, 아래 `dragged()` 분기(앵커를 보정하는 그
/// 분기)가 아직 안 도는데 스크롤만 나가 한 순간 화면이 안 따라오는 창이 생긴다.
///
/// 즉 **스크롤을 보존해 보내는 조건과 앵커를 보정하는 조건이 같아야** 어긋나지 않는다.
fn wheel_scroll_keeps_selection(dragging: bool, selection_on_session: bool) -> bool {
    dragging && selection_on_session
}

/// 스크롤로 화면이 delta_rows행 이동했을 때(양수=과거로 → 내용이 아래로 이동)
/// 화면 좌표 기반 선택 셀 인덱스를 같은 텍스트로 보정한다. 화면 밖으로 나가면
/// 선형 경계(첫 행 첫 열 / 마지막 행 마지막 열)로 clamp — 스냅샷이 가시 영역만
/// 담으므로 화면 밖 선택은 표현할 수 없다 (T4).
fn shift_selection_cell(idx: usize, delta_rows: i32, cols: usize, rows: usize) -> usize {
    let max_idx = (cols * rows).saturating_sub(1) as i64;
    (idx as i64 + delta_rows as i64 * cols as i64).clamp(0, max_idx) as usize
}

fn terminal_text_paste_bytes(text: &str, bracketed_paste: bool) -> Vec<u8> {
    input_mapper::paste_bytes(text.as_bytes(), bracketed_paste)
}

/// 터미널 화면에서 드래그한 여러 행을 다시 붙일 수 있는 한 줄 명령으로 정리한다.
///
/// - 행 앞뒤 공백과 빈 행은 제거한다.
/// - 다음 내용이 있는 행 끝의 unescaped `\`는 shell line-continuation이므로 제거한다.
/// - 남은 행은 공백 하나로 잇는다.
///
/// 행 내부는 건드리지 않아 `"value  with  spaces"` 같은 인용 값의 공백을 보존한다.
fn clean_terminal_selection_for_copy(text: &str) -> String {
    let mut lines = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .peekable();
    let mut cleaned = Vec::new();
    while let Some(line) = lines.next() {
        let mut line = line.to_owned();
        if lines.peek().is_some()
            && line.chars().rev().take_while(|ch| *ch == '\\').count() % 2 == 1
        {
            line.pop();
            line.truncate(line.trim_end().len());
        }
        if !line.is_empty() {
            cleaned.push(line);
        }
    }
    cleaned.join(" ")
}

/// 모델명이 이미 provider 이름을 품고 있는가 — `Claude · claude-opus-5`처럼 같은
/// 낱말이 한 줄에 두 번 나오는 것을 막는다(2026-08-20 사용자).
///
/// 포함 여부로만 판단한다: `claude-opus-5`는 "claude"를 품으므로 provider 라벨을
/// 빼고, `gpt-5.6-sol`은 "codex"를 품지 않으므로 **남긴다** — 그 경우엔 라벨이 어느
/// 에이전트인지 알려주는 유일한 단서라 지우면 정보가 준다.
fn model_implies_provider(provider_label: &str, model: &str) -> bool {
    let provider = provider_label.trim().to_ascii_lowercase();
    !provider.is_empty() && model.to_ascii_lowercase().contains(&provider)
}

/// 세션 행 2행: "Codex · gpt-5.5 · xhigh · ctx 69%" (빈 부분은 생략).
fn agent_info_line(d: &crate::agent_detect::AgentDisplay) -> String {
    use crate::agent_surface::AgentProvider;

    let provider = AgentProvider::from(d.kind);
    // 전송 방식 배지([PTY])는 뺀다(2026-08-19 사용자) — 이 앱의 에이전트 행은 전부 PTY라
    // 모든 행에 같은 글자가 붙어 구분에 기여하지 않았다. AgentTransport 자체는 다른
    // 표면(구조화 세션 목록)이 계속 쓴다.
    let model = d.model.as_deref().filter(|s| !s.is_empty());
    let mut parts = Vec::new();
    if !model.is_some_and(|m| model_implies_provider(provider.label(), m)) {
        parts.push(provider.label().to_owned());
    }
    if let Some(m) = model {
        parts.push(m.to_owned());
    }
    if let Some(e) = d.effort.as_deref().filter(|s| !s.is_empty()) {
        parts.push(e.to_owned());
    }
    if let Some(context_pct) = d.context_pct {
        parts.push(format!("ctx {context_pct}%"));
    }
    parts.join(" · ")
}

fn session_status_label(status: Option<runtime::SessionStatus>, catalog: &i18n::Catalog) -> String {
    use runtime::SessionStatus as S;
    let key = match status {
        Some(s) => match s {
            S::Running => "status.running",
            S::Waiting => "status.waiting",
            S::NeedsApproval => "status.needs_approval",
            S::Done => "status.done",
            S::Error => "status.error",
            S::Idle => "status.idle",
        },
        None => "status.detecting",
    };
    catalog.t(key, &[])
}

fn agent_activity_line(
    display: &crate::agent_detect::AgentDisplay,
    project_context: Option<&str>,
    status: Option<runtime::SessionStatus>,
    catalog: &i18n::Catalog,
) -> String {
    if let Some(task) = display
        .last_agent_summary
        .as_deref()
        .filter(|task| !task.is_empty())
    {
        return task.to_owned();
    }
    if let Some(instruction) = display
        .user_instruction
        .as_deref()
        .filter(|instruction| !instruction.trim().is_empty())
    {
        return instruction.to_owned();
    }
    if let Some(project) = project_context.filter(|project| !project.trim().is_empty()) {
        return project.to_owned();
    }
    use runtime::SessionStatus as S;
    let key = match status {
        Some(S::Running) => "session.activity.running",
        Some(S::Waiting) => "session.activity.waiting",
        Some(S::NeedsApproval) => "status.needs_approval",
        Some(S::Done) => "session.activity.done",
        Some(S::Error) => "session.activity.error",
        Some(S::Idle) => "session.activity.idle",
        None => "session.activity.detecting",
    };
    catalog.t(key, &[])
}

fn request_terminal_focus(response: &egui::Response) {
    response.request_focus();
    response.ctx.memory_mut(|memory| {
        memory.set_focus_lock_filter(response.id, renderer_egui::terminal_focus_lock_filter());
    });
}

/// pane 안에 떠 있는 버튼만 그린다. 터미널 레이아웃 커서와 가용 크기는 건드리지 않는다.
fn render_scroll_bottom_button(
    ui: &mut egui::Ui,
    bounds: egui::Rect,
    id: egui::Id,
    catalog: &i18n::Catalog,
) -> Option<egui::Response> {
    let available = bounds.intersect(ui.clip_rect()).shrink(6.0);
    if available.width() < 24.0 || available.height() < 28.0 {
        return None;
    }
    let width = available.width().min(176.0);
    let rect = egui::Rect::from_center_size(
        egui::pos2(available.center().x, available.bottom() - 14.0),
        egui::vec2(width, 28.0),
    );
    let label = catalog.t("workspace.menu.scroll_bottom", &[]);
    let display = if width < 112.0 {
        "↓".to_owned()
    } else {
        format!("↓ {label}")
    };
    let tokens = crate::ui::designall::tokens(ui.visuals());
    let mut overlay = ui.new_child(egui::UiBuilder::new().id_salt(id).max_rect(rect).layout(
        egui::Layout::centered_and_justified(egui::Direction::TopDown),
    ));
    overlay.set_clip_rect(available);
    let response = overlay.add(
        egui::Button::new(egui::RichText::new(display).size(12.0).color(tokens.text))
            .fill(tokens.input_background)
            .stroke(egui::Stroke::new(1.0, tokens.accent))
            .corner_radius(egui::CornerRadius::same(14))
            .truncate(),
    );
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), &label)
    });
    Some(response.on_hover_text(label))
}

fn terminal_primary_pointer_clicked(response: &egui::Response) -> bool {
    response.clicked_by(egui::PointerButton::Primary)
}

fn clipboard_terminal_paste_bytes(
    paths: Option<&[std::path::PathBuf]>,
    text_paste_bytes: impl FnOnce() -> Option<Vec<u8>>,
    shell_kind: crate::ui::file_tree::ShellKind,
    bracketed_paste: bool,
) -> Option<Vec<u8>> {
    if let Some(paths) = paths {
        Some(paths_insert_paste_bytes(paths, shell_kind, bracketed_paste))
    } else {
        text_paste_bytes()
    }
}

/// ⌘V press의 Event::Paste와 release 키 이벤트 사이 간격 상한 — 이 안이면 같은
/// 붙여넣기 제스처로 본다. 보통 탭의 press→release는 50~200ms. 너무 길면 별개의
/// 두 붙여넣기를 오인한다 — 메뉴 텍스트 붙여넣기 직후의 ⌘V 이미지 붙여넣기가
/// 스킵되는 구멍 (codex 리뷰 MEDIUM, 3s→600ms 축소). 600ms 이상 키를 누르고
/// 있는 경우는 OS 키 반복이 Event::Paste를 다시 보내 타임스탬프가 갱신된다.
const PASTE_GESTURE_WINDOW: std::time::Duration = std::time::Duration::from_millis(600);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClipboardPasteTrigger {
    /// AppKit local monitor가 winit/egui보다 먼저 본 신뢰 가능한 Command+V key-down.
    NativeKeyDown,
    /// egui가 전달한 플랫폼 shortcut. macOS에서는 V key-up fallback이다.
    EguiShortcut,
}

/// clipboard shortcut이 띄우는 이미지 paste 태스크를 건너뛸지 판정한다.
///
/// NativeKeyDown은 새 제스처의 시작이므로 이전 paste 이력 때문에 건너뛰지 않는다.
/// macOS key-up fallback만 같은 제스처의 native key-down 또는 press Event::Paste가
/// 최근 처리됐고 같은 프레임 text fallback도 없을 때 건너뛴다.
fn should_skip_paste_task(
    trigger: ClipboardPasteTrigger,
    has_text_fallback: bool,
    last_text_paste: Option<std::time::Instant>,
    last_native_paste: Option<std::time::Instant>,
) -> bool {
    trigger == ClipboardPasteTrigger::EguiShortcut
        && !has_text_fallback
        && [last_text_paste, last_native_paste]
            .into_iter()
            .flatten()
            .any(|at| at.elapsed() < PASTE_GESTURE_WINDOW)
}

fn is_clipboard_paste_shortcut(event: &egui::Event) -> bool {
    let egui::Event::Key {
        key: egui::Key::V,
        pressed,
        modifiers,
        ..
    } = event
    else {
        return false;
    };
    if cfg!(target_os = "macos") {
        // 이미지-only clipboard의 Cmd+V PRESS는 egui-winit이 Event::Paste/Key 없이
        // 소비한다. AppKit monitor의 native key-down이 주 경로이고, 이 release는 monitor
        // 설치 실패·포커스 경계 누락을 위한 fallback이다. 제스처 상태가 중복을 제거한다.
        !pressed && modifiers.command && !modifiers.ctrl
    } else {
        *pressed && modifiers.ctrl && modifiers.shift
    }
}

/// 셀 idx 아래의 단어(공백 구분 비어있지 않은 셀 연속) 범위를 [start, end]로 돌려준다.
/// 공백 위를 더블클릭하면 None. 더블클릭 단어 선택에 쓴다.
/// 단어가 URL이면 (뒤따르는 구두점 제거 후) 그 URL을 반환한다. http/https만 연다.
fn extract_url(word: &str) -> Option<&str> {
    // 머리의 여는 괄호/따옴표도 벗긴다 — "(https://…)" 꼴이 흔하다 (resolve_path_click 관례).
    let trimmed = word
        .trim_start_matches(|c: char| "\"'`([{<".contains(c))
        .trim_end_matches(|c: char| ".,;:!?)]}>\"'".contains(c));
    (trimmed.starts_with("http://") || trimmed.starts_with("https://")).then_some(trimmed)
}

fn bounded_path_word(word: &str) -> Result<String, WorkspaceIoErrorCode> {
    let bytes = word.len();
    if bytes == 0 || bytes > WORKSPACE_PATH_MAX_BYTES || word.as_bytes().contains(&0) {
        return Err(WorkspaceIoErrorCode::InvalidPath);
    }
    Ok(word.to_owned())
}

/// 터미널 텍스트가 가리키는 파일시스템 대상 (2026-07-14 사용자 요청).
#[derive(Debug, Clone, PartialEq)]
enum PathClick {
    /// 디렉터리 — 셸에 cd를 보낸다 (alt screen이 아닐 때만)
    Dir(std::path::PathBuf),
    /// 외부 프로그램으로 여는 파일 (OPENABLE_EXTS 허용목록)
    OpenFile(std::path::PathBuf),
}

/// 셀이 "내용"인가 — 공백·NUL은 아니고, wide char 뒤 자리 채움은 앞 글자의 일부다.
/// 단어 선택과 행 선택이 같은 판정을 써야 한글로 끝나는 경우가 갈리지 않는다.
fn cell_has_content(snapshot: &terminal::TerminalViewportSnapshot, idx: usize) -> bool {
    // wide 글자의 **뒷칸**은 글자의 일부라 내용이다. 반면 2칸 글자가 행 끝에 안 들어가
    // 다음 줄로 밀릴 때 남는 **행 끝 필러**는 같은 `wide_spacer` 비트를 쓰지만 이 행에는
    // 아무 글자도 없다 — 내용으로 세면 눈에 빈 행이 더블클릭에 강조된다(2026-08-18 리뷰).
    if snapshot.is_trailing_wide_spacer(idx) {
        return true;
    }
    snapshot
        .visible_cells
        .get(idx)
        .is_some_and(|cell| !cell.wide_spacer() && !cell.c.is_whitespace() && cell.c != '\0')
}

fn snapshot_has_visible_text(snapshot: &TerminalViewportSnapshot) -> bool {
    (0..snapshot.visible_cells.len()).any(|index| cell_has_content(snapshot, index))
}

/// 더블클릭이 잡는 **화면 행 전체** 범위 (2026-08-17 사용자 요청).
///
/// 스냅샷은 평면 그리드라 wrap 정보가 없다 — 접힌 논리 줄을 이어 붙일 방법이 없으므로
/// 단위는 "보이는 행 하나"다. 시작은 0열(앞 들여쓰기도 행의 일부), 끝은 마지막 내용
/// 셀이다. 끝의 빈 칸을 넣으면 선택 강조만 화면 끝까지 늘어나고 복사 결과는 어차피
/// 같다(`selection_text`가 행 끝 공백을 자른다).
///
/// 행이 통째로 비어 있으면 `None` — 빈 줄을 더블클릭해도 아무 일도 일어나지 않는다
/// (공백 위 단어 선택이 `None`이던 것과 같은 감각).
fn line_range_at(
    snapshot: &terminal::TerminalViewportSnapshot,
    idx: usize,
) -> Option<(usize, usize)> {
    let cols = snapshot.cols as usize;
    if cols == 0 {
        return None;
    }
    let base = (idx / cols) * cols;
    let last = (0..cols)
        .rev()
        .find(|offset| cell_has_content(snapshot, base + offset))?;
    Some((base, base + last))
}

fn word_range_at(
    snapshot: &terminal::TerminalViewportSnapshot,
    idx: usize,
) -> Option<(usize, usize)> {
    let cols = snapshot.cols as usize;
    if cols == 0 {
        return None;
    }
    let row = idx / cols;
    let col = idx % cols;
    let base = row * cols;
    // wide char(한글 등) 뒤의 자리 채움 셀은 c==' '지만 단어의 일부다 — 공백으로
    // 취급하면 "nant-성과분석.pdf"가 첫 한글에서 끊긴다 (2026-07-14). 판정은
    // `cell_has_content`가 행 선택과 공유한다.
    let is_word = |c: usize| -> bool { cell_has_content(snapshot, base + c) };
    if !is_word(col) {
        return None;
    }
    let mut start = col;
    while start > 0 && is_word(start - 1) {
        start -= 1;
    }
    let mut end = col;
    while end + 1 < cols && is_word(end + 1) {
        end += 1;
    }
    Some((base + start, base + end))
}

/// regex/휴리스틱 상태와 transcript 기반 활동(옵션2)을 병합한다. 승인/오류/완료는
/// transcript에 없는 '주의' 신호라 regex를 우선하고, 그 외(실행/대기/유휴/미보고)는
/// 더 정확한 transcript 활동으로 덮어쓴다.
fn merge_agent_status(
    regex: Option<runtime::SessionStatus>,
    activity: Option<crate::agent_transcript::AgentActivity>,
    needs_input: bool,
    turn_done: bool,
    hook_working: bool,
    needs_response: bool,
) -> Option<runtime::SessionStatus> {
    use crate::agent_transcript::AgentActivity;
    use runtime::SessionStatus as S;
    // 확정된 미응답 요청은 오래된 Working 레코드나 병렬 작업으로 해제하지 않는다.
    if needs_input {
        return Some(if needs_response {
            S::Waiting
        } else {
            S::NeedsApproval
        });
    }
    // 명시적 오류는 완료보다 우선 — Stop은 모든 턴 종료에 오므로 turn_done이 error를
    // 가리면 실패한 턴이 '완료(바이올렛)'로 위장된다(codex 리뷰).
    // 완료 직후 계획 선택창을 여는 CLI도 있으므로 현재 화면의 요청을 먼저 보인다.
    if matches!(regex, Some(S::Error | S::Waiting | S::NeedsApproval)) {
        return regex;
    }
    // Stop hook = 턴 완료. UserPromptSubmit/PreToolUse가 clear하므로 재개 시 즉시 해제.
    // transcript activity(Stop 직후 잠깐 Working으로 남음)보다 우선한다.
    if turn_done {
        return Some(S::Done);
    }
    if matches!(regex, Some(S::Done)) {
        return regex;
    }
    // hook "작업 중"(v32, cmux식): UserPromptSubmit/PreToolUse가 턴 경계에서 즉시 기록 —
    // transcript(1.5s 폴링 + 활성 전용)보다 빠르고 warm에서도 동작한다. 화면 regex의
    // 대기/승인(위)은 hook이 놓치는 프롬프트를 잡는 fallback이라 여전히 우선한다.
    if hook_working {
        return Some(S::Running);
    }
    match activity {
        Some(AgentActivity::Working) => Some(S::Running),
        Some(AgentActivity::Idle) => Some(S::Idle),
        None => regex,
    }
}

fn terminal_keyboard_input_allowed(
    text_edit_focused: bool,
    popup_open: bool,
    top_window_open: bool,
    terminal_refocus_pending: bool,
) -> bool {
    // TextEditState는 widget이 사라진 뒤 한 프레임 더 memory에 남을 수 있다. terminal
    // refocus가 명시적으로 대기 중이면 그 stale 상태는 무시해야 첫 글자가 빠지지 않는다.
    !popup_open && !top_window_open && (terminal_refocus_pending || !text_edit_focused)
}

/// `frame_has_active_preedit`은 이번 프레임 raw 입력에 비어 있지 않은 preedit이 있다는
/// 뜻이다. 조합이 시작되는 프레임에는 egui 공식 소유권도 `self.preedit`도 아직 없어
/// 나머지 두 근거가 모두 false다. 그 프레임을 거절하면 `self.preedit`이 영영 안 차고,
/// renderer는 뒤이은 **입력 없는 프레임**에서 조합이 끝난 줄 알고 포커스를 복구하다가
/// IME를 강제 중단한다(자모 분리). 관문은 `terminal_keyboard_active`와 TextEdit·팝업
/// 배제 조건이 그대로 지키므로 다른 입력창의 조합을 가로채지 않는다.
fn terminal_accepts_ime_events(
    terminal_keyboard_active: bool,
    owns_ime_events: bool,
    preedit_active: bool,
    frame_has_active_preedit: bool,
    text_edit_focused: bool,
    popup_open: bool,
    blocking_window_open: bool,
) -> bool {
    terminal_keyboard_active
        && !text_edit_focused
        && !popup_open
        && !blocking_window_open
        && (owns_ime_events || preedit_active || frame_has_active_preedit)
}

fn terminal_should_copy_selection(
    native_copy_requested: bool,
    events: &[egui::Event],
    terminal_keyboard_active: bool,
    copy_suppressed: bool,
    has_selection: bool,
) -> bool {
    terminal_keyboard_active
        && !copy_suppressed
        && has_selection
        && (native_copy_requested
            || events
                .iter()
                .any(|event| matches!(event, egui::Event::Copy)))
}

/// Agents and the diff review panel are floating but non-modal. A terminal
/// click must be able to reclaim focus while they remain open;
/// confirmation/error windows continue to block terminal input as before.
pub(crate) fn is_blocking_terminal_window(layer: &egui::LayerId) -> bool {
    layer.order == egui::Order::Middle
        && layer.id != crate::ui::agent_sessions::agents_window_id()
        && layer.id != crate::ui::diff_panel::diff_window_id()
}

/// Pending focus가 있으면 runtime snapshot의 이전 focused pane 대신 그것이 유일한 입력
/// 대상이다. 이 규칙이 없으면 pane 전환 직후 문장부호가 이전 pane에 들어가거나 유실된다.
fn terminal_input_owner(
    pane_id: &runtime::MuxPaneId,
    runtime_focused: bool,
    pending_focus: Option<&runtime::MuxPaneId>,
) -> bool {
    match pending_focus {
        Some(pending) => pending == pane_id,
        None => runtime_focused,
    }
}

/// Runtime focus snapshots normally arm the newly focused pane for native keyboard ownership.
/// An explicit App-side focus intent is newer, however, and must remain the exclusive input fence
/// while its persisted pane is still materializing.
fn sync_runtime_focus_intent(
    last_runtime_focus: &mut Option<runtime::MuxPaneId>,
    pending_focus: &mut Option<runtime::MuxPaneId>,
    explicit_pending_focus: Option<&runtime::MuxPaneId>,
    next_runtime_focus: Option<runtime::MuxPaneId>,
) -> bool {
    if *last_runtime_focus == next_runtime_focus {
        return false;
    }
    *last_runtime_focus = next_runtime_focus.clone();
    if explicit_pending_focus.is_none() {
        *pending_focus = next_runtime_focus;
    }
    true
}

/// 후보에서 같은 문자 하나만 제거한다. 연속으로 같은 키를 누른 횟수를 보존하려면
/// 집합이 아니라 이 one-for-one 소비가 필요하다.
fn consume_ime_terminator_char(candidates: &mut Vec<char>, character: char) -> bool {
    if let Some(index) = candidates
        .iter()
        .position(|candidate| *candidate == character)
    {
        candidates.remove(index);
        true
    } else {
        false
    }
}

fn paired_old_ime_text_echo_index(
    events: &[egui::Event],
    commit_index: usize,
    committed: &str,
    native_key_monitor_available: bool,
) -> Option<usize> {
    let next = commit_index.checked_add(1)?;
    let paired_key = committed
        .chars()
        .last()
        .filter(char::is_ascii)
        .is_some_and(|character| {
            events
                .get(next)
                .and_then(input_mapper::ime_terminator_key_char)
                == Some(character)
        });
    let echo_index = next + usize::from(paired_key);
    let egui::Event::Text(text) = events.get(echo_index)? else {
        return None;
    };
    if text.is_empty() || !committed.ends_with(text) {
        return None;
    }
    // A full Text copy or a matching Key+Text pair is attributable to the
    // old Commit on every platform. A bare suffix Text is ambiguous without
    // AppKit's physical-key observations, so keep it on other platforms.
    (text == committed || paired_key || native_key_monitor_available).then_some(echo_index)
}

#[derive(Debug, PartialEq, Eq)]
struct ImeTextReconciliation {
    /// raw event와 같은 길이. Text/Commit 위치에는 중복을 제거한 최종 문자열이 있고,
    /// 다른 이벤트 위치에는 None이 있다.
    event_text: Vec<Option<String>>,
    /// IME가 Text/Commit을 생략한 실제 key-down만 event batch 뒤에 보낸다.
    fallback_bytes: Vec<u8>,
    /// AppKit이 Return 다음에 관찰한 실제 key-down. 제출 뒤에만 전달한다.
    fallback_after_submit_bytes: Vec<u8>,
}

fn settle_ime_fallback(
    pending: &mut Vec<u8>,
    deferred: &mut Option<PendingImeSubmit>,
    had_deferred_before_frame: bool,
    insert_at: Option<usize>,
    previous_deferred_bytes: usize,
    following_insert_at: Option<usize>,
    reconciliation: &ImeTextReconciliation,
) {
    let mut following_insert_at = following_insert_at;
    if had_deferred_before_frame {
        // Every physical key in this frame follows an earlier Enter. Place
        // recovered keys before later raw keys in this frame, not after them.
        let bytes = reconciliation
            .fallback_bytes
            .iter()
            .chain(&reconciliation.fallback_after_submit_bytes)
            .copied();
        if let Some(deferred) = deferred {
            deferred
                .after_submit
                .splice(previous_deferred_bytes..previous_deferred_bytes, bytes);
        } else if let Some(insert_at) = following_insert_at {
            pending.splice(insert_at..insert_at, bytes);
        } else {
            pending.extend(bytes);
        }
        return;
    } else if let Some(insert_at) = insert_at {
        pending.splice(
            insert_at..insert_at,
            reconciliation.fallback_bytes.iter().copied(),
        );
        if let Some(after) = &mut following_insert_at
            && insert_at <= *after
        {
            *after += reconciliation.fallback_bytes.len();
        }
    } else if let Some(deferred) = deferred {
        deferred
            .before_submit
            .extend(&reconciliation.fallback_bytes);
    } else {
        pending.extend(&reconciliation.fallback_bytes);
    }
    if let Some(insert_at) = following_insert_at {
        pending.splice(
            insert_at..insert_at,
            reconciliation.fallback_after_submit_bytes.iter().copied(),
        );
    } else if let Some(deferred) = deferred {
        let insert_at = usize::from(!deferred.after_submit.is_empty());
        deferred.after_submit.splice(
            insert_at..insert_at,
            reconciliation.fallback_after_submit_bytes.iter().copied(),
        );
    } else {
        pending.extend(&reconciliation.fallback_after_submit_bytes);
    }
}

/// 한 raw input batch의 IME Commit/Text/fallback을 하나의 물리 키 원장으로 조정한다.
///
/// macOS Korean IME는 한 번 누른 Space/Comma를 `Ime::Commit`에 포함한 직후 일반
/// `Text`로 다시 보낼 수 있다. 반대로 조합을 확정한 키의 Text를 완전히 생략하기도
/// 한다. AppKit local monitor와 egui Key는 같은 물리 키의 두 관측값이므로 합산하지
/// 않고 문자별 최대 개수로 병합한다. 그 개수를 넘는 Commit/Text 문자만 버리고,
/// 모자란 개수는 IME가 관여한 batch에서만 fallback으로 보낸다.
fn reconcile_ime_text_events(
    key_downs: &[crate::native_key_monitor::NativePrintableKeyDown],
    events: &[egui::Event],
    preedit_active: bool,
    submit_pending: bool,
) -> ImeTextReconciliation {
    let ime_involved = preedit_active
        || submit_pending
        || events.iter().any(|event| {
            matches!(event, egui::Event::Ime(egui::ImeEvent::Commit(_)))
                || matches!(
                    event,
                    egui::Event::Ime(egui::ImeEvent::Preedit { text, .. }) if !text.is_empty()
                )
        });

    let native_candidates: Vec<(char, bool)> = key_downs
        .iter()
        .map(|key_down| (key_down.character, submit_pending || key_down.after_submit))
        .collect();
    let mut physical_candidates = native_candidates.clone();
    if ime_involved {
        // AppKit과 egui Key는 같은 key-down을 보는 두 경로다. 먼저 native 후보와
        // one-for-one으로 짝지어, native가 놓친 egui 후보만 원장에 추가한다.
        let mut unmatched_native: Vec<char> = native_candidates
            .iter()
            .map(|(character, _)| *character)
            .collect();
        let mut after_submit = submit_pending;
        for event in events {
            if matches!(
                event,
                egui::Event::Key {
                    key: egui::Key::Enter,
                    pressed: true,
                    ..
                }
            ) {
                after_submit = true;
            }
            if let Some(character) = input_mapper::ime_terminator_key_char(event)
                && !consume_ime_terminator_char(&mut unmatched_native, character)
            {
                physical_candidates.push((character, after_submit));
            }
        }
    }

    let constrained_characters: HashSet<char> = physical_candidates
        .iter()
        .map(|(character, _)| *character)
        .collect();
    let mut unclaimed_physical = physical_candidates;
    let mut event_text = Vec::with_capacity(events.len());

    let mut after_submit = submit_pending;
    for event in events {
        if matches!(
            event,
            egui::Event::Key {
                key: egui::Key::Enter,
                pressed: true,
                ..
            }
        ) {
            after_submit = true;
        }
        let text = match event {
            egui::Event::Text(text) | egui::Event::Ime(egui::ImeEvent::Commit(text)) => text,
            _ => {
                event_text.push(None);
                continue;
            }
        };
        // Commit belongs to the composition before Return even when AppKit
        // reports that Commit after the Return key event.
        let text_after_submit =
            after_submit && !matches!(event, egui::Event::Ime(egui::ImeEvent::Commit(_)));

        let mut filtered = String::with_capacity(text.len());
        for character in text.chars() {
            if !constrained_characters.contains(&character) {
                filtered.push(character);
                continue;
            }
            let Some(position) = unclaimed_physical.iter().position(|(candidate, side)| {
                *candidate == character && *side == text_after_submit
            }) else {
                // A Text event on the other side of Return can echo a key
                // swallowed by the IME. Leave that physical key for its own
                // before/after fallback slot instead of moving it across CR.
                continue;
            };
            if ime_involved {
                // Restore only earlier physical keys on this side of Return.
                // Opposite-side keys remain for the final fallback slots.
                let mut position = position;
                let mut index = 0;
                while index < position {
                    if unclaimed_physical[index].1 == text_after_submit {
                        filtered.push(unclaimed_physical.remove(index).0);
                        position -= 1;
                    } else {
                        index += 1;
                    }
                }
                unclaimed_physical.remove(position);
            } else {
                unclaimed_physical.remove(position);
            }
            filtered.push(character);
        }
        event_text.push(Some(filtered));
    }

    let mut fallback_bytes = Vec::new();
    let mut fallback_after_submit_bytes = Vec::new();
    if ime_involved {
        for (character, after_submit) in unclaimed_physical {
            if after_submit {
                fallback_after_submit_bytes.push(character as u8);
            } else {
                fallback_bytes.push(character as u8);
            }
        }
    }

    ImeTextReconciliation {
        event_text,
        fallback_bytes,
        fallback_after_submit_bytes,
    }
}

/// 상태 → tab 제목 아이콘 (PR-12).
/// 사이드바 세션 요약 — 화면의 마지막 비어있지 않은 행 (≤48자, 2026-07-05).
fn last_line_summary(snapshot: &TerminalViewportSnapshot) -> String {
    let cols = snapshot.cols as usize;
    if cols == 0 {
        return String::new();
    }
    for row in (0..snapshot.rows as usize).rev() {
        let mut line = String::new();
        let mut count = 0;
        let mut non_whitespace_end = 0;
        for index in row * cols..(row + 1) * cols {
            let cell = &snapshot.visible_cells[index];
            if cell.wide_spacer() {
                continue;
            }
            let mut scalar = [0; 4];
            let text = match snapshot.cell_grapheme(index) {
                Some(text) => text,
                None => cell.c.encode_utf8(&mut scalar),
            };
            let whitespace = text.chars().all(char::is_whitespace);
            if line.is_empty() && whitespace {
                continue;
            }
            let chars = text.chars().count();
            // Keep the prior48-scalar ceiling without splitting a cell's cluster.
            if count + chars > 48 {
                if line.is_empty() {
                    return "…".into();
                }
                break;
            }
            line.push_str(text);
            count += chars;
            if !whitespace {
                non_whitespace_end = line.len();
            }
        }
        line.truncate(non_whitespace_end);
        if !line.is_empty() {
            return line;
        }
    }
    String::new()
}

fn mux_sessions(snapshot: &MuxSnapshot) -> HashSet<SessionId> {
    snapshot
        .tabs
        .iter()
        .flat_map(|tab| &tab.panes)
        .filter_map(|pane| pane.session_id)
        .collect()
}

fn mux_split_ratio(snapshot: &MuxSnapshot, tab_id: &runtime::MuxTabId, path: &[u8]) -> Option<f32> {
    let mut node = &snapshot.tabs.iter().find(|tab| &tab.id == tab_id)?.layout;
    for part in path {
        match (node, part) {
            (LayoutNode::Split { first, .. }, 0) => node = first,
            (LayoutNode::Split { second, .. }, 1) => node = second,
            _ => return None,
        }
    }
    match node {
        LayoutNode::Split { ratio, .. } => Some(*ratio),
        LayoutNode::Pane(_) => None,
    }
}

/// 분할이 대상으로 삼을 pane: 포커스된 pane이 있으면 그것, 없으면 활성 탭(없으면 첫 탭)의
/// 첫 pane. pane이 하나도 없으면 None(빈 워크스페이스 — 분할할 게 없다).
fn split_target_pane(snapshot: &MuxSnapshot) -> Option<runtime::MuxPaneId> {
    if let Some(pane) = &snapshot.focused_pane {
        return Some(pane.clone());
    }
    let tab = snapshot
        .active_tab
        .as_ref()
        .and_then(|id| snapshot.tabs.iter().find(|tab| &tab.id == id))
        .or_else(|| snapshot.tabs.first());
    tab.and_then(|tab| tab.panes.first())
        .map(|pane| pane.id.clone())
}

/// pane 강조 플래시 지속 시간 — 입력요청·작업완료 시 pane 전체 테두리를 이만큼
/// 포인트색으로 그리고 페이드아웃한다. 탑라인(포커스 지속 표시)은 이와 무관하다.
const PANE_FLASH: std::time::Duration = std::time::Duration::from_secs(2);
/// 터미널 선택(포커스 이동) 시 pane 테두리 강조 지속. 원래 2초였는데(2026-07-23
/// 사용자 요청) 색이 오래 남는 느낌이라 줄였다 — 1.5초도 길어 1.2초로(2026-08-08 사용자).
///
/// 알림(`PANE_FLASH`)보다 짧은 건 의도다 — 포커스 이동은 사용자가 방금 자기 손으로
/// 한 행동이라 확인 신호면 충분하지만, 입력요청·작업완료는 놓치면 안 되는 알림이다.
const FOCUS_FLASH: std::time::Duration = std::time::Duration::from_millis(1_200);

/// pane 전체 플래시를 유발하는 상태: 입력요청(Waiting/NeedsApproval)·작업종료(Done/Error).
/// Running(작업 중)·Idle(쉬는 중)은 제외 — 주목이 필요한 순간만 번쩍인다.
fn is_flash_status(status: SessionStatus) -> bool {
    matches!(
        status,
        SessionStatus::Waiting
            | SessionStatus::NeedsApproval
            | SessionStatus::Done
            | SessionStatus::Error
    )
}

fn visible_mux_sessions(snapshot: &MuxSnapshot) -> HashSet<SessionId> {
    snapshot
        .active_tab
        .as_ref()
        .and_then(|active| snapshot.tabs.iter().find(|tab| &tab.id == active))
        .into_iter()
        .flat_map(|tab| &tab.panes)
        .filter_map(|pane| pane.session_id)
        .collect()
}

fn visible_split_contains_session(snapshot: &MuxSnapshot, session: SessionId) -> bool {
    snapshot
        .active_tab
        .as_ref()
        .and_then(|active| snapshot.tabs.iter().find(|tab| &tab.id == active))
        .is_some_and(|tab| {
            matches!(&tab.layout, LayoutNode::Split { .. })
                && tab
                    .panes
                    .iter()
                    .any(|pane| pane.session_id == Some(session))
        })
}

#[cfg(test)]
mod tests {
    #[test]
    fn pr2_selected_text_keeps_captured_owner_and_bounded_no_submit_intent() {
        let mut ui = super::WorkspaceUi::new();
        let id = runtime::SessionId(7);
        let old = crate::agent_detect::AgentExecutionIdentity::fixture(
            crate::agent_detect::AgentKind::Claude,
            1,
        );
        let new = crate::agent_detect::AgentExecutionIdentity::fixture(
            crate::agent_detect::AgentKind::Claude,
            2,
        );
        ui.set_agent_executions(std::collections::HashMap::from([(id, old)]));
        ui.dispatch_agent_prompt(vec![(id, old)], "explain this:\nselected text");
        let intent = ui.take_selected_agent_prompt().unwrap();
        assert_eq!(intent.session, id);
        assert_eq!(intent.execution, old);
        assert_eq!(intent.prompt.as_ref(), "explain this:\nselected text");
        assert!(
            ui.take_protocol_intent().is_none(),
            "leaf never emits unguarded WriteInput"
        );
        ui.set_agent_executions(std::collections::HashMap::from([(id, new)]));
        ui.dispatch_agent_prompt(vec![(id, old)], "stale");
        assert!(ui.take_selected_agent_prompt().is_none());
        ui.dispatch_agent_prompt(vec![(id, new); 33], "bounded");
        assert!(ui.take_selected_agent_prompt().is_none());
        assert!(ui.protocol_request_lost);
    }

    use super::*;
    use runtime::{MuxPaneId, MuxTabId, PaneSnapshot, TabSnapshot};
    use terminal::{CursorShape, CursorSnapshot, TerminalCell};

    #[path = "ime_input_harness.rs"]
    mod ime_input_harness;

    #[test]
    fn 응답대기_최신_대기는_작업중_기록으로_지우지_않는다() {
        use crate::agent_transcript::AgentActivity;
        use runtime::SessionStatus as S;
        assert_eq!(
            merge_agent_status(
                Some(S::Idle),
                Some(AgentActivity::Working),
                true,
                false,
                true,
                false
            ),
            Some(S::NeedsApproval)
        );
        assert_eq!(
            merge_agent_status(Some(S::Waiting), None, false, false, false, false),
            Some(S::Waiting)
        );
    }
    #[test]
    fn pane_render_output_merge는_document_drop_경로_순서를_보존한다() {
        let mut merged = PaneRenderOutput {
            document_drop_paths: vec![PathBuf::from("/tmp/first.rs")],
            ..Default::default()
        };
        merged.merge(PaneRenderOutput {
            document_drop_paths: vec![
                PathBuf::from("/tmp/second.json"),
                PathBuf::from("/tmp/third.yaml"),
            ],
            ..Default::default()
        });

        assert_eq!(
            merged.document_drop_paths,
            vec![
                PathBuf::from("/tmp/first.rs"),
                PathBuf::from("/tmp/second.json"),
                PathBuf::from("/tmp/third.yaml"),
            ]
        );
    }

    /// hook "작업 중"(v32) 병합 우선순위: 확정 상태(승인대기/오류/완료/화면 대기)가
    /// 이기고, 그 외엔 hook working이 transcript(지연·활성 전용)보다 우선한다.
    #[test]
    fn merge_agent_status_hook_working_우선순위() {
        use crate::agent_transcript::AgentActivity;
        use runtime::SessionStatus as S;
        // hook working 단독 → Running (transcript 없음/유휴여도)
        assert_eq!(
            merge_agent_status(None, None, false, false, true, false),
            Some(S::Running)
        );
        assert_eq!(
            merge_agent_status(None, Some(AgentActivity::Idle), false, false, true, false),
            Some(S::Running)
        );
        // 확정 상태가 우선: needs_input / regex Error / turn_done / 화면 대기(regex)
        assert_eq!(
            merge_agent_status(None, None, true, false, true, false),
            Some(S::NeedsApproval)
        );
        assert_eq!(
            merge_agent_status(Some(S::Error), None, false, false, true, false),
            Some(S::Error)
        );
        assert_eq!(
            merge_agent_status(None, None, false, true, true, false),
            Some(S::Done)
        );
        assert_eq!(
            merge_agent_status(Some(S::Waiting), None, false, false, true, false),
            Some(S::Waiting)
        );
        // hook working 없으면 기존과 동일(transcript 폴백)
        assert_eq!(
            merge_agent_status(None, Some(AgentActivity::Idle), false, false, false, false),
            Some(S::Idle)
        );
    }

    #[test]
    fn merge_agent_status_완료_후_선택창은_응답을_요청한다() {
        use runtime::SessionStatus as S;
        for status in [S::Waiting, S::NeedsApproval] {
            assert_eq!(
                merge_agent_status(Some(status), None, false, true, false, false),
                Some(status)
            );
        }
    }

    fn drain_protocol(ui: &mut WorkspaceUi) -> Vec<RuntimeCommand> {
        let mut commands = Vec::new();
        while let Some(intent) = ui.take_protocol_intent() {
            let operation = intent.operation();
            let generation = intent.generation();
            commands.push(intent.into_command());
            ui.complete_protocol(WorkspaceProtocolCompletion {
                operation,
                generation,
                result: Ok(()),
            });
        }
        commands
    }

    #[test]
    fn popup_review_pending_session_close_blocks_global_keys_before_modal_render() {
        let ctx = egui::Context::default();
        let catalog = i18n::Catalog::load("en-US").unwrap();
        let config = crate::config::ShortcutsConfig::default();
        let mut workspace = WorkspaceUi::new();
        workspace.mux = Some(mux(
            "tab",
            vec![tab(
                "tab",
                vec![pane("p", SessionId(7))],
                LayoutNode::Pane(pane_id("p")),
            )],
            "p",
        ));
        workspace.request_close_pane(pane_id("p"));
        assert!(workspace.has_pending_close_confirmation());
        let binding = crate::shortcuts::effective_binding(
            &config,
            crate::shortcuts::ShortcutAction::ClosePane,
        )
        .unwrap();
        ctx.run_ui(
            egui::RawInput {
                events: vec![egui::Event::Key {
                    key: binding.logical_key,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: binding.modifiers,
                }],
                ..Default::default()
            },
            |ui| {
                super::super::popup::set_pending_modal(
                    ui.ctx(),
                    workspace.has_pending_close_confirmation(),
                );
                assert!(crate::shortcuts::take_triggered_action(ui.ctx(), &config).is_none());
                workspace.close_confirm_dialog(ui.ctx(), &catalog);
                assert!(workspace.has_pending_close_confirmation());
            },
        )
        .drop_without_applying_deltas();
        workspace.mux = None;
        assert!(
            !workspace.has_pending_close_confirmation(),
            "a disappeared pane must not block all later input"
        );
    }

    #[test]
    fn popup_review_actual_search_area_keeps_enter_shift_enter_and_escape() {
        let mut workspace = WorkspaceUi::new();
        workspace.search = Some(TerminalSearch {
            session: SessionId(7),
            query: "ready".into(),
            requested: Some("ready".into()),
            delivery: None,
            matches: (0..3)
                .map(|line_from_bottom| terminal::ScrollbackMatch {
                    line_from_bottom,
                    col_start: 0,
                    col_end: 1,
                })
                .collect(),
            total_lines: 10,
            capped: false,
            current: 0,
            focus_input: true,
            input_id: None,
            scroll_to_current: false,
        });
        let catalog = catalog();
        let view = snapshot("ready");
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, ws: &mut WorkspaceUi| {
                ws.handle_terminal_search_keys(ui.ctx());
                ws.render_terminal_search(
                    ui,
                    SessionId(7),
                    egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0)),
                    egui::Pos2::ZERO,
                    egui::vec2(8.0, 16.0),
                    &view,
                    &catalog,
                );
            },
            workspace,
        );
        harness.run();
        harness.key_press(egui::Key::Enter);
        harness.run();
        assert_eq!(harness.state().search.as_ref().unwrap().current, 1);
        harness.key_press_modifiers(egui::Modifiers::SHIFT, egui::Key::Enter);
        harness.run();
        assert_eq!(harness.state().search.as_ref().unwrap().current, 0);
        harness.key_press(egui::Key::Escape);
        harness.run();
        assert!(harness.state().search.is_none());
    }

    #[test]
    fn popup_review_pending_conflict_blocks_first_frame_text_and_enter() {
        let session = SessionId(7);
        let mut workspace = WorkspaceUi::new();
        workspace.mux = Some(mux(
            "primary",
            vec![tab(
                "primary",
                vec![pane("pane", session)],
                LayoutNode::Pane(pane_id("pane")),
            )],
            "pane",
        ));
        workspace.last_focused_pane = Some(pane_id("pane"));
        workspace.pending_focus = Some(pane_id("pane"));
        workspace.sessions.entry(session).or_default().snapshot = Some(snapshot("ready"));
        let catalog = catalog();
        let config = TerminalConfig::default();
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, state: &mut (WorkspaceUi, bool)| {
                // Mirrors App's terminal-before-document-modal order.
                super::super::popup::set_pending_modal(ui.ctx(), state.1);
                state.0.show_with_input(ui, &config, &[], &catalog, true);
                if state.1 {
                    let _ = super::super::document_dialogs::conflict(
                        ui.ctx(),
                        egui::Id::new("document_conflict_confirmation"),
                        egui::Id::new("saved"),
                        "saved.md",
                        0,
                        &catalog,
                    );
                }
            },
            (workspace, false),
        );
        harness.run();
        drain_protocol(&mut harness.state_mut().0);
        harness.event(egui::Event::Text("baseline".into()));
        harness.run();
        assert!(
            drain_protocol(&mut harness.state_mut().0)
                .iter()
                .any(|cmd| matches!(cmd, RuntimeCommand::WriteInput { .. }))
        );
        harness.state_mut().1 = true;
        harness.input_mut().events.extend([
            egui::Event::Text("race".into()),
            egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            },
        ]);
        harness.run();
        assert!(
            !drain_protocol(&mut harness.state_mut().0)
                .iter()
                .any(|cmd| matches!(cmd, RuntimeCommand::WriteInput { .. })),
            "pending conflict must fence input before its first rendering"
        );
    }

    #[test]
    fn scroll_bottom_action은_대상_선택과_잔여_스크롤을_해제한다() {
        let mut ui = WorkspaceUi::new();
        let session = SessionId(7);
        ui.selection = Some((session, 1, 8));
        ui.scroll_residual = 0.75;
        ui.drag_autoscroll_residual = 0.5;
        ui.scroll_session_to_bottom(session);
        assert!(
            ui.selection.is_none(),
            "선택 freeze가 최신 화면 복귀를 막으면 안 됨"
        );
        assert_eq!(ui.scroll_residual, 0.0);
        assert_eq!(ui.drag_autoscroll_residual, 0.0);
        let commands = drain_protocol(&mut ui);
        assert!(
            matches!(commands.as_slice(), [RuntimeCommand::ScrollToBottom { session: id }] if *id == session)
        );
        // 다른 pane의 선택은 보존하며 포커스된 세션으로 대상을 바꾸지 않는다.
        ui.selection = Some((SessionId(9), 2, 5));
        ui.scroll_session_to_bottom(session);
        assert_eq!(ui.selection, Some((SessionId(9), 2, 5)));
        let commands = drain_protocol(&mut ui);
        assert!(
            matches!(commands.as_slice(), [RuntimeCommand::ScrollToBottom { session: id }] if *id == session)
        );
    }

    #[test]
    fn search_request는_큐_포화_뒤_최신_검색만_재전송한다() {
        let mut ui = WorkspaceUi::new();
        let session = SessionId(7);
        ui.open_search_for_session(session);
        ui.search.as_mut().unwrap().query = "api-key".to_owned();
        for _ in 0..WORKSPACE_PROTOCOL_CAP {
            ui.queue_protocol_intent(RuntimeCommand::FocusPane { pane: pane_id("p") })
                .unwrap();
        }
        ui.submit_terminal_search();
        assert_eq!(ui.search.as_ref().unwrap().requested, None);
        drain_protocol(&mut ui);
        ui.search.as_mut().unwrap().query = "api-key-new".to_owned();
        ui.submit_terminal_search();
        ui.submit_terminal_search();
        let commands = drain_protocol(&mut ui);
        assert!(
            matches!(commands.as_slice(), [RuntimeCommand::SearchScrollback { session: target, query, .. }] if *target == session && query == "api-key-new")
        );
        ui.submit_terminal_search();
        assert!(drain_protocol(&mut ui).is_empty());
        ui.search.as_mut().unwrap().query = "delivery-retry".to_owned();
        ui.submit_terminal_search();
        let intent = ui.take_protocol_intent().unwrap();
        ui.complete_protocol(WorkspaceProtocolCompletion {
            operation: intent.operation(),
            generation: intent.generation(),
            result: Err(WorkspaceProtocolErrorCode::Busy),
        });
        assert_eq!(ui.search.as_ref().unwrap().requested, None);
        ui.submit_terminal_search();
        let commands = drain_protocol(&mut ui);
        assert!(
            matches!(commands.as_slice(), [RuntimeCommand::SearchScrollback { query, .. }] if query == "delivery-retry")
        );
        ui.search.as_mut().unwrap().query = "x".repeat(WORKSPACE_PROTOCOL_QUERY_MAX_BYTES + 1);
        ui.submit_terminal_search();
        let search = ui.search.as_ref().unwrap();
        assert_eq!(search.requested.as_deref(), Some(search.query.as_str()));
        assert!(drain_protocol(&mut ui).is_empty());
    }

    fn take_error_message(ui: &mut WorkspaceUi, catalog: &i18n::Catalog) -> Option<String> {
        ui.take_error_notice(catalog).map(|notice| notice.message)
    }

    fn split_mux_snapshot(ratio: f32) -> Arc<MuxSnapshot> {
        mux(
            "t",
            vec![tab(
                "t",
                vec![pane("left", SessionId(41)), pane("right", SessionId(42))],
                LayoutNode::Split {
                    direction: SplitDirection::Horizontal,
                    ratio,
                    first: Box::new(LayoutNode::Pane(pane_id("left"))),
                    second: Box::new(LayoutNode::Pane(pane_id("right"))),
                },
            )],
            "left",
        )
    }

    #[test]
    fn pane_drag_stable_state_does_not_enqueue_resize_until_matching_ack() {
        let mut ui = WorkspaceUi::new();
        let ctx = egui::Context::default();
        ui.handle_events(
            &[RuntimeEvent::MuxUpdated {
                snapshot: split_mux_snapshot(0.5),
            }],
            &catalog(),
        );
        ui.sent_sizes.insert(SessionId(41), (80, 24));
        ui.sent_sizes.insert(SessionId(42), (80, 24));
        assert!(ui.pending_resize_target.is_empty());

        ui.begin_split_drag(tab_id("t"), Vec::new(), 0.72);
        for (pass, cols) in [(10, 90), (11, 100), (12, 110)] {
            ui.stage_terminal_resize_for_pass(pass, false, SessionId(41), cols, 24);
            ui.stage_terminal_resize_for_pass(pass, false, SessionId(42), cols, 24);
            ui.flush_render_side_effects_for_pass(&ctx, pass, false);
            assert!(ui.protocol_intents.is_empty(), "active drag sent Resize");
        }

        ui.commit_split_drag(13);
        ui.flush_render_side_effects_for_pass(&ctx, 13, false);
        let commands = drain_protocol(&mut ui);
        assert_eq!(
            commands
                .iter()
                .filter(|command| matches!(command, RuntimeCommand::ResizeSplit { .. }))
                .count(),
            1
        );
        assert!(
            !commands
                .iter()
                .any(|command| matches!(command, RuntimeCommand::ResizeTracked { .. }))
        );

        ui.stage_terminal_resize_for_pass(14, false, SessionId(41), 110, 24);
        ui.flush_render_side_effects_for_pass(&ctx, 14, false);
        assert!(ui.protocol_intents.is_empty(), "pre-ACK Resize escaped");

        ui.handle_events(
            &[RuntimeEvent::MuxUpdated {
                snapshot: split_mux_snapshot(0.6),
            }],
            &catalog(),
        );
        assert!(ui.split_drag.is_some(), "old ratio is not an ACK");

        ui.handle_events(
            &[RuntimeEvent::MuxUpdated {
                snapshot: split_mux_snapshot(0.72),
            }],
            &catalog(),
        );
        assert!(ui.split_drag.is_none(), "matching ACK must settle preview");
        assert_eq!(
            ui.split_final_resize_sessions,
            HashSet::from([SessionId(41), SessionId(42)])
        );

        ui.stage_terminal_resize_for_pass(15, false, SessionId(41), 110, 24);
        ui.stage_terminal_resize_for_pass(15, false, SessionId(42), 50, 24);
        ui.flush_render_side_effects_for_pass(&ctx, 15, false);
        let commands = drain_protocol(&mut ui);
        assert_eq!(
            commands
                .iter()
                .filter(|command| matches!(command, RuntimeCommand::ResizeTracked { .. }))
                .count(),
            2,
            "matching ACK must produce one final Resize per changed session"
        );
    }

    #[test]
    fn committed_split_preview_survives_until_matching_mux_ack() {
        let mut ui = WorkspaceUi::new();
        let ctx = egui::Context::default();
        ui.handle_events(
            &[RuntimeEvent::MuxUpdated {
                snapshot: split_mux_snapshot(0.5),
            }],
            &catalog(),
        );
        ui.begin_split_drag(tab_id("t"), Vec::new(), 0.7);
        ui.commit_split_drag(20);
        ui.flush_render_side_effects_for_pass(&ctx, 20, false);
        assert!(matches!(
            drain_protocol(&mut ui).as_slice(),
            [RuntimeCommand::ResizeSplit { ratio, .. }] if (*ratio - 0.7).abs() < 0.0001
        ));
        assert!((ui.split_preview_ratio(&tab_id("t"), &[], 0.5) - 0.7).abs() < 0.0001);

        ui.handle_events(
            &[RuntimeEvent::MuxUpdated {
                snapshot: split_mux_snapshot(0.5),
            }],
            &catalog(),
        );
        assert!((ui.split_preview_ratio(&tab_id("t"), &[], 0.5) - 0.7).abs() < 0.0001);

        ui.handle_events(
            &[RuntimeEvent::MuxUpdated {
                snapshot: split_mux_snapshot(0.7),
            }],
            &catalog(),
        );
        assert!((ui.split_preview_ratio(&tab_id("t"), &[], 0.7) - 0.7).abs() < 0.0001);
        assert!(ui.split_drag.is_none());
    }

    #[test]
    fn late_first_ack_does_not_cancel_a_new_active_drag() {
        let mut ui = WorkspaceUi::new();
        let ctx = egui::Context::default();
        ui.handle_events(
            &[RuntimeEvent::MuxUpdated {
                snapshot: split_mux_snapshot(0.5),
            }],
            &catalog(),
        );
        ui.begin_split_drag(tab_id("t"), Vec::new(), 0.65);
        ui.commit_split_drag(30);
        ui.flush_render_side_effects_for_pass(&ctx, 30, false);
        drain_protocol(&mut ui);

        ui.begin_split_drag(tab_id("t"), Vec::new(), 0.8);
        ui.handle_events(
            &[RuntimeEvent::MuxUpdated {
                snapshot: split_mux_snapshot(0.65),
            }],
            &catalog(),
        );

        assert!((ui.split_preview_ratio(&tab_id("t"), &[], 0.65) - 0.8).abs() < 0.0001);
        assert!(ui.split_drag.is_some(), "late ACK cancelled the new drag");
    }

    #[test]
    fn discarded_or_sizing_pass_does_not_admit_split_protocol() {
        let mut ui = WorkspaceUi::new();
        let ctx = egui::Context::default();
        let session = SessionId(51);

        ui.stage_terminal_resize_for_pass(40, true, session, 80, 24);
        ui.flush_render_side_effects_for_pass(&ctx, 40, false);
        assert!(ui.protocol_intents.is_empty(), "sizing pass staged Resize");

        ui.stage_terminal_resize_for_pass(41, false, session, 80, 24);
        ui.flush_render_side_effects_for_pass(&ctx, 41, true);
        assert!(
            ui.protocol_intents.is_empty(),
            "discarded pass admitted Resize"
        );

        ui.stage_terminal_resize_for_pass(42, false, session, 80, 24);
        ui.flush_render_side_effects_for_pass(&ctx, 42, false);
        assert!(matches!(
            drain_protocol(&mut ui).as_slice(),
            [RuntimeCommand::ResizeTracked { session: sent, cols: 80, rows: 24, .. }] if *sent == session
        ));
    }

    #[test]
    fn final_pass_admission_requests_the_next_logic_repaint() {
        let mut ui = WorkspaceUi::new();
        let ctx = egui::Context::default();
        let repaint_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let repaint_count_for_callback = Arc::clone(&repaint_count);
        ctx.set_request_repaint_callback(move |_| {
            repaint_count_for_callback.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        });
        let pass = ctx.cumulative_pass_nr();
        let before = repaint_count.load(std::sync::atomic::Ordering::Relaxed);

        ui.stage_terminal_resize_for_pass(pass, false, SessionId(52), 80, 24);
        ui.flush_render_side_effects(&ctx);

        assert!(
            repaint_count.load(std::sync::atomic::Ordering::Relaxed) > before,
            "tail admission must wake the next logic drain"
        );
    }

    #[test]
    fn flushed_resize_staging_reuses_its_bounded_allocation() {
        let mut ui = WorkspaceUi::new();
        let ctx = egui::Context::default();
        for session in 1..=4 {
            ui.stage_terminal_resize_for_pass(43, false, SessionId(session), 80, 24);
        }
        let capacity = ui.staged_terminal_resizes.capacity();

        ui.flush_render_side_effects_for_pass(&ctx, 43, false);

        assert!(ui.staged_terminal_resizes.is_empty());
        assert!(
            ui.staged_terminal_resizes.capacity() >= capacity,
            "normal passes should retain the bounded staging allocation"
        );
    }

    #[test]
    fn failed_split_delivery_cancels_preview_and_unblocks_terminal_resize() {
        let mut ui = WorkspaceUi::new();
        let ctx = egui::Context::default();
        ui.handle_events(
            &[RuntimeEvent::MuxUpdated {
                snapshot: split_mux_snapshot(0.5),
            }],
            &catalog(),
        );
        ui.begin_split_drag(tab_id("t"), Vec::new(), 0.7);
        ui.commit_split_drag(44);
        ui.flush_render_side_effects_for_pass(&ctx, 44, false);
        let intent = ui.take_protocol_intent().expect("split commit");

        ui.complete_protocol(WorkspaceProtocolCompletion {
            operation: intent.operation(),
            generation: intent.generation(),
            result: Err(WorkspaceProtocolErrorCode::DeliveryFailed),
        });

        assert!(ui.split_drag.is_none(), "failed commit cannot await an ACK");
        ui.stage_terminal_resize_for_pass(45, false, SessionId(41), 100, 24);
        ui.flush_render_side_effects_for_pass(&ctx, 45, false);
        assert!(matches!(
            drain_protocol(&mut ui).as_slice(),
            [RuntimeCommand::ResizeTracked { session, cols: 100, rows: 24, .. }]
                if *session == SessionId(41)
        ));
    }

    #[test]
    fn matching_mux_ratio_is_not_an_ack_before_split_delivery_succeeds() {
        let mut ui = WorkspaceUi::new();
        let ctx = egui::Context::default();
        ui.handle_events(
            &[RuntimeEvent::MuxUpdated {
                snapshot: split_mux_snapshot(0.5),
            }],
            &catalog(),
        );
        ui.begin_split_drag(tab_id("t"), Vec::new(), 0.7);
        ui.commit_split_drag(46);
        ui.flush_render_side_effects_for_pass(&ctx, 46, false);

        ui.handle_events(
            &[RuntimeEvent::MuxUpdated {
                snapshot: split_mux_snapshot(0.7),
            }],
            &catalog(),
        );

        assert!(
            ui.split_drag.is_some(),
            "a locally queued command is not a delivered command"
        );
    }

    #[test]
    fn busy_split_delivery_waits_for_backoff_instead_of_hot_looping_or_cancelling() {
        let mut ui = WorkspaceUi::new();
        let ctx = egui::Context::default();
        ui.handle_events(
            &[RuntimeEvent::MuxUpdated {
                snapshot: split_mux_snapshot(0.5),
            }],
            &catalog(),
        );
        ui.begin_split_drag(tab_id("t"), Vec::new(), 0.7);
        ui.commit_split_drag(47);
        ui.flush_render_side_effects_for_pass(&ctx, 47, false);
        let intent = ui.take_protocol_intent().expect("split commit");
        ui.complete_protocol(WorkspaceProtocolCompletion {
            operation: intent.operation(),
            generation: intent.generation(),
            result: Err(WorkspaceProtocolErrorCode::Busy),
        });

        let transaction = ui
            .split_drag
            .as_ref()
            .expect("transient pressure keeps preview");
        assert!(transaction.pending_delivery.is_none());
        assert!(transaction.retry.retry_at.is_some());
        ui.stage_unadmitted_split_commit_for_pass(&ctx, 48, false);
        ui.flush_render_side_effects_for_pass(&ctx, 48, false);
        assert!(
            ui.protocol_intents.is_empty(),
            "same-frame retry is a hot loop"
        );
    }

    #[test]
    fn protocol_busy_backoff_is_exponential_and_stops_after_bounded_attempts() {
        let started = std::time::Instant::now();
        let mut retry = ProtocolRetryBackoff::default();

        for failure in 0..PROTOCOL_RETRY_LIMIT {
            assert!(retry.record_busy(started));
            let expected = PROTOCOL_RETRY_BASE.saturating_mul(1_u32 << u32::from(failure));
            assert!(matches!(
                retry.gate(started),
                ProtocolRetryGate::Wait(delay) if delay == expected
            ));
            assert!(matches!(
                retry.gate(started + expected),
                ProtocolRetryGate::Ready
            ));
        }

        assert!(!retry.record_busy(started));
        assert!(matches!(retry.gate(started), ProtocolRetryGate::Exhausted));
    }

    #[test]
    fn disappearing_split_path_cancels_transaction_without_waiting_forever() {
        let mut ui = WorkspaceUi::new();
        ui.handle_events(
            &[RuntimeEvent::MuxUpdated {
                snapshot: split_mux_snapshot(0.5),
            }],
            &catalog(),
        );
        ui.begin_split_drag(tab_id("t"), Vec::new(), 0.7);

        let collapsed = mux(
            "t",
            vec![tab(
                "t",
                vec![pane("left", SessionId(41))],
                LayoutNode::Pane(pane_id("left")),
            )],
            "left",
        );
        ui.handle_events(
            &[RuntimeEvent::MuxUpdated {
                snapshot: collapsed,
            }],
            &catalog(),
        );

        assert!(ui.split_drag.is_none());
    }

    #[test]
    fn hidden_input_cancels_active_split_drag_and_unblocks_resize() {
        let mut ui = WorkspaceUi::new();
        let ctx = egui::Context::default();
        ui.begin_split_drag(tab_id("t"), Vec::new(), 0.72);
        ui.cancel_active_split_drag(&ctx);
        assert!(ui.split_drag.is_none());
        ui.stage_terminal_resize_for_pass(45, false, SessionId(41), 100, 24);
        ui.flush_render_side_effects_for_pass(&ctx, 45, false);
        assert!(matches!(
            drain_protocol(&mut ui).as_slice(),
            [RuntimeCommand::ResizeTracked {
                session,
                cols: 100,
                rows: 24, .. }] if *session == SessionId(41)
        ));
    }

    #[test]
    fn hidden_input_releases_matching_drag_owner_without_revival_on_return() {
        let mut workspace = WorkspaceUi::new();
        workspace.mux = Some(split_mux_snapshot(0.5));
        for session in [SessionId(41), SessionId(42)] {
            workspace
                .sessions
                .entry(session)
                .or_default()
                .install_snapshot(shaped_snapshot(80, 24, "stable"));
        }
        let ctx = egui::Context::default();
        let handle_id = split_handle_id(&tab_id("t"), &[]);
        workspace.begin_split_drag(tab_id("t"), Vec::new(), 0.72);
        ctx.set_dragged_id(handle_id);

        workspace.update_hidden_with_native_input(&ctx, &[], &catalog(), || {
            crate::native_key_monitor::NativeKeyDownBatch::default()
        });

        assert_eq!(ctx.dragged_id(), None);
        assert!(workspace.split_drag.is_none());

        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(400.0, 240.0),
            )),
            events: vec![
                egui::Event::PointerMoved(egui::pos2(200.0, 120.0)),
                egui::Event::PointerButton {
                    pos: egui::pos2(200.0, 120.0),
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            ..egui::RawInput::default()
        };
        ctx.run_ui(input, |ui| {
            workspace.show_with_input(ui, &TerminalConfig::default(), &[], &catalog(), true);
        })
        .drop_without_applying_deltas();

        assert!(workspace.split_drag.is_none());
        assert!(
            !drain_protocol(&mut workspace)
                .iter()
                .any(|command| matches!(command, RuntimeCommand::ResizeSplit { .. })),
            "returning to input must not revive or commit the cancelled divider drag",
        );
        workspace.stage_terminal_resize_for_pass(45, false, SessionId(41), 100, 24);
        workspace.flush_render_side_effects_for_pass(&ctx, 45, false);
        assert!(drain_protocol(&mut workspace).iter().any(|command| {
            matches!(
                command,
                RuntimeCommand::ResizeTracked {
                    session: SessionId(41),
                    cols: 100,
                    rows: 24,
                    ..
                }
            )
        }));
    }

    #[test]
    fn focus_loss_cancels_active_split_drag_and_matching_egui_owner() {
        let mut workspace = WorkspaceUi::new();
        workspace.mux = Some(split_mux_snapshot(0.5));
        let ctx = egui::Context::default();
        let handle_id = split_handle_id(&tab_id("t"), &[]);
        workspace.begin_split_drag(tab_id("t"), Vec::new(), 0.72);
        let input = egui::RawInput {
            focused: false,
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(400.0, 240.0),
            )),
            ..egui::RawInput::default()
        };

        ctx.run_ui(input, |ui| {
            ui.ctx().set_dragged_id(handle_id);
            workspace.show_with_input(ui, &TerminalConfig::default(), &[], &catalog(), true);
        })
        .drop_without_applying_deltas();

        assert!(workspace.split_drag.is_none());
        assert_eq!(ctx.dragged_id(), None);
        assert!(
            !drain_protocol(&mut workspace)
                .iter()
                .any(|command| matches!(command, RuntimeCommand::ResizeSplit { .. })),
        );
    }

    #[test]
    fn split_drag_release_while_widget_absent_cancels_preview_and_unblocks_resize() {
        let mut workspace = WorkspaceUi::new();
        workspace.mux = Some(split_mux_snapshot(0.5));
        for session in [SessionId(41), SessionId(42)] {
            workspace
                .sessions
                .entry(session)
                .or_default()
                .install_snapshot(shaped_snapshot(80, 24, "stable"));
        }
        workspace.begin_split_drag(tab_id("t"), Vec::new(), 0.72);
        let ctx = egui::Context::default();
        let catalog = catalog();
        let config = TerminalConfig::default();

        ctx.run_ui(egui::RawInput::default(), |ui| {
            workspace.show_with_input(ui, &config, &[], &catalog, false);
        })
        .drop_without_applying_deltas();

        assert!(workspace.split_drag.is_none());
        assert!(
            !drain_protocol(&mut workspace)
                .iter()
                .any(|command| matches!(command, RuntimeCommand::ResizeSplit { .. })),
            "losing the divider widget must cancel, not commit, the preview",
        );
        workspace.stage_terminal_resize_for_pass(45, false, SessionId(41), 100, 24);
        workspace.flush_render_side_effects_for_pass(&ctx, 45, false);
        assert!(matches!(
            drain_protocol(&mut workspace).as_slice(),
            [RuntimeCommand::ResizeTracked {
                session,
                cols: 100,
                rows: 24, .. }] if *session == SessionId(41)
        ));
    }

    #[test]
    fn hidden_input_preserves_committed_split_ack_lifecycle() {
        let mut workspace = WorkspaceUi::new();
        let ctx = egui::Context::default();
        workspace.begin_split_drag(tab_id("t"), Vec::new(), 0.72);
        workspace.commit_split_drag(44);

        workspace.cancel_active_split_drag(&ctx);

        assert!(workspace.split_drag.as_ref().is_some_and(|transaction| {
            matches!(transaction.phase, SplitDragPhase::Committed { .. })
        }));
    }

    #[test]
    fn active_split_drag_reconciles_stop_tab_and_widget_owner_changes() {
        let mut workspace = WorkspaceUi::new();
        workspace.mux = Some(split_mux_snapshot(0.5));
        let ctx = egui::Context::default();
        let tab = tab_id("t");

        ctx.run_ui(egui::RawInput::default(), |ui| {
            workspace.begin_split_drag(tab.clone(), Vec::new(), 0.72);
            ui.ctx().set_dragged_id(split_handle_id(&tab, &[]));
            ui.ctx().stop_dragging();
            workspace.reconcile_active_split_drag(ui.ctx(), true, Some(&tab));
            assert!(workspace.split_drag.as_ref().is_some_and(|transaction| {
                matches!(transaction.phase, SplitDragPhase::Committed { .. })
            }));

            workspace.split_drag = None;
            workspace.begin_split_drag(tab.clone(), Vec::new(), 0.72);
            workspace.reconcile_active_split_drag(ui.ctx(), true, Some(&tab_id("other")));
            assert!(workspace.split_drag.is_none());

            workspace.begin_split_drag(tab.clone(), Vec::new(), 0.72);
            let another_widget = egui::Id::new("another_widget");
            ui.ctx().set_dragged_id(another_widget);
            workspace.reconcile_active_split_drag(ui.ctx(), true, Some(&tab));
            assert!(workspace.split_drag.is_none());
            assert_eq!(ui.ctx().dragged_id(), Some(another_widget));
        })
        .drop_without_applying_deltas();
    }

    #[test]
    fn final_resize_keeps_stable_snapshot_until_target_viewport_settles() {
        let started = std::time::Instant::now();
        let stable = shaped_snapshot(80, 24, "stable content");
        let first_target = shaped_snapshot(100, 30, "first target");
        let latest_target = shaped_snapshot(100, 30, "latest target");
        let mut view = SessionView::default();
        view.install_snapshot(Arc::clone(&stable));
        let stable_generation = view.snapshot_gen;
        view.arm_resize_presentation(100, 30, started);

        view.buffer_resize_snapshot(
            Arc::clone(&first_target),
            started + std::time::Duration::from_millis(1),
        );
        view.buffer_resize_snapshot(
            Arc::clone(&latest_target),
            started + std::time::Duration::from_millis(10),
        );
        view.settle_resize_presentation(started + std::time::Duration::from_millis(41));

        assert!(Arc::ptr_eq(view.snapshot.as_ref().unwrap(), &stable));
        assert_eq!(view.snapshot_gen, stable_generation);
        view.settle_resize_presentation(started + std::time::Duration::from_millis(42));
        assert!(Arc::ptr_eq(view.snapshot.as_ref().unwrap(), &latest_target));
        assert_eq!(view.snapshot_gen, stable_generation + 1);
        assert!(view.resize_presentation.is_none());
    }

    #[test]
    fn active_split_drag_defers_prior_resize_presentation_fence() {
        let started = std::time::Instant::now();
        let session = SessionId(62);
        let mut ui = WorkspaceUi::new();
        let view = ui.sessions.entry(session).or_default();
        view.install_snapshot(shaped_snapshot(80, 24, "stable"));
        view.arm_resize_presentation(100, 30, started);
        assert!(view.buffer_resize_snapshot(
            shaped_snapshot(100, 30, "old target"),
            started + std::time::Duration::from_millis(1),
        ));
        ui.begin_split_drag(tab_id("t"), Vec::new(), 0.72);

        let repaint = ui.settle_session_resize_presentation(
            session,
            started + std::time::Duration::from_millis(250),
        );

        assert_eq!(repaint, None);
        assert_eq!(ui.sessions[&session].snapshot.as_ref().unwrap().cols, 80);
        assert!(ui.sessions[&session].resize_presentation.is_some());
    }

    #[test]
    fn committed_and_final_resize_lifecycle_defer_prior_presentation_fence() {
        let started = std::time::Instant::now();
        let session = SessionId(63);
        let mut ui = WorkspaceUi::new();
        let view = ui.sessions.entry(session).or_default();
        view.install_snapshot(shaped_snapshot(80, 24, "stable"));
        view.arm_resize_presentation(100, 30, started);
        assert!(view.buffer_resize_snapshot(
            shaped_snapshot(100, 30, "old target"),
            started + std::time::Duration::from_millis(1),
        ));

        ui.begin_split_drag(tab_id("t"), Vec::new(), 0.72);
        ui.commit_split_drag(44);
        assert_eq!(
            ui.settle_session_resize_presentation(
                session,
                started + std::time::Duration::from_millis(250),
            ),
            None
        );
        assert_eq!(ui.sessions[&session].snapshot.as_ref().unwrap().cols, 80);

        ui.split_drag = None;
        ui.split_final_resize_sessions.insert(session);
        assert_eq!(
            ui.settle_session_resize_presentation(
                session,
                started + std::time::Duration::from_millis(250),
            ),
            None
        );
        assert_eq!(ui.sessions[&session].snapshot.as_ref().unwrap().cols, 80);

        ui.split_final_resize_sessions.clear();
        ui.split_final_resize_pending
            .insert((WorkspaceProtocolOperation(1), 1), (session, 100, 30));
        assert_eq!(
            ui.settle_session_resize_presentation(
                session,
                started + std::time::Duration::from_millis(250),
            ),
            None
        );
        assert_eq!(ui.sessions[&session].snapshot.as_ref().unwrap().cols, 80);

        ui.split_final_resize_pending.clear();
        assert_eq!(
            ui.settle_session_resize_presentation(
                session,
                started + std::time::Duration::from_millis(250),
            ),
            None
        );
        assert_eq!(ui.sessions[&session].snapshot.as_ref().unwrap().cols, 100);
        assert!(ui.sessions[&session].resize_presentation.is_none());
    }

    #[test]
    fn split_drag_is_activated_before_child_pane_settlement() {
        let started = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_millis(250))
            .unwrap();
        let mut workspace = WorkspaceUi::new();
        workspace.mux = Some(split_mux_snapshot(0.5));
        for session in [SessionId(41), SessionId(42)] {
            workspace
                .sessions
                .entry(session)
                .or_default()
                .install_snapshot(shaped_snapshot(80, 24, "stable"));
        }
        let view = workspace.sessions.get_mut(&SessionId(41)).unwrap();
        let stable_generation = view.snapshot_gen;
        view.arm_resize_presentation(100, 30, started);
        assert!(view.buffer_resize_snapshot(
            shaped_snapshot(100, 30, "old target"),
            started + std::time::Duration::from_millis(1),
        ));

        let ctx = egui::Context::default();
        let pointer = egui::pos2(200.0, 120.0);
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(400.0, 240.0),
            )),
            events: vec![
                egui::Event::PointerMoved(pointer),
                egui::Event::PointerButton {
                    pos: pointer,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            ..egui::RawInput::default()
        };
        let catalog = catalog();
        let config = TerminalConfig::default();
        let handle_id = split_handle_id(&tab_id("t"), &[]);
        ctx.run_ui(input, |ui| {
            ui.ctx().set_dragged_id(handle_id);
            workspace.show_with_input(ui, &config, &[], &catalog, true);
        })
        .drop_without_applying_deltas();

        assert!(workspace.split_drag.is_some());
        assert_eq!(
            workspace.sessions[&SessionId(41)].snapshot_gen,
            stable_generation,
            "the first dragged frame must activate the split fence before child panes settle",
        );
        assert!(
            workspace.sessions[&SessionId(41)]
                .resize_presentation
                .is_some()
        );
    }

    #[test]
    fn blank_target_viewport_waits_for_nonblank_or_hard_deadline() {
        let started = std::time::Instant::now();
        let stable = shaped_snapshot(80, 24, "stable content");
        let blank = shaped_snapshot(100, 30, "");
        let nonblank = shaped_snapshot(100, 30, "ready");
        let mut view = SessionView::default();
        view.install_snapshot(Arc::clone(&stable));
        view.arm_resize_presentation(100, 30, started);
        view.buffer_resize_snapshot(
            Arc::clone(&blank),
            started + std::time::Duration::from_millis(1),
        );

        view.settle_resize_presentation(started + std::time::Duration::from_millis(40));
        assert!(Arc::ptr_eq(view.snapshot.as_ref().unwrap(), &stable));
        view.buffer_resize_snapshot(
            Arc::clone(&nonblank),
            started + std::time::Duration::from_millis(50),
        );
        view.settle_resize_presentation(started + std::time::Duration::from_millis(81));
        assert!(Arc::ptr_eq(view.snapshot.as_ref().unwrap(), &stable));
        view.settle_resize_presentation(started + std::time::Duration::from_millis(82));
        assert!(Arc::ptr_eq(view.snapshot.as_ref().unwrap(), &nonblank));

        let mut hard_deadline_view = SessionView::default();
        hard_deadline_view.install_snapshot(Arc::clone(&stable));
        hard_deadline_view.arm_resize_presentation(100, 30, started);
        hard_deadline_view.buffer_resize_snapshot(Arc::clone(&blank), started);
        hard_deadline_view
            .settle_resize_presentation(started + std::time::Duration::from_millis(249));
        assert!(Arc::ptr_eq(
            hard_deadline_view.snapshot.as_ref().unwrap(),
            &stable
        ));
        hard_deadline_view
            .settle_resize_presentation(started + std::time::Duration::from_millis(250));
        assert!(Arc::ptr_eq(
            hard_deadline_view.snapshot.as_ref().unwrap(),
            &blank
        ));
    }

    #[test]
    fn final_resize_clears_selection_only_for_changed_grid() {
        let now = std::time::Instant::now();
        let session = SessionId(61);
        let mut ui = WorkspaceUi::new();
        ui.sessions
            .entry(session)
            .or_default()
            .install_snapshot(shaped_snapshot(80, 24, "selected"));
        ui.sessions.get_mut(&session).unwrap().pending_snapshot =
            Some(shaped_snapshot(80, 24, "pending"));
        ui.sent_sizes.insert(session, (80, 24));
        ui.selection = Some((session, 0, 3));
        ui.split_final_resize_sessions.insert(session);

        assert!(!ui.apply_split_final_resize_at(session, 80, 24, now));
        assert!(ui.selection.is_some(), "same grid must preserve selection");
        assert!(
            ui.sessions
                .get(&session)
                .unwrap()
                .resize_presentation
                .is_none()
        );

        ui.split_final_resize_sessions.insert(session);
        assert!(ui.apply_split_final_resize_at(session, 100, 30, now));
        assert!(
            ui.selection.is_some(),
            "selection stays valid until the runtime accepts the new grid"
        );
        assert!(
            ui.sessions
                .get(&session)
                .unwrap()
                .resize_request
                .as_ref()
                .unwrap()
                .applied
                .is_none(),
            "요청 시 안정 화면을 보존하되 적용 deadline은 시작하지 않는다"
        );
        drain_protocol(&mut ui);
        assert!(
            ui.selection.is_some(),
            "실제 viewport 승격 전 기존 화면 선택을 유지한다"
        );
        let view = ui.sessions.get(&session).unwrap();
        assert!(view.pending_snapshot.is_none());
        assert_eq!(
            view.resize_presentation.as_ref().map(|fence| fence.target),
            Some((100, 30))
        );
    }

    #[test]
    fn final_resize_promotion_clears_selection_created_while_fenced() {
        let started = std::time::Instant::now();
        let session = SessionId(62);
        let mut ui = WorkspaceUi::new();
        let view = ui.sessions.entry(session).or_default();
        view.install_snapshot(shaped_snapshot(80, 24, "stable"));
        view.arm_resize_presentation(100, 30, started);
        assert!(view.buffer_resize_snapshot(
            shaped_snapshot(100, 30, "settled"),
            started + std::time::Duration::from_millis(1),
        ));
        ui.selection = Some((session, 0, 3));

        let repaint = ui.settle_session_resize_presentation(
            session,
            started + std::time::Duration::from_millis(33),
        );

        assert_eq!(repaint, None);
        assert!(ui.selection.is_none());
        assert_eq!(
            ui.sessions[&session]
                .snapshot
                .as_ref()
                .map(|snapshot| (snapshot.cols, snapshot.rows)),
            Some((100, 30))
        );
    }

    #[test]
    fn terminal_final_resize_failure_releases_prior_presentation_fence() {
        let now = std::time::Instant::now();
        let started = now
            .checked_sub(std::time::Duration::from_millis(250))
            .unwrap();
        let session = SessionId(67);
        let mut ui = WorkspaceUi::new();
        let view = ui.sessions.entry(session).or_default();
        view.install_snapshot(shaped_snapshot(80, 24, "stable"));
        view.arm_resize_presentation(90, 30, started);
        assert!(view.buffer_resize_snapshot(
            shaped_snapshot(90, 30, "prior target"),
            started + std::time::Duration::from_millis(1),
        ));
        ui.sent_sizes.insert(session, (80, 24));
        ui.split_final_resize_sessions.insert(session);
        assert!(ui.apply_split_final_resize_at(session, 100, 30, now));
        let intent = ui.take_protocol_intent().expect("final resize");

        ui.complete_protocol(WorkspaceProtocolCompletion {
            operation: intent.operation(),
            generation: intent.generation(),
            result: Err(WorkspaceProtocolErrorCode::DeliveryFailed),
        });

        assert_eq!(ui.failed_resize_targets.get(&session), Some(&(100, 30)));
        assert!(!ui.split_final_resize_sessions.contains(&session));
        assert_eq!(ui.settle_session_resize_presentation(session, now), None);
        assert_eq!(ui.sessions[&session].snapshot.as_ref().unwrap().cols, 80);
        assert!(ui.sessions[&session].resize_presentation.is_none());
    }

    #[test]
    fn later_split_ack_consumes_exact_failed_final_target_and_releases_prior_fence() {
        let now = std::time::Instant::now();
        let started = now
            .checked_sub(std::time::Duration::from_millis(250))
            .unwrap();
        let session = SessionId(41);
        let mut ui = WorkspaceUi::new();
        let ctx = egui::Context::default();
        ui.handle_events(
            &[RuntimeEvent::MuxUpdated {
                snapshot: split_mux_snapshot(0.5),
            }],
            &catalog(),
        );
        let ready = shaped_snapshot(90, 30, "prior target");
        let view = ui.sessions.entry(session).or_default();
        view.install_snapshot(shaped_snapshot(80, 24, "stable"));
        view.arm_resize_presentation(90, 30, started);
        assert!(view.buffer_resize_snapshot(
            Arc::clone(&ready),
            started + std::time::Duration::from_millis(1),
        ));
        ui.sent_sizes.insert(session, (80, 24));
        ui.split_final_resize_sessions.insert(session);
        assert!(ui.apply_split_final_resize_at(session, 100, 30, now));
        let failed = ui.take_protocol_intent().expect("failed final resize");
        ui.complete_protocol(WorkspaceProtocolCompletion {
            operation: failed.operation(),
            generation: failed.generation(),
            result: Err(WorkspaceProtocolErrorCode::DeliveryFailed),
        });
        assert_eq!(ui.failed_resize_targets.get(&session), Some(&(100, 30)));

        ui.begin_split_drag(tab_id("t"), Vec::new(), 0.72);
        ui.commit_split_drag(60);
        ui.flush_render_side_effects_for_pass(&ctx, 60, false);
        assert!(matches!(
            drain_protocol(&mut ui).as_slice(),
            [RuntimeCommand::ResizeSplit { ratio, .. }] if (*ratio - 0.72).abs() < 0.0001
        ));
        ui.handle_events(
            &[RuntimeEvent::MuxUpdated {
                snapshot: split_mux_snapshot(0.72),
            }],
            &catalog(),
        );
        assert!(ui.split_final_resize_sessions.contains(&session));
        assert_eq!(ui.settle_session_resize_presentation(session, now), None);

        let repaint_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let repaint_count_for_callback = Arc::clone(&repaint_count);
        ctx.set_request_repaint_callback(move |_| {
            repaint_count_for_callback.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        });
        let pass = ctx.cumulative_pass_nr();
        ui.stage_terminal_resize_for_pass(pass, false, session, 100, 30);
        let before = repaint_count.load(std::sync::atomic::Ordering::Relaxed);

        ui.flush_render_side_effects(&ctx);

        assert!(!ui.split_final_resize_sessions.contains(&session));
        assert_eq!(ui.failed_resize_targets.get(&session), Some(&(100, 30)));
        assert!(ui.protocol_intents.is_empty());
        assert!(
            repaint_count.load(std::sync::atomic::Ordering::Relaxed) > before,
            "consuming an exact failed target must wake prior fence settlement",
        );
        assert_eq!(ui.settle_session_resize_presentation(session, now), None);
        assert_eq!(
            ui.sessions[&session].snapshot.as_ref().unwrap().cols,
            80,
            "실패 뒤에는 검증되지 않은 중간 후보 대신 기존 stable 화면을 유지한다"
        );
        assert!(ui.sessions[&session].resize_presentation.is_none());
    }

    #[test]
    fn same_grid_final_marker_clear_wakes_prior_presentation_settlement() {
        let now = std::time::Instant::now();
        let started = now
            .checked_sub(std::time::Duration::from_millis(250))
            .unwrap();
        let session = SessionId(68);
        let mut ui = WorkspaceUi::new();
        let ready = shaped_snapshot(80, 24, "prior target");
        let view = ui.sessions.entry(session).or_default();
        view.install_snapshot(shaped_snapshot(80, 24, "stable"));
        view.arm_resize_presentation(80, 24, started);
        assert!(view.buffer_resize_snapshot(
            Arc::clone(&ready),
            started + std::time::Duration::from_millis(1),
        ));
        ui.sent_sizes.insert(session, (80, 24));
        ui.split_final_resize_sessions.insert(session);
        assert_eq!(ui.settle_session_resize_presentation(session, now), None);

        let ctx = egui::Context::default();
        let repaint_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let repaint_count_for_callback = Arc::clone(&repaint_count);
        ctx.set_request_repaint_callback(move |_| {
            repaint_count_for_callback.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        });
        let pass = ctx.cumulative_pass_nr();
        ui.stage_terminal_resize_for_pass(pass, false, session, 80, 24);
        let before = repaint_count.load(std::sync::atomic::Ordering::Relaxed);

        ui.flush_render_side_effects(&ctx);

        assert!(!ui.split_final_resize_sessions.contains(&session));
        assert!(
            repaint_count.load(std::sync::atomic::Ordering::Relaxed) > before,
            "consuming a no-op final marker must wake the next settlement pass",
        );
        assert_eq!(ui.settle_session_resize_presentation(session, now), None);
        assert!(Arc::ptr_eq(
            ui.sessions[&session].snapshot.as_ref().unwrap(),
            &ready,
        ));
        assert!(ui.sessions[&session].resize_presentation.is_none());
    }

    #[test]
    fn failed_final_resize_rolls_back_and_requires_new_geometry_before_retry() {
        let started = std::time::Instant::now();
        let session = SessionId(63);
        let mut ui = WorkspaceUi::new();
        ui.sessions
            .entry(session)
            .or_default()
            .install_snapshot(shaped_snapshot(80, 24, "stable"));
        ui.sent_sizes.insert(session, (80, 24));
        ui.selection = Some((session, 0, 3));
        ui.split_final_resize_sessions.insert(session);
        assert!(ui.apply_split_final_resize_at(session, 100, 30, started));
        let intent = ui.take_protocol_intent().expect("final resize");

        ui.complete_protocol(WorkspaceProtocolCompletion {
            operation: intent.operation(),
            generation: intent.generation(),
            result: Err(WorkspaceProtocolErrorCode::DeliveryFailed),
        });

        assert_eq!(ui.sent_sizes.get(&session), Some(&(80, 24)));
        assert!(
            ui.sessions[&session].resize_presentation.is_none(),
            "a command that never reached the runtime cannot own a viewport fence"
        );
        assert!(ui.selection.is_some());
        assert!(!ui.split_final_resize_sessions.contains(&session));
        assert!(!ui.queue_terminal_resize(session, 100, 30));
        assert!(
            ui.protocol_intents.is_empty(),
            "do not spin on a dead channel"
        );

        ui.handle_events(
            &[RuntimeEvent::Viewport {
                session,
                snapshot: shaped_snapshot(80, 24, "unrelated output"),
                bracketed_paste: false,
            }],
            &catalog(),
        );
        assert!(
            !ui.queue_terminal_resize(session, 100, 30),
            "viewport output does not prove that command backpressure recovered"
        );
        assert!(ui.queue_terminal_resize(session, 101, 30));
    }

    #[test]
    fn coalesced_newer_geometry_updates_the_exact_pending_final_fence() {
        let started = std::time::Instant::now();
        let session = SessionId(64);
        let mut ui = WorkspaceUi::new();
        ui.sessions
            .entry(session)
            .or_default()
            .install_snapshot(shaped_snapshot(80, 24, "stable"));
        ui.sent_sizes.insert(session, (80, 24));
        ui.split_final_resize_sessions.insert(session);
        assert!(ui.apply_split_final_resize_at(session, 100, 30, started));

        assert!(ui.queue_terminal_resize(session, 120, 40));
        assert_eq!(
            ui.protocol_intents.len(),
            1,
            "same-session Resize coalesces"
        );
        drain_protocol(&mut ui);

        assert_eq!(
            ui.sessions[&session]
                .resize_presentation
                .as_ref()
                .map(|fence| fence.target),
            Some((120, 40)),
            "the fence must match the command that actually reached the runtime"
        );
    }

    /// 창 리사이즈로 나가는 Resize는 split 최종 Resize가 아니라 fence를 한 번도 얻지
    /// 못했다. 그래서 PTY reflow 뒤 자식 TUI의 clear→redraw 중간 viewport가 그대로
    /// 화면에 올라가 리사이즈가 끝나는 순간 한 번 번쩍였다(2026-09-06). 안정 화면은
    /// target 모양의 viewport가 조용해질 때까지 유지돼야 한다.
    #[test]
    fn tracked_ui_newer_external_ack_cancels_older_unpresented_candidate() {
        let mut view = SessionView::default();
        let stable = shaped_snapshot(80, 24, "stable");
        view.install_snapshot(Arc::clone(&stable));
        let now = std::time::Instant::now();
        let token = view.prepare_tracked_resize([1; 16], (100, 30)).unwrap();
        let applied = runtime::ResizeStamp {
            epoch: 1,
            owner_epoch: 1,
            token: Some(token),
            cols: 100,
            rows: 30,
        };
        assert!(view.accepts_resize_viewport(Some(applied), (100, 30), now));
        view.buffer_resize_snapshot(shaped_snapshot(100, 30, "old candidate"), now);
        let external = runtime::ResizeStamp {
            epoch: 2,
            owner_epoch: 2,
            token: Some(runtime::ResizeToken {
                owner: [2; 16],
                owner_epoch: 2,
                generation: 1,
            }),
            cols: 110,
            rows: 40,
        };
        assert!(!view.observe_resize_applied(external, now));
        view.settle_resize_presentation(now + RESIZE_VIEWPORT_HARD_DEADLINE);
        assert!(Arc::ptr_eq(view.snapshot.as_ref().unwrap(), &stable));
        assert!(view.accepts_resize_viewport(Some(external), (110, 40), now));
    }

    #[test]
    fn tracked_ui_pending_retry_coalesced_to_new_target_becomes_initial_delivery() {
        let mut ui = WorkspaceUi::new();
        let session = SessionId(66);
        let ctx = egui::Context::default();
        ui.queue_terminal_resize(session, 100, 30);
        drain_protocol(&mut ui);
        let token = ui.sessions[&session].resize_request.as_ref().unwrap().token;
        ui.sessions
            .get_mut(&session)
            .unwrap()
            .resize_request
            .as_mut()
            .unwrap()
            .retry_at = Some(std::time::Instant::now());
        ui.flush_render_side_effects_for_pass(&ctx, 1, false);
        let stamp = runtime::ResizeStamp {
            epoch: 1,
            owner_epoch: 1,
            token: Some(token),
            cols: 100,
            rows: 30,
        };
        ui.sessions
            .get_mut(&session)
            .unwrap()
            .observe_resize_applied(stamp, std::time::Instant::now());
        assert!(ui.queue_terminal_resize(session, 110, 40));
        drain_protocol(&mut ui);
        let request = ui.sessions[&session].resize_request.as_ref().unwrap();
        assert!(
            request.admitted,
            "새 target으로 coalesce된 retry는 initial delivery다"
        );
        assert_eq!(request.retries, 0);
        assert!(request.retry_at.is_some());
    }

    #[test]
    fn tracked_ui_stale_retry_completion_cannot_cancel_new_token() {
        let mut ui = WorkspaceUi::new();
        let session = SessionId(66);
        ui.queue_terminal_resize(session, 100, 30);
        drain_protocol(&mut ui);
        let token = ui.sessions[&session].resize_request.as_ref().unwrap().token;
        ui.sessions
            .get_mut(&session)
            .unwrap()
            .resize_request
            .as_mut()
            .unwrap()
            .retry_at = Some(std::time::Instant::now());
        ui.flush_render_side_effects_for_pass(&egui::Context::default(), 1, false);
        let old = ui.take_protocol_intent().unwrap();
        let stamp = runtime::ResizeStamp {
            epoch: 1,
            owner_epoch: 1,
            token: Some(token),
            cols: 100,
            rows: 30,
        };
        ui.sessions
            .get_mut(&session)
            .unwrap()
            .observe_resize_applied(stamp, std::time::Instant::now());
        ui.queue_terminal_resize(session, 110, 40);
        let current = ui.sessions[&session].resize_request.as_ref().unwrap().token;
        ui.complete_protocol(WorkspaceProtocolCompletion {
            operation: old.operation(),
            generation: old.generation(),
            result: Err(WorkspaceProtocolErrorCode::DeliveryFailed),
        });
        assert_eq!(
            ui.sessions[&session].resize_request.as_ref().unwrap().token,
            current
        );
        assert_eq!(
            ui.sessions[&session]
                .resize_request
                .as_ref()
                .unwrap()
                .retries,
            0
        );
        assert_eq!(ui.sent_sizes[&session], (110, 40));
        assert!(!ui.failed_resize_targets.contains_key(&session));
    }

    #[test]
    fn tracked_ui_retry_budget_counts_host_acceptance_not_local_queue_pressure() {
        let mut ui = WorkspaceUi::new();
        let session = SessionId(66);
        let ctx = egui::Context::default();
        ui.queue_terminal_resize(session, 100, 30);
        drain_protocol(&mut ui);
        for n in 0..WORKSPACE_PROTOCOL_CAP {
            ui.queue_protocol_intent(RuntimeCommand::Scroll {
                session: SessionId(100 + n as u64),
                delta: 1,
            })
            .unwrap();
        }
        for pass in [1, 2] {
            ui.sessions
                .get_mut(&session)
                .unwrap()
                .resize_request
                .as_mut()
                .unwrap()
                .retry_at = Some(std::time::Instant::now());
            ui.flush_render_side_effects_for_pass(&ctx, pass, false);
            let request = ui.sessions[&session].resize_request.as_ref().unwrap();
            assert_eq!(
                request.retries, 0,
                "로컬 큐 거부는 ACK 재확인 예산을 소비하지 않는다"
            );
            assert!(request.retry_at.unwrap() > std::time::Instant::now());
        }
        drain_protocol(&mut ui);
        ui.sessions
            .get_mut(&session)
            .unwrap()
            .resize_request
            .as_mut()
            .unwrap()
            .retry_at = Some(std::time::Instant::now());
        ui.flush_render_side_effects_for_pass(&ctx, 3, false);
        let intent = ui.take_protocol_intent().unwrap();
        assert_eq!(
            ui.sessions[&session]
                .resize_request
                .as_ref()
                .unwrap()
                .retries,
            0
        );
        ui.flush_render_side_effects_for_pass(&ctx, 4, false);
        assert!(
            ui.protocol_intents.is_empty(),
            "delivery 대기 중 같은 retry를 중복 enqueue하지 않는다"
        );
        ui.complete_protocol(WorkspaceProtocolCompletion {
            operation: intent.operation(),
            generation: intent.generation(),
            result: Err(WorkspaceProtocolErrorCode::Busy),
        });
        assert_eq!(
            ui.sessions[&session]
                .resize_request
                .as_ref()
                .unwrap()
                .retries,
            0
        );
        ui.sessions
            .get_mut(&session)
            .unwrap()
            .resize_request
            .as_mut()
            .unwrap()
            .retry_at = Some(std::time::Instant::now());
        ui.flush_render_side_effects_for_pass(&ctx, 5, false);
        let intent = ui.take_protocol_intent().unwrap();
        ui.complete_protocol(WorkspaceProtocolCompletion {
            operation: intent.operation(),
            generation: intent.generation(),
            result: Ok(()),
        });
        assert_eq!(
            ui.sessions[&session]
                .resize_request
                .as_ref()
                .unwrap()
                .retries,
            1
        );
    }

    #[test]
    fn tracked_ui_split_final_local_queue_full_preserves_marker_until_retry() {
        let mut ui = WorkspaceUi::new();
        let session = SessionId(66);
        let ctx = egui::Context::default();
        ui.sessions
            .entry(session)
            .or_default()
            .install_snapshot(shaped_snapshot(80, 24, "stable"));
        ui.sent_sizes.insert(session, (80, 24));
        ui.sessions.get_mut(&session).unwrap().resize_generation = 7;
        ui.split_final_resize_sessions.insert(session);
        for n in 0..WORKSPACE_PROTOCOL_CAP {
            ui.queue_protocol_intent(RuntimeCommand::Scroll {
                session: SessionId(100 + n as u64),
                delta: 1,
            })
            .unwrap();
        }
        for pass in [1, 2] {
            ui.stage_terminal_resize_for_pass(pass, false, session, 100, 30);
            ui.flush_render_side_effects_for_pass(&ctx, pass, false);
            assert!(
                ui.split_final_resize_sessions.contains(&session),
                "로컬 큐 거부가 최종 marker를 소비하면 안 된다"
            );
        }
        assert_eq!(ui.sessions[&session].resize_generation, 7);
        drain_protocol(&mut ui);
        ui.stage_terminal_resize_for_pass(3, false, session, 100, 30);
        ui.flush_render_side_effects_for_pass(&ctx, 3, false);
        assert!(matches!(
            ui.take_protocol_intent().unwrap().command,
            RuntimeCommand::ResizeTracked {
                cols: 100,
                rows: 30,
                ..
            }
        ));
        assert_eq!(ui.split_final_resize_pending.len(), 1);
    }

    #[test]
    fn tracked_ui_local_queue_rejection_does_not_leave_unadmitted_fence() {
        let mut ui = WorkspaceUi::new();
        let session = SessionId(66);
        ui.sessions
            .entry(session)
            .or_default()
            .install_snapshot(shaped_snapshot(80, 24, "stable"));
        ui.sent_sizes.insert(session, (80, 24));
        for n in 0..WORKSPACE_PROTOCOL_CAP {
            ui.queue_protocol_intent(RuntimeCommand::Scroll {
                session: SessionId(100 + n as u64),
                delta: 1,
            })
            .unwrap();
        }
        assert!(!ui.queue_terminal_resize(session, 100, 30));
        assert!(ui.sessions[&session].resize_request.is_none());
        assert!(ui.sessions[&session].resize_presentation.is_none());
        assert!(
            ui.sessions
                .get_mut(&session)
                .unwrap()
                .accepts_resize_viewport(None, (80, 24), std::time::Instant::now())
        );
    }

    #[test]
    fn tracked_ui_new_debounced_geometry_resets_owner_recovery_budget() {
        let mut ui = WorkspaceUi::new();
        let session = SessionId(66);
        let view = ui.sessions.entry(session).or_default();
        view.resize_desired = Some((100, 30));
        view.resize_owner_retries = 2;
        ui.queue_terminal_resize_debounced(&egui::Context::default(), session, 110, 40);
        assert_eq!(ui.sessions[&session].resize_owner_retries, 0);
    }

    #[test]
    fn tracked_ui_external_applied_geometry_invalidates_sent_size_for_local_reapply() {
        let mut ui = WorkspaceUi::new();
        let session = SessionId(66);
        ui.sent_sizes.insert(session, (100, 30));
        let view = ui.sessions.entry(session).or_default();
        view.resize_desired = Some((100, 30));
        let token = view.prepare_tracked_resize([1; 16], (100, 30)).unwrap();
        let local = runtime::ResizeStamp {
            epoch: 1,
            owner_epoch: 1,
            token: Some(token),
            cols: 100,
            rows: 30,
        };
        assert!(view.accepts_resize_viewport(Some(local), (100, 30), std::time::Instant::now()));
        ui.finish_resize_viewport(session, local);
        let external = runtime::ResizeStamp {
            epoch: 2,
            owner_epoch: 1,
            token: None,
            cols: 110,
            rows: 40,
        };
        assert!(
            ui.sessions
                .get_mut(&session)
                .unwrap()
                .accepts_resize_viewport(Some(external), (110, 40), std::time::Instant::now())
        );
        ui.finish_resize_viewport(session, external);
        assert!(!ui.sent_sizes.contains_key(&session));
        ui.queue_terminal_resize_debounced(&egui::Context::default(), session, 100, 30);
        assert!(matches!(
            ui.take_protocol_intent().unwrap().command,
            RuntimeCommand::ResizeTracked {
                cols: 100,
                rows: 30,
                ..
            }
        ));
    }

    #[test]
    fn tracked_ui_presentable_request_releases_for_newer_untracked_or_foreign_epoch() {
        for owner in [None, Some([2; 16])] {
            let mut view = SessionView::default();
            view.install_snapshot(shaped_snapshot(80, 24, "stable"));
            let now = std::time::Instant::now();
            let token = view.prepare_tracked_resize([1; 16], (100, 30)).unwrap();
            let applied = runtime::ResizeStamp {
                epoch: 1,
                owner_epoch: 1,
                token: Some(token),
                cols: 100,
                rows: 30,
            };
            assert!(view.accepts_resize_viewport(Some(applied), (100, 30), now));
            view.buffer_resize_snapshot(shaped_snapshot(100, 30, "ready"), now);
            view.settle_resize_presentation(now + RESIZE_VIEWPORT_QUIET);
            assert!(view.resize_request.is_none(), "승격된 성공 요청은 종료한다");
            let next_token = owner.map(|owner| runtime::ResizeToken {
                owner,
                generation: 1,
                owner_epoch: 2,
            });
            let next = runtime::ResizeStamp {
                epoch: 2,
                owner_epoch: if owner.is_some() { 2 } else { 1 },
                token: next_token,
                cols: 110,
                rows: 40,
            };
            assert!(view.accepts_resize_viewport(Some(next), (110, 40), now));
            let local = view.prepare_tracked_resize([1; 16], (100, 30)).unwrap();
            assert!(local.generation > token.generation);
            assert_eq!(local.owner_epoch, if owner.is_some() { 3 } else { 1 });
            assert!(!view.accepts_resize_viewport(Some(applied), (100, 30), now));
        }
    }

    #[test]
    fn tracked_ui_owner_collision_retries_from_latest_epoch_at_most_twice() {
        let mut view = SessionView::default();
        let now = std::time::Instant::now();
        for attempt in 0..3u64 {
            let token = view.prepare_tracked_resize([1; 16], (100, 30)).unwrap();
            view.resize_failed(token, runtime::ResizeFailure::Conflict);
            let winner = runtime::ResizeToken {
                owner: [2; 16],
                owner_epoch: token.owner_epoch,
                generation: 1,
            };
            let stamp = runtime::ResizeStamp {
                epoch: attempt + 1,
                owner_epoch: winner.owner_epoch,
                token: Some(winner),
                cols: 110,
                rows: 40,
            };
            assert!(view.accepts_resize_viewport(Some(stamp), (110, 40), now));
            assert_eq!(view.take_resize_owner_retry(stamp), attempt < 2);
            if attempt < 2 {
                assert_eq!(
                    view.prepare_tracked_resize([1; 16], (100, 30))
                        .unwrap()
                        .owner_epoch,
                    token.owner_epoch + 1
                );
            }
        }
    }

    #[test]
    fn tracked_ui_hidden_late_ack_does_not_revive_snapshot_or_timer() {
        let mut ui = WorkspaceUi::new();
        let session = SessionId(41);
        ui.handle_events(
            &[RuntimeEvent::MuxUpdated {
                snapshot: split_mux_snapshot(0.5),
            }],
            &catalog(),
        );
        ui.sessions
            .entry(session)
            .or_default()
            .install_snapshot(shaped_snapshot(80, 24, "stable"));
        ui.queue_terminal_resize(session, 100, 30);
        drain_protocol(&mut ui);
        let token = ui.sessions[&session].resize_request.as_ref().unwrap().token;
        let mut hidden = (*split_mux_snapshot(0.5)).clone();
        hidden.active_tab = None;
        let stamp = runtime::ResizeStamp {
            epoch: 1,
            owner_epoch: 1,
            token: Some(token),
            cols: 100,
            rows: 30,
        };
        ui.handle_events(
            &[
                RuntimeEvent::MuxUpdated {
                    snapshot: Arc::new(hidden),
                },
                RuntimeEvent::ResizeApplied { session, stamp },
                RuntimeEvent::ViewportTracked {
                    session,
                    stamp,
                    snapshot: shaped_snapshot(100, 30, "late"),
                    bracketed_paste: true,
                },
            ],
            &catalog(),
        );
        let view = &ui.sessions[&session];
        assert!(view.snapshot.is_none());
        assert!(view.resize_request.is_none());
        assert!(view.resize_presentation.is_none());
        assert!(!view.bracketed_paste);
    }

    #[test]
    fn tracked_ui_coalescing_preserves_latest_token_and_final_dimensions() {
        let mut ui = WorkspaceUi::new();
        let session = SessionId(66);
        ui.split_final_resize_sessions.insert(session);
        assert!(ui.apply_split_final_resize_at(session, 100, 30, std::time::Instant::now()));
        let old = ui.sessions[&session].resize_request.as_ref().unwrap().token;
        assert!(ui.queue_terminal_resize(session, 110, 40));
        let intent = ui.take_protocol_intent().unwrap();
        let RuntimeCommand::ResizeTracked {
            token, cols, rows, ..
        } = intent.command
        else {
            panic!("tracked resize");
        };
        assert_ne!(token, old);
        let key = (intent.operation, intent.generation);
        assert_eq!(ui.resize_delivery_rollbacks[&key].token, token);
        assert_eq!(ui.resize_delivery_rollbacks[&key].target, (cols, rows));
        assert_eq!(ui.split_final_resize_pending[&key], (session, cols, rows));
    }

    #[test]
    fn tracked_ui_applied_b_rejects_old_a_and_requires_exact_viewport() {
        let mut view = SessionView::default();
        view.install_snapshot(shaped_snapshot(80, 24, "stable"));
        let now = std::time::Instant::now();
        let a = view.prepare_tracked_resize([1; 16], (100, 30)).unwrap();
        let stamp_a = runtime::ResizeStamp {
            epoch: 1,
            owner_epoch: 1,
            token: Some(a),
            cols: 100,
            rows: 30,
        };
        assert!(view.observe_resize_applied(stamp_a, now));
        let b = view.prepare_tracked_resize([1; 16], (110, 40)).unwrap();
        let stamp_b = runtime::ResizeStamp {
            epoch: 2,
            owner_epoch: 1,
            token: Some(b),
            cols: 110,
            rows: 40,
        };
        assert!(view.observe_resize_applied(stamp_b, now));
        assert!(!view.accepts_resize_viewport(Some(stamp_a), (100, 30), now));
        assert!(!view.accepts_resize_viewport(Some(stamp_b), (100, 30), now));
        assert!(view.accepts_resize_viewport(Some(stamp_b), (110, 40), now));
        let a3 = view.prepare_tracked_resize([1; 16], (100, 30)).unwrap();
        assert_ne!(a, a3);
        assert!(!view.accepts_resize_viewport(Some(stamp_a), (100, 30), now));
        assert!(!view.accepts_resize_viewport(Some(stamp_b), (110, 40), now));
    }

    #[test]
    fn tracked_ui_split_same_admitted_target_keeps_final_pending() {
        let mut ui = WorkspaceUi::new();
        let session = SessionId(66);
        assert!(ui.queue_terminal_resize(session, 100, 30));
        drain_protocol(&mut ui);
        ui.split_final_resize_sessions.insert(session);
        assert!(!ui.apply_split_final_resize_at(session, 100, 30, std::time::Instant::now()));
        assert!(ui.split_final_resize_sessions.contains(&session));
    }

    #[test]
    fn tracked_ui_runtime_replacement_uses_new_owner_and_cannot_accept_old_proof() {
        let mut old = WorkspaceUi::with_resize_owner([1; 16]);
        let mut new = WorkspaceUi::with_resize_owner([2; 16]);
        let session = SessionId(66);
        old.queue_terminal_resize(session, 100, 30);
        new.queue_terminal_resize(session, 100, 30);
        let token = old.sessions[&session]
            .resize_request
            .as_ref()
            .unwrap()
            .token;
        let stamp = runtime::ResizeStamp {
            epoch: 1,
            owner_epoch: 1,
            token: Some(token),
            cols: 100,
            rows: 30,
        };
        assert!(
            !new.sessions
                .get_mut(&session)
                .unwrap()
                .accepts_resize_viewport(Some(stamp), (100, 30), std::time::Instant::now())
        );
    }

    #[test]
    fn tracked_ui_admission_keeps_final_marker_and_rollback() {
        let mut ui = WorkspaceUi::new();
        let session = SessionId(66);
        ui.sessions
            .entry(session)
            .or_default()
            .install_snapshot(shaped_snapshot(80, 24, "stable"));
        ui.split_final_resize_sessions.insert(session);
        assert!(ui.apply_split_final_resize_at(session, 100, 30, std::time::Instant::now()));
        drain_protocol(&mut ui);
        assert_eq!(ui.resize_delivery_rollbacks.len(), 1);
        assert_eq!(ui.split_final_resize_pending.len(), 1);
    }

    #[test]
    fn tracked_ui_late_aba_delivery_failure_cannot_rollback_latest_request() {
        let mut ui = WorkspaceUi::new();
        let session = SessionId(66);
        ui.sent_sizes.insert(session, (80, 24));
        assert!(ui.queue_terminal_resize(session, 100, 30));
        let old = ui.take_protocol_intent().unwrap();
        assert!(ui.queue_terminal_resize(session, 110, 30));
        assert!(ui.queue_terminal_resize(session, 100, 30));
        let token = ui.sessions[&session].resize_request.as_ref().unwrap().token;
        ui.complete_protocol(WorkspaceProtocolCompletion {
            operation: old.operation,
            generation: old.generation,
            result: Err(WorkspaceProtocolErrorCode::DeliveryFailed),
        });
        assert_eq!(ui.sent_sizes[&session], (100, 30));
        assert_eq!(
            ui.sessions[&session].resize_request.as_ref().unwrap().token,
            token
        );
        assert!(!ui.failed_resize_targets.contains_key(&session));
    }

    #[test]
    fn tracked_ui_failed_and_exhausted_requests_release_presentation() {
        for reason in [
            runtime::ResizeFailure::Superseded,
            runtime::ResizeFailure::Conflict,
        ] {
            let mut view = SessionView::default();
            view.install_snapshot(shaped_snapshot(80, 24, "stable"));
            let token = view.prepare_tracked_resize([1; 16], (100, 30)).unwrap();
            view.resize_failed(token, reason);
            assert!(view.resize_request.is_none());
            assert!(view.resize_presentation.is_none());
            assert!(view.snapshot.is_some());
        }
        let mut view = SessionView::default();
        view.install_snapshot(shaped_snapshot(80, 24, "stable"));
        let token = view.prepare_tracked_resize([1; 16], (100, 30)).unwrap();
        let now = std::time::Instant::now();
        view.resize_admitted(token, now);
        for seconds in [2, 4, 6] {
            let at = now + std::time::Duration::from_secs(seconds);
            if view.resize_retry_due(at).is_some() {
                let key = (WorkspaceProtocolOperation(seconds), 1);
                view.resize_request.as_mut().unwrap().retry_pending = Some(key);
                view.resize_retry_completed(token, key, true, at);
            }
        }
        assert!(view.resize_request.is_none());
        assert!(view.resize_presentation.is_none());
        let stamp = runtime::ResizeStamp {
            epoch: 9,
            owner_epoch: 1,
            token: Some(token),
            cols: 100,
            rows: 30,
        };
        assert!(view.accepts_resize_viewport(Some(stamp), (100, 30), now));
    }

    #[test]
    fn resize_admission은_실제_적용_전_deadline을_시작하지_않는다() {
        let session = SessionId(66);
        let mut ui = WorkspaceUi::new();
        ui.sessions
            .entry(session)
            .or_default()
            .install_snapshot(shaped_snapshot(80, 24, "stable"));
        ui.sent_sizes.insert(session, (80, 24));
        assert!(ui.queue_terminal_resize(session, 100, 30));
        drain_protocol(&mut ui);
        let view = ui.sessions.get_mut(&session).unwrap();
        let now = std::time::Instant::now() + std::time::Duration::from_millis(300);
        view.settle_resize_presentation(now);
        assert!(
            view.resize_presentation.is_some(),
            "queue 수락은 실제 적용 증거가 아니다"
        );
    }

    #[test]
    fn 창_리사이즈_전송도_안정_화면을_fence로_지킨다() {
        let session = SessionId(66);
        let mut ui = WorkspaceUi::new();
        let stable = shaped_snapshot(80, 24, "이미 있던 출력");
        ui.sessions
            .entry(session)
            .or_default()
            .install_snapshot(Arc::clone(&stable));
        ui.sent_sizes.insert(session, (80, 24));

        // 창 드래그가 멈춰 최종 크기가 나간다 — split 경로가 아니다.
        assert!(ui.queue_terminal_resize(session, 100, 30));
        drain_protocol(&mut ui);
        let request = ui.sessions[&session].resize_request.as_ref().unwrap();
        let stamp = runtime::ResizeStamp {
            epoch: 1,
            owner_epoch: request.token.owner_epoch,
            token: Some(request.token),
            cols: request.target.0,
            rows: request.target.1,
        };
        ui.sessions
            .get_mut(&session)
            .unwrap()
            .observe_resize_applied(stamp, std::time::Instant::now());

        assert!(!ui.split_final_resize_sessions.contains(&session));

        let started = ui.sessions[&session]
            .resize_presentation
            .as_ref()
            .expect("창 리사이즈에도 fence가 걸려야 한다")
            .started_at;
        assert_eq!(
            ui.sessions[&session]
                .resize_presentation
                .as_ref()
                .map(|fence| fence.target),
            Some((100, 30))
        );

        // clear 직후의 빈 중간 viewport는 안정 화면을 덮지 못한다.
        assert!(
            ui.sessions
                .get_mut(&session)
                .unwrap()
                .buffer_resize_snapshot(
                    shaped_snapshot(100, 30, ""),
                    started + std::time::Duration::from_millis(1),
                )
        );
        assert!(
            ui.settle_session_resize_presentation(
                session,
                started + std::time::Duration::from_millis(40),
            )
            .is_some()
        );
        assert!(
            Arc::ptr_eq(ui.sessions[&session].snapshot.as_ref().unwrap(), &stable),
            "clear 직후의 빈 화면이 표시됐다"
        );

        // 실제로 다시 그려진 화면이 조용해지면 그때 승격된다.
        assert!(
            ui.sessions
                .get_mut(&session)
                .unwrap()
                .buffer_resize_snapshot(
                    shaped_snapshot(100, 30, "다시 그린 출력"),
                    started + std::time::Duration::from_millis(50),
                )
        );
        assert_eq!(
            ui.settle_session_resize_presentation(
                session,
                started + std::time::Duration::from_millis(82),
            ),
            None
        );
        assert_eq!(ui.sessions[&session].snapshot.as_ref().unwrap().cols, 100);
        assert!(ui.sessions[&session].resize_presentation.is_none());
    }

    /// 스냅샷이 아직 하나도 없는 세션(생성 직후 첫 Resize)에는 fence를 걸지 않는다.
    /// 지킬 안정 화면이 없는데 걸면 target과 모양이 다른 첫 viewport가 통째로 버려져
    /// 「연결 중」이 최대 250ms 남는다.
    #[test]
    fn 첫_화면이_없는_세션의_리사이즈는_fence를_걸지_않는다() {
        let session = SessionId(67);
        let mut ui = WorkspaceUi::new();

        assert!(ui.queue_terminal_resize(session, 100, 30));
        drain_protocol(&mut ui);

        assert!(
            ui.sessions
                .get(&session)
                .and_then(|view| view.resize_presentation.as_ref())
                .is_none()
        );
    }

    #[test]
    fn newer_resize_delivery_retargets_existing_fence_without_extending_deadline() {
        let started = std::time::Instant::now();
        let session = SessionId(65);
        let mut ui = WorkspaceUi::new();
        let view = ui.sessions.entry(session).or_default();
        view.install_snapshot(shaped_snapshot(80, 24, "stable"));
        view.arm_resize_presentation(100, 30, started);
        assert!(view.buffer_resize_snapshot(
            shaped_snapshot(100, 30, ""),
            started + std::time::Duration::from_millis(1),
        ));
        ui.sent_sizes.insert(session, (100, 30));

        assert!(ui.queue_terminal_resize(session, 120, 40));
        drain_protocol(&mut ui);
        let request = ui.sessions[&session].resize_request.as_ref().unwrap();
        let stamp = runtime::ResizeStamp {
            epoch: 1,
            owner_epoch: request.token.owner_epoch,
            token: Some(request.token),
            cols: request.target.0,
            rows: request.target.1,
        };
        ui.sessions
            .get_mut(&session)
            .unwrap()
            .observe_resize_applied(stamp, started);

        let fence = ui.sessions[&session]
            .resize_presentation
            .as_ref()
            .expect("original stable presentation remains fenced");
        assert_eq!(fence.target, (120, 40));
        assert_eq!(fence.started_at, started, "hard deadline must not extend");
        assert!(
            fence.latest_target.is_none(),
            "stale A candidate must be dropped"
        );

        assert!(
            ui.sessions
                .get_mut(&session)
                .unwrap()
                .buffer_resize_snapshot(
                    shaped_snapshot(120, 40, "new target"),
                    started + std::time::Duration::from_millis(10),
                )
        );
        ui.settle_session_resize_presentation(
            session,
            started + std::time::Duration::from_millis(42),
        );
        assert_eq!(
            ui.sessions[&session]
                .snapshot
                .as_ref()
                .map(|snapshot| (snapshot.cols, snapshot.rows)),
            Some((120, 40))
        );
    }

    #[test]
    fn busy_final_resize_rolls_back_and_waits_before_retrying() {
        let started = std::time::Instant::now();
        let session = SessionId(66);
        let mut ui = WorkspaceUi::new();
        let ctx = egui::Context::default();
        ui.sessions
            .entry(session)
            .or_default()
            .install_snapshot(shaped_snapshot(80, 24, "stable"));
        ui.sent_sizes.insert(session, (80, 24));
        ui.split_final_resize_sessions.insert(session);
        assert!(ui.apply_split_final_resize_at(session, 100, 30, started));
        let intent = ui.take_protocol_intent().expect("final resize");
        ui.complete_protocol(WorkspaceProtocolCompletion {
            operation: intent.operation(),
            generation: intent.generation(),
            result: Err(WorkspaceProtocolErrorCode::Busy),
        });

        assert_eq!(ui.sent_sizes.get(&session), Some(&(80, 24)));
        assert!(ui.split_final_resize_sessions.contains(&session));
        assert!(
            ui.resize_retry
                .get(&session)
                .is_some_and(|retry| retry.retry_at.is_some())
        );
        ui.stage_terminal_resize_for_pass(49, false, session, 100, 30);
        ui.flush_render_side_effects_for_pass(&ctx, 49, false);
        assert!(
            ui.protocol_intents.is_empty(),
            "same-frame retry is a hot loop"
        );
    }

    #[test]
    fn exhausted_busy_final_resize_releases_prior_presentation_fence_without_hot_loop() {
        let now = std::time::Instant::now();
        let started = now
            .checked_sub(std::time::Duration::from_millis(250))
            .unwrap();
        let session = SessionId(69);
        let mut ui = WorkspaceUi::new();
        let ctx = egui::Context::default();
        let view = ui.sessions.entry(session).or_default();
        view.install_snapshot(shaped_snapshot(80, 24, "stable"));
        view.arm_resize_presentation(90, 30, started);
        assert!(view.buffer_resize_snapshot(
            shaped_snapshot(90, 30, "prior target"),
            started + std::time::Duration::from_millis(1),
        ));
        ui.sent_sizes.insert(session, (80, 24));
        ui.split_final_resize_sessions.insert(session);
        assert!(ui.apply_split_final_resize_at(session, 100, 30, now));

        for failure in 0..=PROTOCOL_RETRY_LIMIT {
            let intent = ui.take_protocol_intent().expect("final resize attempt");
            ui.complete_protocol(WorkspaceProtocolCompletion {
                operation: intent.operation(),
                generation: intent.generation(),
                result: Err(WorkspaceProtocolErrorCode::Busy),
            });
            if failure < PROTOCOL_RETRY_LIMIT {
                let retry = ui.resize_retry.get_mut(&session).expect("scheduled retry");
                retry.retry_at = Some(
                    std::time::Instant::now()
                        .checked_sub(std::time::Duration::from_millis(1))
                        .unwrap(),
                );
                let pass = 100 + u64::from(failure);
                ui.stage_terminal_resize_for_pass(pass, false, session, 100, 30);
                ui.flush_render_side_effects_for_pass(&ctx, pass, false);
            }
        }

        assert_eq!(ui.failed_resize_targets.get(&session), Some(&(100, 30)));
        assert!(!ui.resize_retry.contains_key(&session));
        assert!(ui.protocol_intents.is_empty());
        assert!(!ui.split_final_resize_sessions.contains(&session));
        assert_eq!(ui.settle_session_resize_presentation(session, now), None);
        assert_eq!(ui.sessions[&session].snapshot.as_ref().unwrap().cols, 80);
        assert!(ui.sessions[&session].resize_presentation.is_none());
    }

    #[test]
    fn one_matching_ack_produces_at_most_one_distinct_resize_per_session() {
        let now = std::time::Instant::now();
        let mut ui = WorkspaceUi::new();
        ui.split_final_resize_sessions = HashSet::from([SessionId(71), SessionId(72)]);
        ui.sent_sizes.insert(SessionId(71), (80, 24));
        ui.sent_sizes.insert(SessionId(72), (80, 24));

        assert!(ui.apply_split_final_resize_at(SessionId(71), 100, 30, now));
        assert!(ui.apply_split_final_resize_at(SessionId(72), 50, 30, now));
        assert!(!ui.apply_split_final_resize_at(SessionId(71), 100, 30, now));
        assert!(!ui.apply_split_final_resize_at(SessionId(72), 50, 30, now));

        let commands = drain_protocol(&mut ui);
        assert_eq!(
            commands
                .iter()
                .filter(|command| matches!(command, RuntimeCommand::ResizeTracked { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn split_shell_blank_seed_is_withheld_until_first_nonblank_viewport() {
        let new_session = SessionId(42);
        let blank = shaped_snapshot(80, 24, "");
        let prompt = shaped_snapshot(80, 24, "prompt ready");
        let mut ui = WorkspaceUi::new();
        ui.handle_events(
            &[
                RuntimeEvent::MuxUpdated {
                    snapshot: split_mux_snapshot(0.5),
                },
                RuntimeEvent::ShellSpawned {
                    session: new_session,
                },
                RuntimeEvent::Viewport {
                    session: new_session,
                    snapshot: Arc::clone(&blank),
                    bracketed_paste: false,
                },
            ],
            &catalog(),
        );

        let view = ui.sessions.get(&new_session).expect("new split view");
        assert!(view.snapshot.is_none(), "blank seed became visible");
        assert!(view.initial_presentation.is_some());

        ui.handle_events(
            &[RuntimeEvent::Viewport {
                session: new_session,
                snapshot: Arc::clone(&prompt),
                bracketed_paste: true,
            }],
            &catalog(),
        );
        let view = ui.sessions.get(&new_session).unwrap();
        assert!(Arc::ptr_eq(view.snapshot.as_ref().unwrap(), &prompt));
        assert!(view.initial_presentation.is_none());
        assert!(view.bracketed_paste);
    }

    #[test]
    fn split_shell_blank_seed_is_bounded_by_250ms() {
        let started = std::time::Instant::now();
        let blank = shaped_snapshot(80, 24, "");
        let mut view = SessionView::default();
        view.arm_initial_presentation(started);
        assert!(view.buffer_initial_snapshot(
            Arc::clone(&blank),
            started + std::time::Duration::from_millis(249)
        ));
        view.settle_initial_presentation(started + std::time::Duration::from_millis(249));
        assert!(view.snapshot.is_none());
        view.settle_initial_presentation(started + std::time::Duration::from_millis(250));
        assert!(Arc::ptr_eq(view.snapshot.as_ref().unwrap(), &blank));
        assert!(view.initial_presentation.is_none());
    }

    #[test]
    fn ordinary_existing_session_blank_output_is_not_misclassified_as_seed() {
        let session = SessionId(41);
        let blank = shaped_snapshot(80, 24, "");
        let mut ui = WorkspaceUi::new();
        ui.handle_events(
            &[
                RuntimeEvent::MuxUpdated {
                    snapshot: split_mux_snapshot(0.5),
                },
                RuntimeEvent::Viewport {
                    session,
                    snapshot: Arc::clone(&blank),
                    bracketed_paste: false,
                },
            ],
            &catalog(),
        );
        let view = ui.sessions.get(&session).unwrap();
        assert!(Arc::ptr_eq(view.snapshot.as_ref().unwrap(), &blank));
        assert!(view.initial_presentation.is_none());
    }

    #[test]
    fn discarded_pass_does_not_consume_final_resize_arm() {
        let ctx = egui::Context::default();
        let session = SessionId(81);
        let mut ui = WorkspaceUi::new();
        ui.sent_sizes.insert(session, (80, 24));
        ui.split_final_resize_sessions.insert(session);

        ui.stage_terminal_resize_for_pass(50, false, session, 100, 30);
        ui.flush_render_side_effects_for_pass(&ctx, 50, true);
        assert!(ui.protocol_intents.is_empty());
        assert!(ui.split_final_resize_sessions.contains(&session));

        ui.stage_terminal_resize_for_pass(51, false, session, 100, 30);
        ui.flush_render_side_effects_for_pass(&ctx, 51, false);
        assert!(matches!(
            drain_protocol(&mut ui).as_slice(),
            [RuntimeCommand::ResizeTracked { session: sent, cols: 100, rows: 30, .. }] if *sent == session
        ));
        assert!(!ui.split_final_resize_sessions.contains(&session));
    }

    #[test]
    fn warm_split_shell_replay_does_not_extend_seed_deadline() {
        let new_session = SessionId(42);
        let mut ui = WorkspaceUi::new();
        let events = [
            RuntimeEvent::MuxUpdated {
                snapshot: split_mux_snapshot(0.5),
            },
            RuntimeEvent::ShellSpawned {
                session: new_session,
            },
        ];
        ui.apply_warm_events(&events, &catalog());
        let started = ui
            .sessions
            .get(&new_session)
            .and_then(|view| view.initial_presentation.as_ref())
            .expect("warm split seed fence")
            .started_at;

        ui.apply_warm_events(
            &[RuntimeEvent::ShellSpawned {
                session: new_session,
            }],
            &catalog(),
        );
        assert_eq!(
            ui.sessions
                .get(&new_session)
                .and_then(|view| view.initial_presentation.as_ref())
                .unwrap()
                .started_at,
            started
        );
    }

    #[test]
    fn followup_layout_session_tab_header_is_two_pixels_shorter() {
        let layout = terminal_pane_layout(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(589.0, 358.0),
        ));
        assert_eq!(layout.header.height(), 27.0);
        assert_eq!(layout.surface.top(), 27.0);
        assert_eq!(layout.content.top(), 33.0);
    }

    #[test]
    fn 분할_터미널은_32pt_헤더와_좌우3_상하6_본문여백을_유지한다() {
        // 상수 조정(헤더 30→34pt, 좌우 여백 6→3pt — 2026-07-18 디자인 트랙)에 맞춘
        // 기대값. 상수의 단일 원천은 TERMINAL_PANE_HEADER_HEIGHT/STREAM_*_PADDING.
        let layout = terminal_pane_layout(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(589.0, 358.0),
        ));
        assert_eq!(layout.header.height(), TERMINAL_PANE_HEADER_HEIGHT);
        assert_eq!(layout.surface.left(), 0.0);
        assert_eq!(layout.surface.right(), 589.0);
        assert_eq!(layout.surface.top(), TERMINAL_PANE_HEADER_HEIGHT);
        assert_eq!(layout.surface.bottom(), 358.0);
        assert_eq!(layout.content.left(), TERMINAL_STREAM_LEFT_PADDING);
        assert_eq!(
            layout.content.top(),
            TERMINAL_PANE_HEADER_HEIGHT + TERMINAL_STREAM_VERTICAL_PADDING
        );
        assert_eq!(
            layout.content.right(),
            589.0 - TERMINAL_STREAM_RIGHT_PADDING
        );
        assert_eq!(
            layout.content.bottom(),
            358.0 - TERMINAL_STREAM_VERTICAL_PADDING
        );
    }

    #[test]
    fn designall_단일pane은_본문내부헤더를_유지한다() {
        let layout_node = LayoutNode::Pane(pane_id("single"));
        assert!(keeps_embedded_pane_header(&layout_node));
        let layout = terminal_pane_layout_with_embedded_header(
            egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(589.0, 358.0)),
            keeps_embedded_pane_header(&layout_node),
        );

        assert_eq!(layout.header.height(), TERMINAL_PANE_HEADER_HEIGHT);
        assert_eq!(layout.surface.top(), TERMINAL_PANE_HEADER_HEIGHT);
        assert_eq!(
            layout.content.top(),
            TERMINAL_PANE_HEADER_HEIGHT + TERMINAL_STREAM_VERTICAL_PADDING
        );
        assert_eq!(
            layout.content.bottom(),
            358.0 - TERMINAL_STREAM_VERTICAL_PADDING
        );
    }

    #[test]
    fn 복원_agent_안내바는_터미널과_겹치지_않는_불투명_차콜영역이다() {
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(589.0, 358.0));
        let layout = terminal_pane_layout_for_state(rect, true, true);
        let notice = layout.archived_notice.expect("복원 안내 바가 있어야 한다");
        let style = archived_agent_notice_style();

        assert_eq!(notice.height(), ARCHIVED_AGENT_NOTICE_HEIGHT);
        assert_eq!(notice.left(), layout.surface.left());
        assert_eq!(notice.right(), layout.surface.right());
        assert_eq!(notice.bottom(), layout.surface.bottom());
        assert_eq!(
            layout.content.bottom(),
            notice.top() - TERMINAL_STREAM_VERTICAL_PADDING
        );
        assert!(!layout.content.intersects(notice));
        assert_eq!(style.fill, egui::Color32::from_rgb(0x1a, 0x1d, 0x23));
        assert_eq!(
            style.separator.color,
            egui::Color32::from_rgb(0x35, 0x3b, 0x45)
        );
        assert_eq!(style.text, egui::Color32::from_rgb(0xb0, 0xb5, 0xbf));
        assert_eq!(style.button_fill, egui::Color32::from_rgb(0x29, 0x2e, 0x37));
        assert_eq!(
            style.button_stroke.color,
            egui::Color32::from_rgb(0x48, 0x50, 0x5d)
        );
        assert_eq!(style.button_height, 26.0);
        assert_eq!(style.button_corner_radius, 4);

        let compact = terminal_pane_layout_for_state(
            egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(80.0, 42.0)),
            true,
            true,
        );
        let compact_notice = compact
            .archived_notice
            .expect("낮은 pane도 안내 바를 유지한다");
        assert!(compact.surface.is_positive());
        assert!(compact.content.is_positive());
        assert!(compact_notice.is_positive());
        assert!(!compact.content.intersects(compact_notice));
    }

    /// 상단선은 헤더 **첫 물리행부터** 덮어야 한다. 예전에는 round_to_pixel_center를
    /// 썼는데 그건 홀수 픽셀 폭 전용이라, header.top()이 소수일 때 첫 행이 비어
    /// 1px 여백처럼 보였다(2026-08-07 사용자).
    #[test]
    fn 헤더_상단선은_첫_물리행부터_덮는다() {
        let width = 1.0;
        for ppp in [1.0_f32, 2.0, 3.0] {
            for header_top in [38.0_f32, 38.5, 100.0, 100.25, 7.3] {
                let y = pane_header_top_line_y(header_top, width, ppp);
                let line_top_px = (y - width * 0.5) * ppp;
                let header_top_px = header_top * ppp;
                // 선 윗변이 헤더 상단보다 아래로 내려가면 그만큼 빈 띠가 생긴다.
                assert!(
                    line_top_px <= header_top_px + 0.001,
                    "ppp {ppp}, top {header_top}: 선 위에 {:.2}물리픽셀 여백",
                    line_top_px - header_top_px
                );
                // 선 윗변은 물리픽셀 경계여야 뭉개지지 않는다.
                assert!(
                    (line_top_px - line_top_px.round()).abs() < 0.001,
                    "ppp {ppp}, top {header_top}: 선 윗변 {line_top_px}이 픽셀 경계가 아니다"
                );
            }
        }
    }

    /// 2026-08-08: pane 전체 플래시가 풀 채도 시안(selection.bg_fill)을 알파 255로
    /// 2px×4변에 그려 화면에서 가장 세게 튀었다. 상단선과 같은 워크스페이스 색·채도
    /// 규칙을 쓰고 최대 알파를 낮춘다. 값을 되돌리면 이 테스트가 잡는다.
    #[test]
    fn pane_플래시는_워크스페이스색을_낮춘_채도로_쓰고_알파를_제한한다() {
        let identity = crate::ui::file_tree::WORKSPACE_ACCENT_SAMPLE;

        let peak = pane_flash_color(identity, 1.0);
        assert_eq!(
            peak.a(),
            PANE_FLASH_PEAK_ALPHA as u8,
            "가장 셀 때도 불투명하면 안 된다"
        );
        assert!(peak.a() < 255, "알파 255는 예전의 튀던 값이다");

        // 색은 상단선과 같은 규칙 — 같은 채도로 낮춘 워크스페이스 고유색이다.
        // Color32는 프리멀티플라이 저장이라 `.r()`은 알파가 곱해진 값이다. 기대값도
        // 같은 생성자를 태워 비교한다(원본 채널과 직접 비교하면 알파만큼 어긋난다).
        let header = desaturate(identity, PANE_HEADER_IDENTITY_SATURATION);
        assert_eq!(
            peak,
            egui::Color32::from_rgba_unmultiplied(
                header.r(),
                header.g(),
                header.b(),
                PANE_FLASH_PEAK_ALPHA as u8
            ),
            "플래시와 상단선이 다른 색 계통이면 무엇이 반응했는지 안 읽힌다"
        );

        // 채도가 실제로 낮아졌는지 — 알파를 되돌린 뒤 원본과 채널 폭을 비교한다.
        let spread = |channels: [u8; 4]| {
            let [r, g, b, _] = channels;
            i32::from(r.max(g).max(b)) - i32::from(r.min(g).min(b))
        };
        assert!(
            spread(peak.to_srgba_unmultiplied()) < spread(identity.to_array()),
            "원본 채도 그대로면 낮춘 의미가 없다"
        );

        // 페이드는 0까지 내려가고, 범위를 벗어난 입력도 안전하다.
        assert_eq!(pane_flash_color(identity, 0.0).a(), 0);
        assert_eq!(
            pane_flash_color(identity, 5.0).a(),
            PANE_FLASH_PEAK_ALPHA as u8,
            "1.0을 넘겨도 최대치를 넘지 않는다"
        );
        assert_eq!(pane_flash_color(identity, -1.0).a(), 0);
    }

    #[test]
    fn designall_pane_header는_1px탑라인을_close옆에서끝낸다() {
        // 상단선은 accent가 아니라 **그 pane이 속한 워크스페이스 고유색**이다.
        // accent를 쓰면 경계 드래그 라인과 같은 청록이 돼 구분되지 않는다.
        let identity = crate::ui::file_tree::WORKSPACE_ACCENT_SAMPLE;
        let style = pane_header_style(identity, true);
        assert_eq!(
            style.background,
            terminal::renderer_egui::TERMINAL_SURFACE_BG
        );
        assert_eq!(style.selection_fill, None);
        assert_ne!(
            style.active_stroke.map(|stroke| stroke.color),
            Some(crate::ui::designall::DARK.accent.gamma_multiply(0.55)),
            "상단선이 accent면 경계 드래그 라인과 구분되지 않는다"
        );
        assert_eq!(
            style.active_stroke,
            Some(egui::Stroke::new(
                1.0,
                desaturate(identity, PANE_HEADER_IDENTITY_SATURATION).gamma_multiply(0.55)
            ))
        );
        // 탈채도는 **채도만** 낮춘다 — 밝기까지 떨어뜨리면 포커스 신호가 약해진다.
        let toned = desaturate(identity, PANE_HEADER_IDENTITY_SATURATION);
        let luma = |c: egui::Color32| {
            0.2126 * f32::from(c.r()) + 0.7152 * f32::from(c.g()) + 0.0722 * f32::from(c.b())
        };
        assert!(
            (luma(toned) - luma(identity)).abs() < 2.0,
            "밝기가 바뀌었다: {} -> {}",
            luma(identity),
            luma(toned)
        );
        let spread = |c: egui::Color32| {
            let v = [c.r(), c.g(), c.b()];
            f32::from(v.iter().copied().max().unwrap() - v.iter().copied().min().unwrap())
        };
        assert!(spread(toned) < spread(identity), "채도가 안 낮아졌다");
        let header = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(300.0, 32.0));
        let close = egui::Rect::from_min_max(egui::pos2(100.0, 6.0), egui::pos2(120.0, 26.0));
        let boundary = pane_header_active_boundary(header, close);
        assert_eq!(boundary, 126.0);
        assert!(boundary < header.right());

        let source = include_str!("workspace.rs");
        let start = source.find("    fn render_pane_header(").unwrap();
        let end = source[start..]
            .find("\n    fn activate_terminal_toolbar(")
            .map(|offset| start + offset)
            .unwrap();
        assert!(!source[start..end].contains("painter.vline("));
    }

    #[test]
    fn 소형_분할에서도_헤더와_본문이_뒤집히지_않는다() {
        let layout = terminal_pane_layout(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(30.0, 40.0),
        ));
        assert!(layout.header.is_positive());
        assert!(layout.surface.is_positive());
        assert!(layout.content.is_positive());
        assert!(layout.surface.contains_rect(layout.content));
    }

    #[test]
    fn 가로와_세로_split의_모든_leaf가_같은_밀착형_layout을_갖는다() {
        let slots = [
            // 좌우 분할
            egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(294.0, 300.0)),
            egui::Rect::from_min_size(egui::pos2(295.0, 0.0), egui::vec2(294.0, 300.0)),
            // 상하 분할
            egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(589.0, 149.0)),
            egui::Rect::from_min_size(egui::pos2(0.0, 150.0), egui::vec2(589.0, 150.0)),
        ];
        for slot in slots {
            let layout = terminal_pane_layout(slot);
            assert_eq!(layout.header.left(), slot.left());
            assert_eq!(layout.header.right(), slot.right());
            assert_eq!(layout.header.height(), TERMINAL_PANE_HEADER_HEIGHT);
            assert_eq!(layout.surface.left(), slot.left());
            assert_eq!(layout.surface.right(), slot.right());
            assert_eq!(layout.surface.bottom(), slot.bottom());
            assert_eq!(
                layout.content.left() - layout.surface.left(),
                TERMINAL_STREAM_LEFT_PADDING
            );
            assert_eq!(
                layout.content.top() - layout.surface.top(),
                TERMINAL_STREAM_VERTICAL_PADDING
            );
            assert_eq!(
                layout.surface.right() - layout.content.right(),
                TERMINAL_STREAM_RIGHT_PADDING
            );
            assert_eq!(
                layout.surface.bottom() - layout.content.bottom(),
                TERMINAL_STREAM_VERTICAL_PADDING
            );
        }
    }

    #[test]
    fn 분할_최소크기는_모든_leaf의_축방향_50px를_합산한다() {
        let leaf = || LayoutNode::Pane(pane_id("leaf"));
        let columns = LayoutNode::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(leaf()),
            second: Box::new(leaf()),
        };
        let rows = LayoutNode::Split {
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(leaf()),
            second: Box::new(leaf()),
        };

        assert_eq!(
            terminal_layout_min_size(&columns),
            egui::vec2(
                TERMINAL_PANE_MIN_SIZE * 2.0 + TERMINAL_SPLIT_GAP,
                TERMINAL_PANE_MIN_SIZE
            )
        );
        assert_eq!(
            terminal_layout_min_size(&rows),
            egui::vec2(
                TERMINAL_PANE_MIN_SIZE,
                TERMINAL_PANE_MIN_SIZE * 2.0 + TERMINAL_SPLIT_GAP
            )
        );

        let nested_columns = LayoutNode::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(columns),
            second: Box::new(leaf()),
        };
        assert_eq!(
            terminal_layout_min_size(&nested_columns),
            egui::vec2(
                TERMINAL_PANE_MIN_SIZE * 3.0 + TERMINAL_SPLIT_GAP * 2.0,
                TERMINAL_PANE_MIN_SIZE
            ),
            "같은 축의 중첩 split도 각 leaf의 50px를 보존해야 한다"
        );
    }

    #[test]
    fn 분할_minimum측정은_각_node를_한번만_기록한다() {
        let leaf = || LayoutNode::Pane(pane_id("leaf"));
        let first = LayoutNode::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(leaf()),
            second: Box::new(leaf()),
        };
        let tree = LayoutNode::Split {
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(first),
            second: Box::new(leaf()),
        };

        let metrics = terminal_layout_metrics(&tree);
        assert_eq!(metrics.len(), 5, "split 2개 + leaf 3개를 각각 한 번만 측정");
        assert_eq!(metrics[0].subtree_len, 5);
        assert_eq!(metrics[1].subtree_len, 3);
        assert_eq!(metrics[4].subtree_len, 1);
        assert_eq!(metrics[0].min_size, terminal_layout_min_size(&tree));
    }

    #[test]
    fn 분할_ratio는_좌우와_상하_모두_각_pane의_50px에서_멈춘다() {
        let first = LayoutNode::Pane(pane_id("first"));
        let second = LayoutNode::Pane(pane_id("second"));
        let first_min = terminal_layout_min_size(&first);
        let second_min = terminal_layout_min_size(&second);
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(501.0, 501.0));

        for direction in [SplitDirection::Horizontal, SplitDirection::Vertical] {
            let low = terminal_split_ratio(rect, direction, 0.0, first_min, second_min);
            let high = terminal_split_ratio(rect, direction, 1.0, first_min, second_min);
            let axis = match direction {
                SplitDirection::Horizontal => rect.width(),
                SplitDirection::Vertical => rect.height(),
            } - TERMINAL_SPLIT_GAP;

            assert!((axis * low - TERMINAL_PANE_MIN_SIZE).abs() < 0.0001);
            assert!((axis * (1.0 - high) - TERMINAL_PANE_MIN_SIZE).abs() < 0.0001);
        }
    }

    #[test]
    fn 분할_ratio는_비대칭_subtree의_정확한_minimum에서도_panic하지_않는다() {
        let first_min = egui::vec2(50.0, 50.0);
        let second_min = egui::vec2(152.0, 152.0);
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(203.0, 203.0));

        for direction in [SplitDirection::Horizontal, SplitDirection::Vertical] {
            let ratio = terminal_split_ratio(rect, direction, 0.5, first_min, second_min);
            let available = match direction {
                SplitDirection::Horizontal => rect.width(),
                SplitDirection::Vertical => rect.height(),
            } - TERMINAL_SPLIT_GAP;
            assert!(ratio.is_finite());
            assert!((available * ratio - 50.0).abs() < 0.0001);
            assert!((available * (1.0 - ratio) - 152.0).abs() < 0.0001);
        }
    }

    #[test]
    fn 분할_ratio는_중첩_minimum과_물리적으로_좁은_창을_안전하게_처리한다() {
        let leaf = || LayoutNode::Pane(pane_id("leaf"));
        let two_columns = LayoutNode::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(leaf()),
            second: Box::new(leaf()),
        };
        let right = leaf();
        let exact_width = TERMINAL_PANE_MIN_SIZE * 3.0 + TERMINAL_SPLIT_GAP * 2.0;
        let exact = egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(exact_width, TERMINAL_PANE_MIN_SIZE),
        );
        let two_columns_min = terminal_layout_min_size(&two_columns);
        let right_min = terminal_layout_min_size(&right);
        let ratio = terminal_split_ratio(
            exact,
            SplitDirection::Horizontal,
            0.0,
            two_columns_min,
            right_min,
        );
        assert!(((exact.width() - TERMINAL_SPLIT_GAP) * ratio - two_columns_min.x).abs() < 0.0001);

        // 세 leaf의 최소 합보다 좁으면 불가능한 50px를 가장하지 않고, 필요한 크기에
        // 비례해 공간을 나눈다. NaN snapshot도 같은 안전한 경로로 수렴한다.
        let narrow =
            egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(120.0, TERMINAL_PANE_MIN_SIZE));
        let expected = two_columns_min.x / (two_columns_min.x + right_min.x);
        let constrained = terminal_split_ratio(
            narrow,
            SplitDirection::Horizontal,
            f32::NAN,
            two_columns_min,
            right_min,
        );
        assert!((constrained - expected).abs() < 0.0001);
        assert!(constrained.is_finite());
    }

    #[test]
    fn 분할_geometry는_축이_구분선보다_작아도_음수_rect를_만들지_않는다() {
        let first = LayoutNode::Pane(pane_id("first"));
        let second = LayoutNode::Pane(pane_id("second"));
        let first_min = terminal_layout_min_size(&first);
        let second_min = terminal_layout_min_size(&second);

        for (direction, size) in [
            (SplitDirection::Horizontal, egui::vec2(0.5, 100.0)),
            (SplitDirection::Vertical, egui::vec2(100.0, 0.5)),
        ] {
            let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
            let ratio = terminal_split_ratio(rect, direction, 0.5, first_min, second_min);
            let (first_rect, second_rect, gap_rect) = terminal_split_rects(rect, direction, ratio);
            let hit_rect = terminal_split_hit_rect(rect, gap_rect, direction);

            for child in [first_rect, second_rect, gap_rect, hit_rect] {
                assert!(child.width() >= 0.0, "음수 width: {child:?}");
                assert!(child.height() >= 0.0, "음수 height: {child:?}");
                assert!(child.left() >= rect.left() && child.right() <= rect.right());
                assert!(child.top() >= rect.top() && child.bottom() <= rect.bottom());
            }
        }
    }

    /// codex 리뷰 P2 재현 가드 — compact 헤더(3e3e909)는 visible_toolbar를
    /// clamp(1,·)로 최소 1개 강제해, 당시 최소 pane(58.9px)에서
    /// SplitRows 버튼([30.9, 54.9])이 닫기(×) 히트박스 22px 중 17.1px([26, 48])를
    /// 덮었다. 도구 interact가 나중에 등록되므로 겹침 클릭은 닫기 대신 분할을
    /// 실행했다. 지금은 도구 0개 허용 + 겹침 시 왼쪽 도구 추가 숨김으로 닫기가
    /// 항상 우선한다.
    #[test]
    fn 지원되는_모든_좁은_split에서_도구가_닫기를_덮지_않는다() {
        // 현재 px 기반 최소 split 크기 — 정확히 50px pane.
        let narrow = egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(TERMINAL_PANE_MIN_SIZE, TERMINAL_PANE_HEADER_HEIGHT),
        );
        for title_width in [0.0_f32, 13.0, 26.0, 70.0, 130.0] {
            let buttons = pane_header_buttons(narrow, title_width, 4, 0.0);
            assert!(
                buttons.toolbar.is_empty(),
                "50px pane은 도구 0개가 정상 (title_width {title_width})"
            );
            assert!(
                narrow.contains_rect(buttons.close),
                "닫기는 헤더 안에 남는다 (title_width {title_width})"
            );
        }
        // 폭·제목 폭 sweep — 어떤 조합에서도 도구 히트박스가 닫기를 덮지 않는다.
        for width_quarter in 96..=3200u32 {
            let width = width_quarter as f32 * 0.25; // 24.0 ..= 800.0
            let header = egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(width, TERMINAL_PANE_HEADER_HEIGHT),
            );
            for title_width in [0.0_f32, 13.0, 40.0, 90.0, 200.0] {
                let buttons = pane_header_buttons(header, title_width, 4, 0.0);
                for rect in &buttons.toolbar {
                    assert!(
                        !rect.intersects(buttons.close),
                        "width {width} title {title_width}: 도구 {rect:?}가 닫기 {:?}를 덮는다",
                        buttons.close
                    );
                }
            }
        }
    }

    /// hover는 filesystem을 읽지 않고 exact host completion 뒤에만 Dir를 노출한다.
    #[test]
    fn hover_path는_host_completion_후_폴더를_dir로_잡는다() {
        let mut ui = WorkspaceUi::new();
        let session = SessionId(1);
        ui.set_session_pids(&[(session, std::process::id())]);
        ui.set_session_cwds(
            HashMap::from([(session, "/workspace".to_owned())]),
            crate::config::SessionNameStyle::default(),
        );
        assert_eq!(ui.resolve_path_cached(session, "src"), None);
        let intent = ui.take_io_intent().expect("one path intent");
        let (operation, generation) = match intent {
            WorkspaceIoIntent::ResolvePath {
                operation,
                generation,
                session: target,
                word,
                ..
            } => {
                assert_eq!(target, session);
                assert_eq!(word, "src");
                (operation, generation)
            }
            other => panic!("unexpected intent: {other:?}"),
        };
        ui.complete_io(WorkspaceIoCompletion::PathResolved {
            operation,
            generation,
            result: Some(WorkspacePathResolution {
                kind: WorkspacePathKind::Directory,
                path: WorkspacePathPayload::try_new(PathBuf::from("/workspace/src")).unwrap(),
            }),
        });
        assert!(matches!(
            ui.resolve_path_cached(session, "src"),
            Some(PathClick::Dir(_))
        ));
    }

    fn pane_id(name: &str) -> MuxPaneId {
        MuxPaneId(name.to_owned())
    }

    /// A1 회귀 — 세션이 하나도 없는 워크스페이스에서도 이력 본문 rect가 나와야 한다.
    /// 예전에는 mux가 없으면 「새 셸」 프롬프트만 그리고 기본 output으로 조기 반환해
    /// 레일만 켜진 채 화면이 아무 반응도 하지 않았다.
    #[test]
    fn 세션없는_워크스페이스에서도_활성_이력탭은_본문rect를_준다() {
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let config = TerminalConfig::default();
        let mut ws = WorkspaceUi::new();
        ws.set_aux_tabs(vec![PaneAuxTab {
            kind: PaneAuxTabKind::History,
            label: "History".to_owned(),
            active: true,
        }]);
        assert!(ws.mux.is_none(), "세션이 없는 상태를 전제로 한다");

        let context = egui::Context::default();
        let mut output = None;
        context
            .run_ui(egui::RawInput::default(), |ui| {
                output = Some(ws.show_with_input(ui, &config, &[], &catalog, true));
            })
            .drop_without_applying_deltas();

        let output = output.expect("렌더가 돌아야 한다");
        let body = output
            .aux_body_rect
            .expect("세션이 없어도 이력 본문 rect가 있어야 한다");
        assert!(body.height() > 0.0, "본문 높이가 0이면 안 된다");
        assert!(
            body.top() >= TERMINAL_PANE_HEADER_HEIGHT.min(body.bottom()),
            "본문은 탭 스트립 아래에서 시작해야 한다"
        );
    }

    /// 문서 탭이 활성이면 **실제 세션이 있어도** pane 본문 rect가 App으로 넘어가고
    /// 터미널 표면·입력은 렌더되지 않는다 — 이력·Git과 같은 fail-closed 규칙(설계
    /// "터미널이 멀쩡해야 한다" 합격 기준의 반대쪽: 문서가 활성인 동안은 반대로
    /// 막혀야 한다). 문서 탭이 **여러 개**(멀티 문서 탭 설계) 동시에 있어도 그중
    /// 하나만 활성이면 같은 규칙이 적용돼야 한다 — 비활성 문서 탭이 몇 개 더 있다고
    /// fail-closed가 느슨해지면 안 된다.
    #[test]
    fn 문서탭이_활성이면_세션이_있어도_본문rect를_주고_터미널을_건너뛴다() {
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let config = TerminalConfig::default();
        let mut ws = WorkspaceUi::new();
        ws.mux = Some(mux(
            "t",
            vec![tab(
                "t",
                vec![pane("p", SessionId(7))],
                LayoutNode::Pane(pane_id("p")),
            )],
            "p",
        ));
        ws.set_aux_tabs(vec![
            PaneAuxTab {
                kind: PaneAuxTabKind::Document(DocumentTabId(1)),
                label: "note.md".to_owned(),
                active: true,
            },
            PaneAuxTab {
                kind: PaneAuxTabKind::Document(DocumentTabId(2)),
                label: "other.md".to_owned(),
                active: false,
            },
            PaneAuxTab {
                kind: PaneAuxTabKind::Document(DocumentTabId(3)),
                label: "third.md".to_owned(),
                active: false,
            },
        ]);

        let context = egui::Context::default();
        let mut output = None;
        context
            .run_ui(egui::RawInput::default(), |ui| {
                output = Some(ws.show_with_input(ui, &config, &[], &catalog, true));
            })
            .drop_without_applying_deltas();

        let output = output.expect("렌더가 돌아야 한다");
        let body = output
            .aux_body_rect
            .expect("문서 탭 활성 중에는 본문 rect가 있어야 한다");
        assert!(body.height() > 0.0, "본문 높이가 0이면 안 된다");
        assert!(
            drain_protocol(&mut ws).is_empty(),
            "문서 탭 활성 중에는 어떤 protocol intent도 나가면 안 된다(터미널 fail-closed)"
        );
    }

    /// 위 테스트(정적 aux_body_rect·protocol 확인)를 실제 키 입력으로 재확인한다 —
    /// 문서 편집기(`ui::document::source_editor`)가 포커스를 쥐고 실제로 타이핑해도
    /// 터미널로 새지 않는다. 문서 탭 활성 중에는 터미널 표면 자체가 그려지지 않아
    /// 구조적으로 불가능하지만(위 테스트), "TextEdit 포커스 상태에서 타이핑"이라는
    /// 구체적 시나리오(설계 §10 수동 항목)를 직접 재현해 회귀를 잡는다. 문서 탭이
    /// 여러 개 열려 있는 상태(멀티 문서 탭 설계)로 확장했다 — 탭 개수와 무관하게
    /// fail-closed가 지켜져야 한다.
    #[test]
    fn kittest_문서탭_활성중_텍스트편집기_포커스로_타이핑해도_터미널_protocol이_비어있다() {
        use egui_kittest::kittest::Queryable;

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let config = TerminalConfig::default();
        let mut ws = WorkspaceUi::new();
        ws.mux = Some(mux(
            "t",
            vec![tab(
                "t",
                vec![pane("p", SessionId(7))],
                LayoutNode::Pane(pane_id("p")),
            )],
            "p",
        ));
        ws.set_aux_tabs(vec![
            PaneAuxTab {
                kind: PaneAuxTabKind::Document(DocumentTabId(1)),
                label: "note.md".to_owned(),
                active: true,
            },
            PaneAuxTab {
                kind: PaneAuxTabKind::Document(DocumentTabId(2)),
                label: "other.md".to_owned(),
                active: false,
            },
        ]);

        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, state: &mut (WorkspaceUi, String)| {
                let output = state.0.show_with_input(ui, &config, &[], &catalog, true);
                if let Some(body) = output.aux_body_rect {
                    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(body));
                    crate::ui::document::source_editor(
                        &mut child,
                        egui::Id::new("test_document_editor"),
                        &mut state.1,
                        true,
                    );
                }
            },
            (ws, String::new()),
        );
        harness.run();

        harness
            .get_by_role(egui::accesskit::Role::MultilineTextInput)
            .click();
        harness.run();
        harness
            .get_by_role(egui::accesskit::Role::MultilineTextInput)
            .type_text("hello");
        harness.run();

        assert_eq!(
            harness.state().1,
            "hello",
            "문서 TextEdit이 포커스 상태에서 타이핑을 그대로 받아야 한다"
        );
        assert!(
            drain_protocol(&mut harness.state_mut().0).is_empty(),
            "문서 탭 활성 중 TextEdit 타이핑이 터미널 protocol intent로 새면 안 된다\
             (fail-closed)"
        );
    }

    /// 세션이 없고 이력 탭이 **비활성**이면 예전처럼 「새 셸」 진입점이 본문을 쓴다.
    #[test]
    fn 세션없는_워크스페이스의_비활성_이력탭은_본문rect를_주지_않는다() {
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let config = TerminalConfig::default();
        let mut ws = WorkspaceUi::new();
        ws.set_aux_tabs(vec![PaneAuxTab {
            kind: PaneAuxTabKind::History,
            label: "History".to_owned(),
            active: false,
        }]);

        let context = egui::Context::default();
        let mut output = None;
        context
            .run_ui(egui::RawInput::default(), |ui| {
                output = Some(ws.show_with_input(ui, &config, &[], &catalog, true));
            })
            .drop_without_applying_deltas();

        assert_eq!(output.expect("렌더가 돌아야 한다").aux_body_rect, None);
    }

    /// 핸드오프가 고정한 상태 기계 — 레일 재클릭은 탭을 **지우지 않고** 세션으로만
    /// 돌아가고, 탭 제거는 보조 탭 X 전용이다.
    #[test]
    fn 보조탭_상태기계는_레일_탭_세션_닫기_규칙을_지킨다() {
        use PaneAuxTabState::{Closed, OpenActive, OpenInactive};

        assert_eq!(Closed.on_rail_click(), OpenActive, "레일: 닫힘 → 열고 활성");
        assert_eq!(
            OpenInactive.on_rail_click(),
            OpenActive,
            "레일: 열림 → 활성"
        );
        assert_eq!(
            OpenActive.on_rail_click(),
            OpenInactive,
            "레일 재클릭은 세션으로 돌아가되 탭은 남긴다"
        );

        assert_eq!(OpenInactive.on_tab_click(), OpenActive);
        assert_eq!(OpenActive.on_tab_click(), OpenActive);
        assert_eq!(
            Closed.on_tab_click(),
            Closed,
            "없는 탭은 클릭으로 살아나지 않는다"
        );

        assert_eq!(OpenActive.on_session_tab_click(), OpenInactive);
        assert_eq!(OpenInactive.on_session_tab_click(), OpenInactive);

        for state in [Closed, OpenInactive, OpenActive] {
            assert_eq!(state.on_close(), Closed, "보조 탭 X는 항상 탭만 제거한다");
        }

        assert!(OpenActive.is_active());
        assert!(
            !OpenInactive.is_active(),
            "열려 있어도 비활성은 레일을 켜지 않는다"
        );
        assert!(!Closed.is_active());

        // 탭 chrome 존재 여부 — 한 번도 열지 않았거나 보조 탭 X로 닫으면 헤더에 탭이 없다.
        assert!(
            !Closed.is_open(),
            "열기 전에는 헤더에 보조 탭이 없어야 한다"
        );
        assert!(OpenInactive.is_open());
        assert!(OpenActive.is_open());
        assert!(
            !OpenActive.on_close().is_open(),
            "보조 탭 X 뒤에는 탭 chrome이 사라져야 한다"
        );
    }

    /// A1 두 번째 경로 — runtime이 아직 아무 pane도 포커스하지 않은 프레임에서도
    /// 보조 탭 주인이 정해져야 한다(없으면 탭도 본문도 사라진다).
    #[test]
    fn 포커스된_pane이_없어도_보조탭_주인은_첫_pane이다() {
        let layout = LayoutNode::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(LayoutNode::Pane(pane_id("left"))),
            second: Box::new(LayoutNode::Pane(pane_id("right"))),
        };

        assert_eq!(
            aux_tab_owner_pane(&layout, None, None),
            Some(pane_id("left")),
            "포커스가 없으면 layout의 첫 pane이 받는다"
        );
        assert_eq!(
            aux_tab_owner_pane(&layout, None, Some(&pane_id("right"))),
            Some(pane_id("right")),
            "포커스된 pane이 layout 안에 있으면 그것이 받는다"
        );
        assert_eq!(
            aux_tab_owner_pane(&layout, None, Some(&pane_id("other-tab"))),
            Some(pane_id("left")),
            "다른 탭의 focused pane은 이 layout의 주인이 될 수 없다"
        );
    }

    #[test]
    fn 보조탭_owner_focus는_mux_ack전_pending_pane을_우선한다() {
        let previous = pane_id("left");
        let dropped = pane_id("right");
        let layout = LayoutNode::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(LayoutNode::Pane(previous.clone())),
            second: Box::new(LayoutNode::Pane(dropped.clone())),
        };

        assert_eq!(
            aux_tab_owner_pane(&layout, Some(&dropped), Some(&previous)),
            Some(dropped),
            "비포커스 split drop 직후에는 옛 mux focus보다 pending focus가 주인이어야 한다"
        );
    }

    /// 넓은 헤더: 세션 탭 → 보조 탭 → 도구 순으로 겹침 없이 놓인다.
    #[test]
    fn 보조탭은_세션탭_오른쪽에_겹치지_않고_도구_앞에서_끝난다() {
        let header = egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(520.0, TERMINAL_PANE_HEADER_HEIGHT),
        );
        let label_width = pane_aux_tab_label_width(header.width(), 26.0, 1);
        let aux_reserved = pane_aux_tab_width(label_width) + PANE_AUX_TAB_RIGHT_PAD;
        let buttons = pane_header_buttons(header, 180.0, 4, aux_reserved);
        let left = pane_header_active_boundary(header, buttons.close);
        let aux = pane_aux_tab_geometry(header, left, buttons.toolbar_left, label_width, true)
            .expect("520pt 헤더에는 보조 탭이 들어간다");
        let aux_close = aux.close.expect("넓은 헤더에서는 이력 X도 보인다");

        assert!(
            aux.label_width >= PANE_AUX_TAB_MIN_LABEL,
            "라벨이 0폭이면 안 된다"
        );
        assert!(
            aux.tab.left() >= buttons.close.right(),
            "보조 탭은 세션 닫기 오른쪽이다"
        );
        assert!(
            !aux_close.intersects(buttons.close),
            "두 닫기가 겹치면 안 된다"
        );
        assert!(
            aux.tab.right() <= buttons.toolbar_left,
            "보조 탭이 도구를 덮으면 안 된다"
        );
        for rect in &buttons.toolbar {
            assert!(
                !aux.tab.intersects(*rect),
                "보조 탭과 도구가 겹치면 안 된다"
            );
        }
    }

    /// 폭을 1pt씩 훑어도 보조 탭의 어떤 rect도 헤더/도구 경계를 넘지 않고, 라벨은
    /// 0폭이 되지 않는다. 가장 좁은 구간에서는 탭 자체를 접는다.
    #[test]
    fn 좁은_헤더에서_보조탭은_경계를_넘지_않고_결국_접힌다() {
        let mut saw_tab_dropped = false;
        let mut width = 40.0_f32;
        while width <= 600.0 {
            let header = egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(width, TERMINAL_PANE_HEADER_HEIGHT),
            );
            let label_width = pane_aux_tab_label_width(header.width(), 26.0, 1);
            let aux_reserved = pane_aux_tab_width(label_width) + PANE_AUX_TAB_RIGHT_PAD;
            let buttons = pane_header_buttons(header, 120.0, 4, aux_reserved);
            let left = pane_header_active_boundary(header, buttons.close);
            match pane_aux_tab_geometry(header, left, buttons.toolbar_left, label_width, true) {
                None => saw_tab_dropped = true,
                Some(aux) => {
                    assert!(
                        aux.label_width >= PANE_AUX_TAB_MIN_LABEL,
                        "{width}: 라벨 0폭"
                    );
                    assert!(
                        aux.tab.right() <= buttons.toolbar_left.min(header.right()) + 0.001,
                        "{width}: 보조 탭이 도구/헤더 경계를 넘었다"
                    );
                    assert!(
                        aux.tab.left() >= buttons.close.right(),
                        "{width}: 세션 닫기와 겹침"
                    );
                    if let Some(close) = aux.close {
                        assert!(close.right() <= header.right() + 0.001, "{width}: X 초과");
                        assert!(!close.intersects(buttons.close), "{width}: 두 X가 겹침");
                    }
                }
            }
            width += 1.0;
        }
        assert!(
            saw_tab_dropped,
            "가장 좁은 폭에서는 보조 탭 자체를 접어야 한다"
        );
    }

    /// 이력·Git·문서 여러 개(가운데 문서가 활성)가 다 있을 때도 폭을 1pt씩 훑어
    /// 어떤 rect도 툴바/헤더 경계를 넘지 않고 라벨이 0폭이 되지 않는지 확인한다(위
    /// 단일 탭 스윕과 같은 방식, `layout_aux_tabs`가 실제로 쓰는 다중 탭 경로를
    /// 훑는다). 멀티 문서 탭 설계 §5의 새 불변식도 같은 스윕에서 고정한다: **활성
    /// 탭(가운데 문서)은 placements가 비지 않는 한 항상 그 안에 있어야 한다** — 탭이
    /// 여러 개 있는 폭에서 활성 탭을 대신 접어 지워버리면 안 된다.
    #[test]
    fn 좁은_헤더에서_여러_탭도_경계를_넘지_않고_활성_탭을_버리지_않는다() {
        let mut saw_tab_dropped = false;
        let mut saw_close_stripped = false;
        let active_kind = PaneAuxTabKind::Document(DocumentTabId(2));
        let tabs = [
            aux_tab(PaneAuxTabKind::History, "이력", false),
            aux_tab(PaneAuxTabKind::Git, "Git", false),
            aux_tab(PaneAuxTabKind::Document(DocumentTabId(1)), "a.md", false),
            aux_tab(active_kind, "b.md", true),
            aux_tab(PaneAuxTabKind::Document(DocumentTabId(3)), "c.md", false),
        ];
        let mut width = 40.0_f32;
        while width <= 600.0 {
            let header = egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(width, TERMINAL_PANE_HEADER_HEIGHT),
            );
            let aux_reserved: f32 = tabs
                .iter()
                .map(|_tab| {
                    pane_aux_tab_width(pane_aux_tab_label_width(header.width(), 26.0, tabs.len()))
                        + PANE_AUX_TAB_RIGHT_PAD
                })
                .sum();
            let buttons = pane_header_buttons(header, 120.0, 4, aux_reserved);
            let close = buttons.close;
            let placements = layout_aux_tabs(header, close, buttons.toolbar_left, &tabs, |_| 26.0);
            if placements.len() < tabs.len() {
                saw_tab_dropped = true;
            }
            assert!(
                placements.is_empty() || placements.iter().any(|p| p.kind == active_kind),
                "{width}: 활성 탭이 비어있지 않은 배치에서 빠졌다: {placements:?}"
            );
            let mut previous_right: Option<f32> = None;
            for placement in &placements {
                let aux = placement.geometry;
                assert!(
                    aux.label_width >= PANE_AUX_TAB_MIN_LABEL,
                    "{width}: 라벨 0폭"
                );
                assert!(
                    aux.tab.right() <= buttons.toolbar_left.min(header.right()) + 0.001,
                    "{width}: 보조 탭이 도구/헤더 경계를 넘었다"
                );
                assert!(aux.tab.left() >= close.right(), "{width}: 세션 닫기와 겹침");
                if let Some(previous_right) = previous_right {
                    assert!(
                        aux.tab.left() >= previous_right,
                        "{width}: 보조 탭끼리 겹침"
                    );
                }
                previous_right = Some(aux.tab.right());
                match aux.close {
                    Some(rect) => {
                        assert!(rect.right() <= header.right() + 0.001, "{width}: X 초과");
                        assert!(!rect.intersects(close), "{width}: 두 X가 겹침");
                    }
                    None => saw_close_stripped = true,
                }
            }
            width += 1.0;
        }
        assert!(
            saw_close_stripped,
            "좁아지면 ×부터 접혀야 한다(축약 순서 ⓐ)"
        );
        assert!(
            saw_tab_dropped,
            "가장 좁은 폭에서는 탭 자체도 접혀야 한다(축약 순서 ⓑ)"
        );
    }

    /// 라벨은 들어가지만 X까지는 안 들어가는 폭에서는 **X만** 버리고 탭 전환은 남긴다.
    #[test]
    fn 라벨만_들어가는_폭에서는_이력_닫기만_접는다() {
        let header = egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(400.0, TERMINAL_PANE_HEADER_HEIGHT),
        );
        let session_close = egui::Rect::from_center_size(
            egui::pos2(60.0, header.center().y),
            egui::vec2(20.0, 20.0),
        );
        // 라벨 끝(=76+10+20=106) 뒤로 6pt만 남기면 X(중심 +14, 반폭 10, 여백 6)가 못 들어간다.
        let toolbar_left = 112.0;
        let left = pane_header_active_boundary(header, session_close);
        let aux = pane_aux_tab_geometry(header, left, toolbar_left, 20.0, true)
            .expect("라벨은 들어가야 한다");

        assert_eq!(aux.close, None, "자리가 없으면 X만 접는다");
        assert!(aux.label_width >= PANE_AUX_TAB_MIN_LABEL);
        assert!(aux.tab.right() <= toolbar_left + 0.001);
    }

    fn aux_tab(kind: PaneAuxTabKind, label: &str, active: bool) -> PaneAuxTab {
        PaneAuxTab {
            kind,
            label: label.to_owned(),
            active,
        }
    }

    #[test]
    fn 보조_탭_두_개는_겹치지_않고_순서대로_놓인다() {
        let header = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(800.0, 24.0));
        let close = egui::Rect::from_center_size(egui::pos2(120.0, 12.0), egui::vec2(20.0, 20.0));
        let placements = layout_aux_tabs(
            header,
            close,
            700.0,
            &[
                aux_tab(PaneAuxTabKind::History, "이력", false),
                aux_tab(PaneAuxTabKind::Git, "Git", true),
            ],
            |_| 30.0,
        );
        assert_eq!(placements.len(), 2);
        assert_eq!(placements[0].kind, PaneAuxTabKind::History);
        assert_eq!(placements[1].kind, PaneAuxTabKind::Git);
        assert!(
            placements[0].geometry.tab.right() <= placements[1].geometry.tab.left(),
            "두 탭이 겹친다: {:?}",
            placements
                .iter()
                .map(|p| p.geometry.tab)
                .collect::<Vec<_>>()
        );
        assert!(
            placements[1].geometry.tab.right() <= 700.0,
            "toolbar_left를 넘지 않는다"
        );
    }

    #[test]
    fn 폭이_모자라면_탭보다_x부터_먼저_사라진다() {
        // 예전엔 이 폭에서 뒤 탭(Git)이 통째로 사라졌다. 축약 순서 ⓐ(×부터 뺀다)가
        // 생긴 뒤로는 두 탭 다 남고 ×만 접힌다 — 탭이 사라지는 건 ×를 전부 빼도
        // 안 맞을 때뿐이다(설계 §2).
        let header = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(220.0, 24.0));
        let close = egui::Rect::from_center_size(egui::pos2(120.0, 12.0), egui::vec2(20.0, 20.0));
        let placements = layout_aux_tabs(
            header,
            close,
            200.0,
            &[
                aux_tab(PaneAuxTabKind::History, "이력", false),
                aux_tab(PaneAuxTabKind::Git, "Git", true),
            ],
            |_| 30.0,
        );
        assert_eq!(
            placements.len(),
            2,
            "×를 뺀 두 탭 다 들어가야 한다: {placements:?}"
        );
        assert!(
            placements.iter().all(|p| p.geometry.close.is_none()),
            "이 폭에서는 ×가 전부 접혀야 한다: {placements:?}"
        );
        assert!(
            placements
                .iter()
                .all(|p| p.geometry.label_width >= PANE_AUX_TAB_MIN_LABEL),
            "라벨이 0폭이면 안 된다"
        );
    }

    #[test]
    fn 폭이_x를_다_접어도_모자라면_뒤_탭부터_사라진다() {
        // ×를 전부 접어도(ⓐ) 안 들어가는 폭 — 그제서야 탭 자체가 우선순위대로
        // 사라진다(ⓑ). Git이 활성이라(새 불변식) 비활성인 이력이 먼저 빠진다.
        let header = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(220.0, 24.0));
        let close = egui::Rect::from_center_size(egui::pos2(120.0, 12.0), egui::vec2(20.0, 20.0));
        let placements = layout_aux_tabs(
            header,
            close,
            180.0,
            &[
                aux_tab(PaneAuxTabKind::History, "이력", false),
                aux_tab(PaneAuxTabKind::Git, "Git", true),
            ],
            |_| 30.0,
        );
        assert_eq!(
            placements.len(),
            1,
            "이 폭에서는 한 탭만 남아야 한다: {placements:?}"
        );
        assert_eq!(
            placements[0].kind,
            PaneAuxTabKind::Git,
            "이력이 먼저 빠지고 활성 탭(Git)이 남아야 한다"
        );
    }

    /// 축약 순서(멀티 문서 탭 설계 §5) 전 단계를 좌표로 고정한다: 이력·Git·**활성**
    /// 문서가 다 있을 때 폭을 줄이면 ⓐ 비활성 탭(Git → 이력) × 부터 접히고, **활성
    /// 문서의 ×는 맨 마지막에** 접힌다(새 불변식) — ×를 다 접어도 모자라면 ⓑ 같은
    /// 순서(활성 제외)로 탭 자체가 사라지고 나머지는 사다리를 처음부터 다시 타되,
    /// **활성 문서 탭 자체는 끝까지 살아남는다.** ⓒ 활성 문서마저 최소 폭을 못 채우면
    /// 그제서야 배열이 빈다(패닉하지 않는다).
    #[test]
    fn 세_탭의_축약_순서를_좌표로_고정한다() {
        let header = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(900.0, 24.0));
        let close = egui::Rect::from_center_size(egui::pos2(120.0, 12.0), egui::vec2(20.0, 20.0));
        let document = PaneAuxTabKind::Document(DocumentTabId(1));
        let tabs = [
            aux_tab(PaneAuxTabKind::History, "이력", false),
            aux_tab(PaneAuxTabKind::Git, "Git", false),
            aux_tab(document, "note.md", true),
        ];
        let place =
            |toolbar_left: f32| layout_aux_tabs(header, close, toolbar_left, &tabs, |_| 30.0);
        let has_close = |placements: &[AuxTabPlacement], kind: PaneAuxTabKind| {
            placements
                .iter()
                .find(|p| p.kind == kind)
                .expect("탭이 있어야 한다")
                .geometry
                .close
                .is_some()
        };

        // 0단계: 셋 다 ×까지 온전하다.
        let p = place(350.0);
        assert_eq!(p.len(), 3);
        assert!(has_close(&p, PaneAuxTabKind::History));
        assert!(has_close(&p, PaneAuxTabKind::Git));
        assert!(has_close(&p, document));

        // ⓐ-1: 비활성인 Git의 ×가 먼저 접힌다 — 활성 문서는 아직 손대지 않는다.
        let p = place(330.0);
        assert_eq!(p.len(), 3, "{p:?}");
        assert!(has_close(&p, PaneAuxTabKind::History));
        assert!(!has_close(&p, PaneAuxTabKind::Git));
        assert!(has_close(&p, document));

        // ⓐ-2: Git에 이어 이력 ×도 접힌다 — 활성 문서는 여전히 남는다.
        let p = place(310.0);
        assert_eq!(p.len(), 3, "{p:?}");
        assert!(!has_close(&p, PaneAuxTabKind::History));
        assert!(!has_close(&p, PaneAuxTabKind::Git));
        assert!(has_close(&p, document));

        // ⓐ-3: 활성 문서의 ×도 결국 접힌다 — 새 불변식은 "면제"가 아니라 "맨 마지막"이다.
        let p = place(270.0);
        assert_eq!(p.len(), 3, "{p:?}");
        assert!(p.iter().all(|t| t.geometry.close.is_none()));

        // ⓑ-1: ×를 다 접어도 안 맞는다 — 비활성 탭 우선순위 맨 앞(Git)이 통째로
        // 사라진다. 활성 문서는 `aux_tab_inactive_priority`에 없어 뽑히지 않는다.
        let p = place(255.0);
        assert_eq!(p.len(), 2, "{p:?}");
        assert!(p.iter().any(|t| t.kind == PaneAuxTabKind::History));
        assert!(!p.iter().any(|t| t.kind == PaneAuxTabKind::Git));
        assert!(p.iter().any(|t| t.kind == document));
        assert!(!has_close(&p, PaneAuxTabKind::History));
        assert!(has_close(&p, document));

        // ⓑ-2: 이력도 통째로 사라지고 활성 문서만 남는다(×는 다시 붙는다 — 사다리를
        // 처음부터 다시 타므로). 활성 탭은 끝까지 살아남는다는 게 이 단계의 요점이다.
        let p = place(210.0);
        assert_eq!(p.len(), 1, "{p:?}");
        assert_eq!(p[0].kind, document);
        assert!(p[0].geometry.close.is_some());

        // ⓒ: 활성 문서마저 최소 라벨 폭을 못 채우는 폭 — 탭이 하나도 없다. 패닉하지
        // 않고 빈 배열만 돌려줘야 세션 제목이 그대로 남는다.
        let p = place(150.0);
        assert!(p.is_empty(), "{p:?}");
    }

    /// 이력 X는 **UI 탭만** 닫는다 — 세션 닫기 확인이나 ClosePane이 나가면 설계 실패다.
    #[test]
    fn kittest_이력탭_닫기는_세션을_닫지_않고_닫기의도만_올린다() {
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let config = TerminalConfig::default();
        let mut ws = WorkspaceUi::new();
        ws.mux = Some(mux(
            "t",
            vec![tab(
                "t",
                vec![pane("p", SessionId(7))],
                LayoutNode::Pane(pane_id("p")),
            )],
            "p",
        ));
        ws.set_aux_tabs(vec![PaneAuxTab {
            kind: PaneAuxTabKind::History,
            label: "History".to_owned(),
            active: true,
        }]);
        // render_pane_header를 직접 부르는 테스트라, show_with_input이 매 프레임 정하는
        // 보조 탭 주인을 여기서 세운다.
        ws.aux_tab_pane = Some(pane_id("p"));
        let header = egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(520.0, TERMINAL_PANE_HEADER_HEIGHT),
        );
        let snapshot = pane("p", SessionId(7));
        let mut intents = Vec::new();
        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, state: &mut (WorkspaceUi, Vec<Option<(PaneAuxTabKind, PaneAuxTabIntent)>>)| {
                let output = state
                    .0
                    .render_pane_header(ui, header, &snapshot, true, &config, &catalog, true);
                state.1.push(output.aux_tab_intent);
            },
            (ws, std::mem::take(&mut intents)),
        );
        harness.run();
        let aux_close = aux_close_center(&harness.state().0, header, &snapshot);
        harness.state_mut().1.clear();

        harness.hover_at(aux_close);
        harness.run();
        harness.drag_at(aux_close);
        harness.run();
        harness.drop_at(aux_close);
        harness.run();

        assert!(
            harness
                .state()
                .1
                .contains(&Some((PaneAuxTabKind::History, PaneAuxTabIntent::Close))),
            "이력 X는 Close 의도를 올려야 한다"
        );
        assert_eq!(
            harness.state().0.confirm_close,
            None,
            "이력 X가 세션 닫기 확인을 띄우면 안 된다"
        );
        assert!(
            !drain_protocol(&mut harness.state_mut().0)
                .iter()
                .any(|command| matches!(
                    command,
                    RuntimeCommand::ClosePane { .. } | RuntimeCommand::KillSession { .. }
                )),
            "이력 X가 세션/pane 종료 명령을 보내면 안 된다"
        );
    }

    /// 문서 X도 **UI 탭만** 닫는다 — 세션 ×와 끝까지 다른 동작이어야 한다(설계
    /// "보조 탭에서 RuntimeCommand가 파생되면 안 된다 — 문서 ×는 pane을 닫지 않는다").
    /// 이력 X 테스트와 같은 모양이다.
    #[test]
    fn kittest_문서탭_닫기는_pane을_닫지_않고_닫기의도만_올린다() {
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let config = TerminalConfig::default();
        let mut ws = WorkspaceUi::new();
        ws.mux = Some(mux(
            "t",
            vec![tab(
                "t",
                vec![pane("p", SessionId(7))],
                LayoutNode::Pane(pane_id("p")),
            )],
            "p",
        ));
        ws.set_aux_tabs(vec![PaneAuxTab {
            kind: PaneAuxTabKind::Document(DocumentTabId(1)),
            label: "note.md".to_owned(),
            active: true,
        }]);
        ws.aux_tab_pane = Some(pane_id("p"));
        let header = egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(520.0, TERMINAL_PANE_HEADER_HEIGHT),
        );
        let snapshot = pane("p", SessionId(7));
        let mut intents = Vec::new();
        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, state: &mut (WorkspaceUi, Vec<Option<(PaneAuxTabKind, PaneAuxTabIntent)>>)| {
                let output = state
                    .0
                    .render_pane_header(ui, header, &snapshot, true, &config, &catalog, true);
                state.1.push(output.aux_tab_intent);
            },
            (ws, std::mem::take(&mut intents)),
        );
        harness.run();
        let aux_close = aux_close_center(&harness.state().0, header, &snapshot);
        harness.state_mut().1.clear();

        harness.hover_at(aux_close);
        harness.run();
        harness.drag_at(aux_close);
        harness.run();
        harness.drop_at(aux_close);
        harness.run();

        assert!(
            harness.state().1.contains(&Some((
                PaneAuxTabKind::Document(DocumentTabId(1)),
                PaneAuxTabIntent::Close
            ))),
            "문서 X는 Close 의도를 올려야 한다"
        );
        assert_eq!(
            harness.state().0.confirm_close,
            None,
            "문서 X가 세션 닫기 확인을 띄우면 안 된다"
        );
        assert!(
            !drain_protocol(&mut harness.state_mut().0)
                .iter()
                .any(|command| matches!(
                    command,
                    RuntimeCommand::ClosePane { .. } | RuntimeCommand::KillSession { .. }
                )),
            "문서 X가 세션/pane 종료 명령을 보내면 안 된다"
        );
    }

    /// 이력이 활성인 동안 세션 탭 영역을 누르면 터미널로 돌아가는 의도가 올라간다.
    #[test]
    fn kittest_이력활성중_세션탭_클릭은_터미널복귀_의도다() {
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let config = TerminalConfig::default();
        let mut ws = WorkspaceUi::new();
        ws.mux = Some(mux(
            "t",
            vec![tab(
                "t",
                vec![pane("p", SessionId(7))],
                LayoutNode::Pane(pane_id("p")),
            )],
            "p",
        ));
        ws.set_aux_tabs(vec![PaneAuxTab {
            kind: PaneAuxTabKind::History,
            label: "History".to_owned(),
            active: true,
        }]);
        // render_pane_header를 직접 부르는 테스트라, show_with_input이 매 프레임 정하는
        // 보조 탭 주인을 여기서 세운다.
        ws.aux_tab_pane = Some(pane_id("p"));
        let header = egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(520.0, TERMINAL_PANE_HEADER_HEIGHT),
        );
        let snapshot = pane("p", SessionId(7));
        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, state: &mut (WorkspaceUi, Vec<Option<(PaneAuxTabKind, PaneAuxTabIntent)>>)| {
                let output = state
                    .0
                    .render_pane_header(ui, header, &snapshot, true, &config, &catalog, true);
                state.1.push(output.aux_tab_intent);
            },
            (ws, Vec::new()),
        );
        harness.run();
        harness.state_mut().1.clear();
        // 제목 글자 위 — 세션 탭 영역이고 닫기/도구/보조 탭 어디에도 속하지 않는다.
        let title = egui::pos2(14.0, TERMINAL_PANE_HEADER_HEIGHT * 0.5);

        harness.hover_at(title);
        harness.run();
        harness.drag_at(title);
        harness.run();
        harness.drop_at(title);
        harness.run();

        assert!(
            harness.state().1.contains(&Some((
                PaneAuxTabKind::History,
                PaneAuxTabIntent::ShowSession
            ))),
            "세션 탭 클릭은 ShowSession 의도를 올려야 한다"
        );
    }

    /// 이력 탭이 비활성일 때 라벨을 누르면 활성화 의도가 올라간다.
    #[test]
    fn kittest_비활성_이력탭_클릭은_활성화_의도다() {
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let config = TerminalConfig::default();
        let mut ws = WorkspaceUi::new();
        ws.mux = Some(mux(
            "t",
            vec![tab(
                "t",
                vec![pane("p", SessionId(7))],
                LayoutNode::Pane(pane_id("p")),
            )],
            "p",
        ));
        ws.set_aux_tabs(vec![PaneAuxTab {
            kind: PaneAuxTabKind::History,
            label: "History".to_owned(),
            active: false,
        }]);
        ws.aux_tab_pane = Some(pane_id("p"));
        let header = egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(520.0, TERMINAL_PANE_HEADER_HEIGHT),
        );
        let snapshot = pane("p", SessionId(7));
        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, state: &mut (WorkspaceUi, Vec<Option<(PaneAuxTabKind, PaneAuxTabIntent)>>)| {
                let output = state
                    .0
                    .render_pane_header(ui, header, &snapshot, true, &config, &catalog, true);
                state.1.push(output.aux_tab_intent);
            },
            (ws, Vec::new()),
        );
        harness.run();
        let label = aux_label_center(&harness.state().0, header, &snapshot);
        harness.state_mut().1.clear();

        harness.hover_at(label);
        harness.run();
        harness.drag_at(label);
        harness.run();
        harness.drop_at(label);
        harness.run();

        assert!(
            harness
                .state()
                .1
                .contains(&Some((PaneAuxTabKind::History, PaneAuxTabIntent::Activate))),
            "비활성 이력 탭 클릭은 Activate 의도를 올려야 한다"
        );
    }

    /// 헤더가 실제로 쓴 것과 같은 기하를 다시 계산해 클릭 좌표를 만든다.
    fn aux_tab_geometry_for_test(
        ws: &WorkspaceUi,
        header: egui::Rect,
        snapshot: &runtime::PaneSnapshot,
    ) -> PaneAuxTabGeometry {
        let context = egui::Context::default();
        let mut geometry = None;
        context
            .run_ui(egui::RawInput::default(), |ui| {
                let label = &ws.aux_tabs.first().expect("aux tab set").label;
                let font = egui::FontId::proportional(13.0);
                let natural = ui
                    .painter()
                    .layout_no_wrap(label.clone(), font.clone(), egui::Color32::WHITE)
                    .size()
                    .x;
                let label_width = pane_aux_tab_label_width(header.width(), natural, 1);
                let aux_reserved = pane_aux_tab_width(label_width) + PANE_AUX_TAB_RIGHT_PAD;
                let title_width = ui
                    .painter()
                    .layout_no_wrap(snapshot.title.clone(), font, egui::Color32::WHITE)
                    .size()
                    .x;
                let buttons = pane_header_buttons(header, title_width, 4, aux_reserved);
                let left = pane_header_active_boundary(header, buttons.close);
                geometry =
                    pane_aux_tab_geometry(header, left, buttons.toolbar_left, label_width, true);
            })
            .drop_without_applying_deltas();
        geometry.expect("테스트 헤더에는 보조 탭이 들어간다")
    }

    fn aux_close_center(
        ws: &WorkspaceUi,
        header: egui::Rect,
        snapshot: &runtime::PaneSnapshot,
    ) -> egui::Pos2 {
        aux_tab_geometry_for_test(ws, header, snapshot)
            .close
            .expect("넓은 헤더에는 이력 X가 있다")
            .center()
    }

    fn aux_label_center(
        ws: &WorkspaceUi,
        header: egui::Rect,
        snapshot: &runtime::PaneSnapshot,
    ) -> egui::Pos2 {
        let geometry = aux_tab_geometry_for_test(ws, header, snapshot);
        egui::pos2(
            geometry.label_left + geometry.label_width * 0.5,
            header.center().y,
        )
    }

    #[test]
    fn pane_drop_feedback_marks_surface_and_right_insertion_edge() {
        let style = pane_drop_feedback_style(crate::ui::designall::DARK);

        assert_eq!(style.outline.width, 2.0);
        assert_eq!(style.outline_inset, 2.0);
        assert_eq!(style.insertion_width, 3.0);
        assert_eq!(style.label_fill, egui::Color32::from_rgb(0xed, 0x5b, 0x61));
    }

    #[test]
    fn pane_drop_feedback_elides_long_label_inside_narrow_pane() {
        let context = egui::Context::default();
        let pane = egui::Rect::from_min_size(egui::pos2(10.0, 10.0), egui::vec2(96.0, 64.0));
        let style = pane_drop_feedback_style(crate::ui::designall::DARK);
        let mut measured = None;

        context
            .run_ui(egui::RawInput::default(), |ui| {
                measured = layout_pane_drop_feedback_label(
                    ui.painter(),
                    pane,
                    "선택한 세션을 이 Pane의 오른쪽에 연결합니다",
                    style,
                );
            })
            .drop_without_applying_deltas();

        let measured = measured.expect("narrow pane still has room for a compact label");
        assert!(pane.contains_rect(measured.rect));
        assert_eq!(measured.galley.rows.len(), 1);
        assert!(measured.galley.elided);
        assert_eq!(measured.galley.rows[0].text().chars().last(), Some('…'));
    }

    #[test]
    fn pane_drop_feedback_label_is_centered_in_pane() {
        let context = egui::Context::default();
        let pane = egui::Rect::from_min_size(egui::pos2(20.0, 40.0), egui::vec2(480.0, 320.0));
        let style = pane_drop_feedback_style(crate::ui::designall::DARK);
        let mut measured = None;

        context
            .run_ui(egui::RawInput::default(), |ui| {
                measured = layout_pane_drop_feedback_label(
                    ui.painter(),
                    pane,
                    "현재 화면 오른쪽에 열기",
                    style,
                );
            })
            .drop_without_applying_deltas();

        let measured = measured.expect("pane has room for the feedback label");
        assert!((measured.rect.center().x - pane.center().x).abs() < f32::EPSILON);
        assert!((measured.rect.center().y - pane.center().y).abs() < f32::EPSILON);
        assert_eq!(measured.galley.job.sections[0].format.font_id.size, 13.0);
    }

    #[test]
    fn document_drop_feedback_painter는_terminal_child전_parent_clip에서_조건부로_잡는다() {
        let source = include_str!("workspace.rs");
        let render_pane = source
            .split_once("    fn render_pane(")
            .expect("render_pane 정의")
            .1
            .split_once("    fn agent_send_targets(")
            .expect("render_pane 끝")
            .0;
        let painter = render_pane
            .find("let pane_feedback_painter")
            .expect("document hover일 때만 parent painter를 보관해야 한다");
        let child = render_pane
            .find("let mut terminal_ui = ui.new_child")
            .expect("terminal child 정의");

        assert!(
            painter < child,
            "terminal child painter는 content clip이므로 pane 전체 feedback에 쓰면 안 된다"
        );
        assert!(
            render_pane[painter..child]
                .contains("matches!(drop_feedback, Some(TerminalDropFeedback::DocumentOpen))"),
            "parent painter는 document feedback frame에만 복제해야 한다"
        );
    }

    #[test]
    fn terminal_drop_hover_feedback은_입력종류를_정확히_분리한다() {
        assert_eq!(
            classify_terminal_drop_feedback(false, true, false),
            Some(TerminalDropFeedback::TerminalInsert)
        );
        assert_eq!(
            classify_terminal_drop_feedback(true, false, false),
            Some(TerminalDropFeedback::DocumentOpen)
        );
        assert_eq!(
            classify_terminal_drop_feedback(false, false, true),
            Some(TerminalDropFeedback::DocumentOpen)
        );
        assert_eq!(classify_terminal_drop_feedback(false, false, false), None);
    }

    #[test]
    fn terminal_os_file_drag_repaint는_input_owner의_활성_drag에서만_유지된다() {
        let ctx = egui::Context::default();
        let repaint_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let callback_count = Arc::clone(&repaint_count);
        ctx.set_request_repaint_callback(move |_| {
            callback_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        });
        let before = repaint_count.load(std::sync::atomic::Ordering::Relaxed);

        request_terminal_os_drag_feedback_repaint(&ctx, false, true);
        request_terminal_os_drag_feedback_repaint(&ctx, true, false);
        assert_eq!(
            repaint_count.load(std::sync::atomic::Ordering::Relaxed),
            before,
            "input을 소유하지 않거나 OS drag가 아니면 idle repaint를 추가하면 안 된다"
        );

        request_terminal_os_drag_feedback_repaint(&ctx, true, true);
        assert!(
            repaint_count.load(std::sync::atomic::Ordering::Relaxed) > before,
            "macOS drag 중에만 AppKit 포인터를 다시 샘플링할 다음 frame을 요청해야 한다"
        );
    }

    #[test]
    fn typed_drop_release_preserves_unrelated_payload() {
        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, preserved: &mut bool| {
                let response = ui.allocate_response(ui.available_size(), egui::Sense::hover());
                let _ = release_typed_dnd_payload::<std::path::PathBuf>(&response);
                if ui.input(|input| input.pointer.any_released()) {
                    *preserved = egui::DragAndDrop::payload::<String>(ui.ctx())
                        .is_some_and(|payload| payload.as_str() == "session-payload");
                }
            },
            false,
        );
        harness.run();
        let pointer = egui::pos2(20.0, 20.0);
        harness.hover_at(pointer);
        harness.drag_at(pointer);
        harness.run();
        egui::DragAndDrop::set_payload(&harness.ctx, "session-payload".to_owned());
        harness.event(egui::Event::PointerMoved(pointer));
        harness.event(egui::Event::PointerButton {
            pos: pointer,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        });
        harness.run();

        assert!(harness.state());
    }

    #[test]
    fn pr11_group_release_survives_legacy_and_attachment_consumers() {
        let expected = vec![
            PathBuf::from("/private-pr11/a"),
            PathBuf::from("/private-pr11/b"),
        ];
        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, paths: &mut Vec<PathBuf>| {
                let response = ui.allocate_response(ui.available_size(), egui::Sense::hover());
                assert!(release_typed_dnd_payload::<PathBuf>(&response).is_none());
                assert!(
                    release_typed_dnd_payload::<crate::ui::cross_workspace::AttachmentId>(
                        &response
                    )
                    .is_none()
                );
                if let Some(group) = release_file_dnd_paths(&response) {
                    *paths = group;
                }
            },
            Vec::new(),
        );
        harness.run();
        let point = egui::pos2(20.0, 20.0);
        harness.hover_at(point);
        harness.drag_at(point);
        harness.run();
        egui::DragAndDrop::set_payload(
            &harness.ctx,
            crate::ui::file_tree::FileTreeDragPayload::try_new(
                PathBuf::from("/private-pr11"),
                8,
                expected.clone(),
            )
            .unwrap(),
        );
        harness.event(egui::Event::PointerButton {
            pos: point,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        });
        harness.run();
        assert_eq!(harness.state(), &expected);
    }

    #[test]
    fn pr11_actual_terminal_surface_receives_all_group_and_legacy_paths_without_pty_input() {
        for group in [true, false] {
            let mut harness = setup_focused_local_pane_drop_harness(SessionId(7));
            let paths = vec![
                PathBuf::from("/private-pr11/a.txt"),
                PathBuf::from("/private-pr11/b.txt"),
                PathBuf::from("/private-pr11/c.txt"),
            ];
            let point = egui::pos2(80.0, TERMINAL_PANE_HEADER_HEIGHT + 40.0);
            harness.hover_at(point);
            harness.drag_at(point);
            harness.run();
            if group {
                egui::DragAndDrop::set_payload(
                    &harness.ctx,
                    crate::ui::file_tree::FileTreeDragPayload::try_new(
                        PathBuf::from("/private-pr11"),
                        8,
                        paths.clone(),
                    )
                    .unwrap(),
                );
            } else {
                egui::DragAndDrop::set_payload(&harness.ctx, paths[0].clone());
            }
            harness.event(egui::Event::PointerButton {
                pos: point,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            });
            harness.run();
            assert_eq!(
                harness.state().1.document_drop_paths,
                if group { paths } else { vec![paths[0].clone()] }
            );
            assert_eq!(harness.state().1.local_focus_claimed, Some(pane_id("pane")));
            assert!(written_bytes(drain_protocol(&mut harness.state_mut().0)).is_empty());
        }
    }

    #[test]
    fn foreign_identity_color_is_top_line_only() {
        let identity = egui::Color32::LIGHT_BLUE;
        let style = attached_identity_style(identity);

        assert_eq!(style.top_line, egui::Stroke::new(1.0, identity));
        assert_eq!(
            style.header_fill,
            terminal::renderer_egui::TERMINAL_SURFACE_BG
        );
        assert_eq!(
            style.body_fill,
            terminal::renderer_egui::TERMINAL_SURFACE_BG
        );
    }

    #[test]
    fn attached_target는_지정한_tab의_정확한_pane만_찾는다() {
        let snapshot = mux(
            "active",
            vec![
                tab(
                    "active",
                    vec![pane("same", SessionId(1))],
                    LayoutNode::Pane(pane_id("same")),
                ),
                tab(
                    "foreign",
                    vec![pane("other", SessionId(2))],
                    LayoutNode::Pane(pane_id("other")),
                ),
            ],
            "same",
        );
        let target = AttachedPaneTarget {
            workspace_id: "workspace-b".to_owned(),
            tab: tab_id("foreign"),
            pane: pane_id("same"),
            session: SessionId(1),
        };

        assert!(find_attached_pane(&snapshot, &target).is_none());
    }

    #[test]
    fn attached_target는_reused_pane_id의_다른_session으로_fallback하지_않는다() {
        let snapshot = mux(
            "foreign",
            vec![tab(
                "foreign",
                vec![pane("pane", SessionId(9))],
                LayoutNode::Pane(pane_id("pane")),
            )],
            "pane",
        );
        let stale_target = AttachedPaneTarget {
            workspace_id: "workspace-b".to_owned(),
            tab: tab_id("foreign"),
            pane: pane_id("pane"),
            session: SessionId(7),
        };

        assert!(find_attached_pane(&snapshot, &stale_target).is_none());
    }

    #[test]
    fn attached_surface_i18n은_frozen_key와_workspace_인자를_사용한다() {
        let source = include_str!("workspace.rs");
        let availability_start = source
            .find("impl AttachedPaneAvailability")
            .expect("availability impl");
        let availability_end = source[availability_start..]
            .find("\n}\n\n#[derive(Clone, Copy, Debug, Default")
            .map(|offset| availability_start + offset)
            .expect("availability impl end");
        let availability = &source[availability_start..availability_end];
        assert!(availability.contains("workspace.cross_pane.input_unavailable"));
        assert!(!availability.contains("workspace.cross_pane.unavailable"));

        let show_start = source
            .find("    pub fn show_attached_pane(")
            .expect("show attached pane");
        let show_end = source[show_start..]
            .find("\n    fn render_attached_pane_header(")
            .map(|offset| show_start + offset)
            .expect("show attached pane end");
        let show = &source[show_start..show_end];
        assert!(show.contains("[(\"workspace\", external_workspace_label)]"));

        let header_start = show_end;
        let header_end = source[header_start..]
            .find("\n    fn render_attached_placeholder(")
            .map(|offset| header_start + offset)
            .expect("attached header end");
        let header = &source[header_start..header_end];
        assert!(header.contains("attached_display_title"));
        assert!(!header.contains("workspace.cross_pane.external_source"));
        assert!(!header.contains("format!(\"↗ {external_workspace_label}"));
    }

    #[test]
    fn attached_header_uses_only_precomputed_project_workspace_title() {
        let source = include_str!("workspace.rs");
        let header = source
            .split_once("fn render_attached_pane_header(")
            .expect("attached header")
            .1
            .split_once("fn render_attached_placeholder(")
            .expect("attached header end")
            .0;

        assert!(header.contains("attached_display_title"));
        assert!(!header.contains("workspace.cross_pane.external_source"));
        assert!(!header.contains("pane_title"));
    }

    #[test]
    fn attached_header_renders_supplied_title_without_legacy_source_or_pane_title() {
        let catalog = catalog();
        let workspace = WorkspaceUi::new();
        let target = AttachedPaneTarget {
            workspace_id: "workspace-b".to_owned(),
            tab: tab_id("foreign"),
            pane: pane_id("workspace.spawn.shell 1"),
            session: SessionId(7),
        };
        let context = egui::Context::default();
        let mut output = context.run_ui(egui::RawInput::default(), |ui| {
            let header = egui::Rect::from_min_size(
                ui.available_rect_before_wrap().min,
                egui::vec2(360.0, TERMINAL_PANE_HEADER_HEIGHT),
            );
            workspace.render_attached_pane_header(
                ui,
                header,
                &target,
                "Other",
                "Project (Workspace)",
                &catalog,
                None,
            );
        });
        output.textures_delta.clear();
        let rendered_text = output
            .shapes
            .iter()
            .filter_map(|clipped| match &clipped.shape {
                egui::Shape::Text(text) => Some(text.galley.text()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let legacy_source = catalog.t(
            "workspace.cross_pane.external_source",
            &[("workspace", "Other")],
        );

        assert!(rendered_text.contains("Project (Workspace)"));
        assert!(!rendered_text.contains(legacy_source.as_str()));
        assert!(!rendered_text.contains("workspace.spawn.shell 1"));
    }

    #[test]
    fn attached_header_elides_long_title_before_close_action() {
        let catalog = catalog();
        let workspace = WorkspaceUi::new();
        let target = AttachedPaneTarget {
            workspace_id: "workspace-b".to_owned(),
            tab: tab_id("foreign"),
            pane: pane_id("workspace.spawn.shell 1"),
            session: SessionId(7),
        };
        let long_title = "Extremely Long Project Name (Extremely Long Workspace Name)";
        let header_width = 180.0;
        let mut title_right = None;
        let context = egui::Context::default();
        let mut output = context.run_ui(egui::RawInput::default(), |ui| {
            let header = egui::Rect::from_min_size(
                ui.available_rect_before_wrap().min,
                egui::vec2(header_width, TERMINAL_PANE_HEADER_HEIGHT),
            );
            title_right = Some(header.right() - 15.0 - 12.0 - 4.0);
            workspace.render_attached_pane_header(
                ui, header, &target, "Other", long_title, &catalog, None,
            );
        });
        output.textures_delta.clear();
        let clipped_title = output
            .shapes
            .iter()
            .find(|clipped| {
                matches!(
                    &clipped.shape,
                    egui::Shape::Text(text) if text.galley.text() == long_title
                )
            })
            .expect("attached pane title shape");
        let egui::Shape::Text(title) = &clipped_title.shape else {
            unreachable!("matched text shape")
        };
        let title_right = title_right.expect("title boundary");

        assert!(title.galley.elided);
        assert_eq!(title.galley.rows.len(), 1);
        assert_eq!(
            title
                .galley
                .rows
                .last()
                .and_then(|row| row.glyphs.last())
                .map(|glyph| glyph.chr),
            Some('…')
        );
        assert!(clipped_title.clip_rect.right() <= title_right);
        assert!(title.pos.x + title.galley.size().x <= title_right);
    }

    #[test]
    fn input_disabled_frame은_native_input을_drain하지_않는다() {
        let catalog = catalog();
        let context = egui::Context::default();
        let mut workspace = WorkspaceUi::new();
        workspace.native_printable_key_downs =
            vec![crate::native_key_monitor::NativePrintableKeyDown::for_test(
                '.',
            )];
        workspace.native_clipboard_paste_requested = true;
        workspace.native_clipboard_copy_requested = true;
        let drained = std::cell::Cell::new(false);

        workspace.prepare_frame_with_native_input(&context, &[], &catalog, false, || {
            drained.set(true);
            crate::native_key_monitor::NativeKeyDownBatch::default()
        });

        assert!(!drained.get());
        assert!(workspace.native_printable_key_downs.is_empty());
        assert!(!workspace.native_clipboard_paste_requested);
        assert!(!workspace.native_clipboard_copy_requested);
        assert!(drain_protocol(&mut workspace).is_empty());
    }

    #[test]
    fn correction_round_input_disabled_frame은_pending_copy를_소유한다() {
        let catalog = catalog();
        let context = egui::Context::default();
        let mut workspace = WorkspaceUi::new();
        workspace.pending_copy = Some("owned copy".to_owned());

        let mut disabled_output = context.run_ui(egui::RawInput::default(), |ui| {
            workspace.prepare_frame_with_native_input(ui.ctx(), &[], &catalog, false, || {
                crate::native_key_monitor::NativeKeyDownBatch::default()
            });
        });
        disabled_output.textures_delta.clear();

        assert_eq!(workspace.pending_copy.as_deref(), Some("owned copy"));
        assert!(disabled_output.platform_output.commands.is_empty());

        let mut enabled_output = context.run_ui(egui::RawInput::default(), |ui| {
            workspace.prepare_frame_with_native_input(ui.ctx(), &[], &catalog, true, || {
                crate::native_key_monitor::NativeKeyDownBatch::default()
            });
        });
        enabled_output.textures_delta.clear();

        assert!(workspace.pending_copy.is_none());
        assert!(enabled_output.platform_output.commands.iter().any(
            |command| matches!(command, egui::OutputCommand::CopyText(text) if text == "owned copy")
        ));
    }

    #[test]
    fn correction_round_input_disabled_frame은_close_confirm을_보존한다() {
        use egui_kittest::kittest::Queryable;

        let catalog = catalog();
        let close_label = catalog.t("action.close", &[]);
        let config = TerminalConfig::default();
        let target = pane_id("pane");
        let mut workspace = WorkspaceUi::new();
        workspace.mux = Some(mux(
            "primary",
            vec![tab(
                "primary",
                vec![pane("pane", SessionId(7))],
                LayoutNode::Pane(target.clone()),
            )],
            "pane",
        ));
        workspace.confirm_close = Some(target.clone());
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, state: &mut (WorkspaceUi, bool)| {
                state.0.show_with_input(ui, &config, &[], &catalog, state.1);
            },
            (workspace, false),
        );

        harness.run();

        assert!(harness.query_by_label(&close_label).is_none());
        assert_eq!(harness.state().0.confirm_close, Some(target.clone()));
        assert!(
            !drain_protocol(&mut harness.state_mut().0)
                .iter()
                .any(|command| matches!(command, RuntimeCommand::ClosePane { .. }))
        );

        harness.state_mut().1 = true;
        harness.run();
        harness.key_press(egui::Key::Enter);
        harness.run();
        assert_eq!(
            harness.state().0.confirm_close,
            Some(target.clone()),
            "bare Enter must not close the session"
        );
        harness.get_by_label(&close_label).click();
        harness.run();

        assert!(harness.state().0.confirm_close.is_none());
        assert!(drain_protocol(&mut harness.state_mut().0).iter().any(
            |command| matches!(command, RuntimeCommand::ClosePane { pane } if pane == &target)
        ));
    }

    #[test]
    #[ignore = "offscreen popup PNGs for manual visual review"]
    fn popup_parity_render_session_close() {
        let catalog = i18n::Catalog::load("ko-KR").unwrap();
        let mut workspace = WorkspaceUi::new();
        let mut session_pane = pane("p", SessionId(7));
        session_pane.title = "Claude Code".into();
        workspace.mux = Some(mux(
            "t",
            vec![tab("t", vec![session_pane], LayoutNode::Pane(pane_id("p")))],
            "p",
        ));
        workspace.confirm_close = Some(pane_id("p"));
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(800.0, 650.0))
            .build_ui_state(
                |ui, workspace: &mut WorkspaceUi| {
                    workspace.close_confirm_dialog(ui.ctx(), &catalog);
                },
                workspace,
            );
        crate::fonts::install_cjk_fallback(&harness.ctx, None, "JetBrainsMono", "Regular");
        harness.ctx.set_visuals(egui::Visuals::dark());
        harness.run();
        let output = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/popup-parity");
        std::fs::create_dir_all(&output).unwrap();
        harness
            .render()
            .unwrap()
            .save(output.join("07-session.png"))
            .unwrap();
    }

    #[test]
    fn close_popup_new_session_target_does_not_inherit_close_focus() {
        use egui_kittest::kittest::Queryable;

        let catalog = catalog();
        let close_label = catalog.t("action.close", &[]);
        let mut workspace = WorkspaceUi::new();
        workspace.mux = Some(mux(
            "t",
            vec![tab(
                "t",
                vec![pane("first", SessionId(7)), pane("second", SessionId(8))],
                LayoutNode::Pane(pane_id("first")),
            )],
            "first",
        ));
        workspace.request_close_pane(pane_id("first"));
        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, workspace: &mut WorkspaceUi| {
                workspace.close_confirm_dialog(ui.ctx(), &catalog);
            },
            workspace,
        );
        harness.run();
        harness.get_by_label(&close_label).focus();
        harness.run();
        // Both targets have the same display name. Only the pane identity changes.
        harness.state_mut().request_close_pane(pane_id("second"));
        harness.key_press(egui::Key::Enter);
        harness.run();
        assert_eq!(harness.state().confirm_close, Some(pane_id("second")));
        assert!(
            !drain_protocol(harness.state_mut())
                .iter()
                .any(|command| matches!(command, RuntimeCommand::ClosePane { .. })),
            "the previous target's focused button must not close another session"
        );
        harness.get_by_label(&close_label).click();
        harness.run();
        assert!(drain_protocol(harness.state_mut()).iter().any(
            |command| matches!(command, RuntimeCommand::ClosePane { pane } if *pane == pane_id("second"))
        ));
        assert!(harness.state().confirm_close.is_none());
    }

    #[test]
    fn confirmation_modal_blocks_terminal_keys() {
        let catalog = catalog();
        let config = TerminalConfig::default();
        let mut workspace = WorkspaceUi::new();
        workspace.mux = Some(mux(
            "t",
            vec![tab(
                "t",
                vec![pane("p", SessionId(7))],
                LayoutNode::Pane(pane_id("p")),
            )],
            "p",
        ));
        workspace.sessions.entry(SessionId(7)).or_default().snapshot = Some(snapshot("ready"));
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, workspace: &mut WorkspaceUi| {
                workspace.show_with_input(ui, &config, &[], &catalog, true);
            },
            workspace,
        );
        harness.run_steps(5);
        harness.event(egui::Event::Text("a".into()));
        harness.run_steps(5);
        assert!(
            drain_protocol(harness.state_mut())
                .iter()
                .any(|command| matches!(command, RuntimeCommand::WriteInput { .. })),
            "fixture must accept ordinary input first"
        );
        harness.state_mut().confirm_close = Some(pane_id("p"));
        harness.run_steps(5);
        harness.event(egui::Event::Text("b".into()));
        harness.key_press(egui::Key::Enter);
        harness.run_steps(5);
        assert!(
            !drain_protocol(harness.state_mut())
                .iter()
                .any(|command| matches!(command, RuntimeCommand::WriteInput { .. })),
            "confirmation keys leaked to PTY"
        );
    }

    #[test]
    fn correction_round_input_disabled_missing_mux는_pure_focus_surface다() {
        use egui_kittest::kittest::Queryable;

        let catalog = catalog();
        let prompt_label = catalog.t("workspace.new_shell", &[]);
        let config = TerminalConfig::default();
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, state: &mut (WorkspaceUi, WorkspaceSurfaceOutput)| {
                let frame = state.0.show_with_input(ui, &config, &[], &catalog, false);
                state.1.focus_requested |= frame.focus_requested;
            },
            (WorkspaceUi::new(), WorkspaceSurfaceOutput::default()),
        );

        harness.run();
        assert!(harness.query_by_label(&prompt_label).is_none());
        let surface_point = egui::pos2(80.0, 40.0);
        harness.hover_at(surface_point);
        harness.run();
        harness.drag_at(surface_point);
        harness.run();
        harness.drop_at(surface_point);
        harness.run();

        assert!(harness.state().1.focus_requested);
        assert!(harness.state_mut().0.take_new_session_requested().is_none());
        assert!(drain_protocol(&mut harness.state_mut().0).is_empty());
    }

    #[test]
    fn correction_round_input_disabled_missing_active_tab은_pure_focus_surface다() {
        use egui_kittest::kittest::Queryable;

        let catalog = catalog();
        let prompt_label = catalog.t("workspace.new_shell", &[]);
        let config = TerminalConfig::default();
        let mut workspace = WorkspaceUi::new();
        workspace.mux = Some(Arc::new(MuxSnapshot {
            tabs: vec![tab(
                "other",
                vec![pane("pane", SessionId(7))],
                LayoutNode::Pane(pane_id("pane")),
            )],
            active_tab: Some(tab_id("missing")),
            focused_pane: Some(pane_id("pane")),
        }));
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, state: &mut (WorkspaceUi, WorkspaceSurfaceOutput)| {
                let frame = state.0.show_with_input(ui, &config, &[], &catalog, false);
                state.1.focus_requested |= frame.focus_requested;
            },
            (workspace, WorkspaceSurfaceOutput::default()),
        );

        harness.run();
        assert!(harness.query_by_label(&prompt_label).is_none());
        let surface_point = egui::pos2(80.0, 40.0);
        harness.hover_at(surface_point);
        harness.run();
        harness.drag_at(surface_point);
        harness.run();
        harness.drop_at(surface_point);
        harness.run();

        assert!(harness.state().1.focus_requested);
        assert!(harness.state_mut().0.take_new_session_requested().is_none());
        assert!(drain_protocol(&mut harness.state_mut().0).is_empty());
    }

    #[test]
    fn attached_inactive_tab은_exact_target_viewport만_유지한다() {
        let catalog = catalog();
        let context = egui::Context::default();
        let target_session = SessionId(7);
        let unrelated_session = SessionId(8);
        let target = AttachedPaneTarget {
            workspace_id: "workspace-b".to_owned(),
            tab: tab_id("foreign"),
            pane: pane_id("target"),
            session: target_session,
        };
        let mux_snapshot = mux(
            "active",
            vec![
                tab(
                    "active",
                    vec![pane("active-pane", SessionId(1))],
                    LayoutNode::Pane(pane_id("active-pane")),
                ),
                tab(
                    "foreign",
                    vec![
                        pane("target", target_session),
                        pane("unrelated", unrelated_session),
                    ],
                    LayoutNode::Pane(pane_id("target")),
                ),
            ],
            "active-pane",
        );
        let mut workspace = WorkspaceUi::new();
        workspace
            .sessions
            .entry(unrelated_session)
            .or_default()
            .snapshot = Some(snapshot("stale unrelated"));

        workspace.prepare_frame_with_native_input_for_target(
            &context,
            &[
                RuntimeEvent::MuxUpdated {
                    snapshot: mux_snapshot,
                },
                RuntimeEvent::Viewport {
                    session: target_session,
                    snapshot: snapshot("target viewport"),
                    bracketed_paste: false,
                },
                RuntimeEvent::Viewport {
                    session: unrelated_session,
                    snapshot: snapshot("unrelated viewport"),
                    bracketed_paste: false,
                },
            ],
            &catalog,
            false,
            Some(&target),
            crate::native_key_monitor::NativeKeyDownBatch::default,
        );

        assert!(workspace.sessions[&target_session].snapshot.is_some());
        assert!(
            workspace
                .sessions
                .get(&unrelated_session)
                .is_none_or(|view| view.snapshot.is_none())
        );
    }

    #[test]
    fn attached_prepare_once는_여러_hidden_tab_target의_viewport를_모두_유지한다() {
        let catalog = catalog();
        let context = egui::Context::default();
        let first_session = SessionId(7);
        let second_session = SessionId(8);
        let targets = [
            AttachedPaneTarget {
                workspace_id: "workspace-b".to_owned(),
                tab: tab_id("foreign-one"),
                pane: pane_id("first"),
                session: first_session,
            },
            AttachedPaneTarget {
                workspace_id: "workspace-b".to_owned(),
                tab: tab_id("foreign-two"),
                pane: pane_id("second"),
                session: second_session,
            },
        ];
        let mux_snapshot = mux(
            "active",
            vec![
                tab(
                    "active",
                    vec![pane("active-pane", SessionId(1))],
                    LayoutNode::Pane(pane_id("active-pane")),
                ),
                tab(
                    "foreign-one",
                    vec![pane("first", first_session)],
                    LayoutNode::Pane(pane_id("first")),
                ),
                tab(
                    "foreign-two",
                    vec![pane("second", second_session)],
                    LayoutNode::Pane(pane_id("second")),
                ),
            ],
            "active-pane",
        );
        let mut workspace = WorkspaceUi::new();

        workspace.prepare_attached_panes_with_native_input(
            &context,
            &[
                RuntimeEvent::MuxUpdated {
                    snapshot: mux_snapshot,
                },
                RuntimeEvent::Viewport {
                    session: first_session,
                    snapshot: snapshot("first viewport"),
                    bracketed_paste: false,
                },
                RuntimeEvent::Viewport {
                    session: second_session,
                    snapshot: snapshot("second viewport"),
                    bracketed_paste: false,
                },
            ],
            &catalog,
            &targets,
            None,
            crate::native_key_monitor::NativeKeyDownBatch::default,
        );

        assert!(workspace.sessions[&first_session].snapshot.is_some());
        assert!(workspace.sessions[&second_session].snapshot.is_some());
    }

    #[test]
    fn attached_prepare_once는_native_input을_한번만_drain한다() {
        let catalog = catalog();
        let context = egui::Context::default();
        let target = AttachedPaneTarget {
            workspace_id: "workspace-b".to_owned(),
            tab: tab_id("foreign"),
            pane: pane_id("pane"),
            session: SessionId(7),
        };
        let drains = std::cell::Cell::new(0);
        let mut workspace = WorkspaceUi::new();

        workspace.prepare_attached_panes_with_native_input(
            &context,
            &[],
            &catalog,
            std::slice::from_ref(&target),
            Some(&target),
            || {
                drains.set(drains.get() + 1);
                crate::native_key_monitor::NativeKeyDownBatch::default()
            },
        );

        assert_eq!(drains.get(), 1);
    }

    #[test]
    fn offscreen_attached_input_owner는_native_batch를_한번_drain하고_폐기한다() {
        let catalog = catalog();
        let context = egui::Context::default();
        let visible = AttachedPaneTarget {
            workspace_id: "workspace-b".to_owned(),
            tab: tab_id("visible-tab"),
            pane: pane_id("visible-pane"),
            session: SessionId(7),
        };
        let offscreen = AttachedPaneTarget {
            workspace_id: "workspace-b".to_owned(),
            tab: tab_id("offscreen-tab"),
            pane: pane_id("offscreen-pane"),
            session: SessionId(8),
        };
        let pending = std::rc::Rc::new(std::cell::RefCell::new(Some(
            crate::native_key_monitor::NativeKeyDownBatch {
                printable: vec![crate::native_key_monitor::NativePrintableKeyDown::for_test(
                    '.',
                )],
                clipboard_paste: true,
                clipboard_copy: true,
            },
        )));
        let drains = std::rc::Rc::new(std::cell::Cell::new(0));
        let mut workspace = WorkspaceUi::new();

        let first_pending = std::rc::Rc::clone(&pending);
        let first_drains = std::rc::Rc::clone(&drains);
        workspace.prepare_attached_panes_with_native_input(
            &context,
            &[],
            &catalog,
            std::slice::from_ref(&visible),
            Some(&offscreen),
            move || {
                first_drains.set(first_drains.get() + 1);
                first_pending.borrow_mut().take().unwrap_or_default()
            },
        );

        assert_eq!(drains.get(), 1);
        assert!(pending.borrow().is_none());
        assert!(workspace.prepared_attached_input_owner.is_none());
        assert!(workspace.native_printable_key_downs.is_empty());
        assert!(!workspace.native_clipboard_paste_requested);
        assert!(!workspace.native_clipboard_copy_requested);

        let second_pending = std::rc::Rc::clone(&pending);
        let second_drains = std::rc::Rc::clone(&drains);
        workspace.prepare_attached_panes_with_native_input(
            &context,
            &[],
            &catalog,
            std::slice::from_ref(&offscreen),
            Some(&offscreen),
            move || {
                second_drains.set(second_drains.get() + 1);
                second_pending.borrow_mut().take().unwrap_or_default()
            },
        );

        assert_eq!(drains.get(), 2);
        assert!(workspace.native_printable_key_downs.is_empty());
        assert!(!workspace.native_clipboard_paste_requested);
        assert!(!workspace.native_clipboard_copy_requested);

        workspace.mux = Some(mux(
            "offscreen-tab",
            vec![tab(
                "offscreen-tab",
                vec![pane("offscreen-pane", SessionId(8))],
                LayoutNode::Pane(pane_id("offscreen-pane")),
            )],
            "offscreen-pane",
        ));
        workspace.sessions.entry(SessionId(8)).or_default().snapshot = Some(snapshot("ready"));
        let config = TerminalConfig::default();
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, workspace: &mut WorkspaceUi| {
                workspace.show_prepared_attached_pane(
                    ui,
                    &config,
                    &catalog,
                    &offscreen,
                    "Other",
                    "Project (Workspace)",
                    AttachedPaneAvailability::Available,
                    None,
                );
            },
            workspace,
        );
        harness.run();

        assert!(
            drain_protocol(harness.state_mut())
                .into_iter()
                .all(|command| { !matches!(command, RuntimeCommand::WriteInput { .. }) })
        );
        assert!(harness.state_mut().take_io_intent().is_none());
    }

    #[test]
    fn no_attached_input_owner는_native_monitor를_drain하지_않는다() {
        let catalog = catalog();
        let context = egui::Context::default();
        let target = AttachedPaneTarget {
            workspace_id: "workspace-b".to_owned(),
            tab: tab_id("foreign"),
            pane: pane_id("pane"),
            session: SessionId(7),
        };
        let drains = std::cell::Cell::new(0);
        let mut workspace = WorkspaceUi::new();

        workspace.prepare_attached_panes_with_native_input(
            &context,
            &[],
            &catalog,
            std::slice::from_ref(&target),
            None,
            || {
                drains.set(drains.get() + 1);
                crate::native_key_monitor::NativeKeyDownBatch {
                    printable: vec![crate::native_key_monitor::NativePrintableKeyDown::for_test(
                        '.',
                    )],
                    clipboard_paste: true,
                    clipboard_copy: true,
                }
            },
        );

        assert_eq!(drains.get(), 0);
        assert!(workspace.prepared_attached_input_owner.is_none());
        assert!(workspace.native_printable_key_downs.is_empty());
        assert!(!workspace.native_clipboard_paste_requested);
        assert!(!workspace.native_clipboard_copy_requested);
    }

    #[test]
    fn unavailable_attached_input_owner는_drain된_native_batch와_owner를_폐기한다() {
        let catalog = catalog();
        let context = egui::Context::default();
        let config = TerminalConfig::default();
        let target = AttachedPaneTarget {
            workspace_id: "workspace-b".to_owned(),
            tab: tab_id("foreign"),
            pane: pane_id("pane"),
            session: SessionId(7),
        };
        let mut workspace = WorkspaceUi::new();
        workspace.prepare_attached_panes_with_native_input(
            &context,
            &[],
            &catalog,
            std::slice::from_ref(&target),
            Some(&target),
            || crate::native_key_monitor::NativeKeyDownBatch {
                printable: vec![crate::native_key_monitor::NativePrintableKeyDown::for_test(
                    '.',
                )],
                clipboard_paste: true,
                clipboard_copy: true,
            },
        );
        workspace.mux = Some(mux(
            "foreign",
            vec![tab(
                "foreign",
                vec![pane("pane", SessionId(7))],
                LayoutNode::Pane(pane_id("pane")),
            )],
            "pane",
        ));
        workspace.sessions.entry(SessionId(7)).or_default().snapshot = Some(snapshot("ready"));
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, workspace: &mut WorkspaceUi| {
                workspace.show_prepared_attached_pane(
                    ui,
                    &config,
                    &catalog,
                    &target,
                    "Other",
                    "Project (Workspace)",
                    AttachedPaneAvailability::Unavailable,
                    None,
                );
            },
            workspace,
        );
        harness.run();

        assert!(harness.state().prepared_attached_input_owner.is_none());
        assert!(harness.state().native_printable_key_downs.is_empty());
        assert!(!harness.state().native_clipboard_paste_requested);
        assert!(!harness.state().native_clipboard_copy_requested);
        assert!(harness.state_mut().take_io_intent().is_none());
        assert!(
            drain_protocol(harness.state_mut())
                .into_iter()
                .all(|command| { !matches!(command, RuntimeCommand::WriteInput { .. }) })
        );
    }

    #[test]
    fn snapshot없는_attached_input_owner는_drain된_native_batch와_owner를_폐기한다() {
        let catalog = catalog();
        let context = egui::Context::default();
        let config = TerminalConfig::default();
        let target = AttachedPaneTarget {
            workspace_id: "workspace-b".to_owned(),
            tab: tab_id("foreign"),
            pane: pane_id("pane"),
            session: SessionId(7),
        };
        let mut workspace = WorkspaceUi::new();
        workspace.prepare_attached_panes_with_native_input(
            &context,
            &[],
            &catalog,
            std::slice::from_ref(&target),
            Some(&target),
            || crate::native_key_monitor::NativeKeyDownBatch {
                printable: vec![crate::native_key_monitor::NativePrintableKeyDown::for_test(
                    '.',
                )],
                clipboard_paste: true,
                clipboard_copy: true,
            },
        );
        workspace.mux = Some(mux(
            "foreign",
            vec![tab(
                "foreign",
                vec![pane("pane", SessionId(7))],
                LayoutNode::Pane(pane_id("pane")),
            )],
            "pane",
        ));
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, workspace: &mut WorkspaceUi| {
                workspace.show_prepared_attached_pane(
                    ui,
                    &config,
                    &catalog,
                    &target,
                    "Other",
                    "Project (Workspace)",
                    AttachedPaneAvailability::Available,
                    None,
                );
            },
            workspace,
        );

        harness.run();

        assert!(harness.state().prepared_attached_input_owner.is_none());
        assert!(harness.state().native_printable_key_downs.is_empty());
        assert!(!harness.state().native_clipboard_paste_requested);
        assert!(!harness.state().native_clipboard_copy_requested);
        assert!(harness.state_mut().take_io_intent().is_none());
        assert!(
            drain_protocol(harness.state_mut())
                .into_iter()
                .all(|command| { !matches!(command, RuntimeCommand::WriteInput { .. }) })
        );
    }

    #[test]
    fn prepared_attached_render는_frame_state와_native_input을_reset하지_않는다() {
        let catalog = catalog();
        let config = TerminalConfig::default();
        let target = AttachedPaneTarget {
            workspace_id: "workspace-b".to_owned(),
            tab: tab_id("foreign"),
            pane: pane_id("pane"),
            session: SessionId(7),
        };
        let mut workspace = WorkspaceUi::new();
        workspace.mux = Some(mux(
            "foreign",
            vec![tab(
                "foreign",
                vec![pane("pane", SessionId(7))],
                LayoutNode::Pane(pane_id("pane")),
            )],
            "pane",
        ));
        workspace.sessions.entry(SessionId(7)).or_default().snapshot = Some(snapshot("ready"));
        workspace.frame_counters.rows_painted = 9;
        workspace.native_printable_key_downs =
            vec![crate::native_key_monitor::NativePrintableKeyDown::for_test(
                '.',
            )];
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, workspace: &mut WorkspaceUi| {
                workspace.show_prepared_attached_pane(
                    ui,
                    &config,
                    &catalog,
                    &target,
                    "Other",
                    "Project (Workspace)",
                    AttachedPaneAvailability::Available,
                    None,
                );
            },
            workspace,
        );

        harness.run();

        assert!(harness.state().frame_counters.rows_painted >= 9);
        assert_eq!(harness.state().native_printable_key_downs.len(), 1);
    }

    #[test]
    fn visible_attachment_indices는_viewport와_겹치는_foreign_pane만_반환한다() {
        let viewport = egui::Rect::from_min_size(egui::pos2(100.0, 0.0), egui::vec2(200.0, 80.0));
        let rects = [
            egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(90.0, 80.0)),
            egui::Rect::from_min_size(egui::pos2(100.0, 0.0), egui::vec2(90.0, 80.0)),
            egui::Rect::from_min_size(egui::pos2(210.0, 0.0), egui::vec2(90.0, 80.0)),
            egui::Rect::from_min_size(egui::pos2(300.0, 0.0), egui::vec2(90.0, 80.0)),
            egui::Rect::from_min_size(egui::pos2(310.0, 0.0), egui::vec2(90.0, 80.0)),
        ];

        assert_eq!(
            visible_attachment_indices(viewport, &rects).collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    #[test]
    fn prepared_attached_input_owner는_두_render중_exact_target에만_input을_emit한다() {
        let catalog = catalog();
        let config = TerminalConfig::default();
        let targets = [
            AttachedPaneTarget {
                workspace_id: "workspace-b".to_owned(),
                tab: tab_id("first-tab"),
                pane: pane_id("first-pane"),
                session: SessionId(7),
            },
            AttachedPaneTarget {
                workspace_id: "workspace-b".to_owned(),
                tab: tab_id("second-tab"),
                pane: pane_id("second-pane"),
                session: SessionId(8),
            },
        ];
        let mut workspace = WorkspaceUi::new();
        workspace.mux = Some(mux(
            "first-tab",
            vec![
                tab(
                    "first-tab",
                    vec![pane("first-pane", SessionId(7))],
                    LayoutNode::Pane(pane_id("first-pane")),
                ),
                tab(
                    "second-tab",
                    vec![pane("second-pane", SessionId(8))],
                    LayoutNode::Pane(pane_id("second-pane")),
                ),
            ],
            "first-pane",
        ));
        workspace.sessions.entry(SessionId(7)).or_default().snapshot = Some(snapshot("first"));
        workspace.sessions.entry(SessionId(8)).or_default().snapshot = Some(snapshot("second"));
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, workspace: &mut WorkspaceUi| {
                workspace.prepare_attached_panes(
                    ui.ctx(),
                    &[],
                    &catalog,
                    &targets,
                    Some(&targets[0]),
                );
                let rect = ui.available_rect_before_wrap();
                let middle = rect.center().x;
                for (index, target) in targets.iter().enumerate() {
                    let pane_rect = if index == 0 {
                        egui::Rect::from_min_max(rect.min, egui::pos2(middle, rect.bottom()))
                    } else {
                        egui::Rect::from_min_max(egui::pos2(middle, rect.top()), rect.max)
                    };
                    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(pane_rect));
                    workspace.show_prepared_attached_pane(
                        &mut child,
                        &config,
                        &catalog,
                        target,
                        "Other",
                        "Project (Workspace)",
                        AttachedPaneAvailability::Available,
                        None,
                    );
                }
            },
            workspace,
        );
        harness.run();
        drain_protocol(harness.state_mut());

        harness.event(egui::Event::Text("x".to_owned()));
        harness.run();

        let writes = drain_protocol(harness.state_mut())
            .into_iter()
            .filter_map(|command| match command {
                RuntimeCommand::WriteInput { session, bytes } => Some((session, bytes)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(writes, vec![(SessionId(7), b"x".to_vec())]);
    }

    #[test]
    fn prepared_attached_input_owner와_render_target이_다르면_input을_emit하지_않는다() {
        let catalog = catalog();
        let config = TerminalConfig::default();
        let owner = AttachedPaneTarget {
            workspace_id: "workspace-b".to_owned(),
            tab: tab_id("owner-tab"),
            pane: pane_id("owner-pane"),
            session: SessionId(7),
        };
        let rendered = AttachedPaneTarget {
            workspace_id: "workspace-b".to_owned(),
            tab: tab_id("rendered-tab"),
            pane: pane_id("rendered-pane"),
            session: SessionId(8),
        };
        let targets = [owner, rendered];
        let mut workspace = WorkspaceUi::new();
        workspace.mux = Some(mux(
            "owner-tab",
            vec![
                tab(
                    "owner-tab",
                    vec![pane("owner-pane", SessionId(7))],
                    LayoutNode::Pane(pane_id("owner-pane")),
                ),
                tab(
                    "rendered-tab",
                    vec![pane("rendered-pane", SessionId(8))],
                    LayoutNode::Pane(pane_id("rendered-pane")),
                ),
            ],
            "owner-pane",
        ));
        workspace.sessions.entry(SessionId(8)).or_default().snapshot = Some(snapshot("rendered"));
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, workspace: &mut WorkspaceUi| {
                workspace.prepare_attached_panes(
                    ui.ctx(),
                    &[],
                    &catalog,
                    &targets,
                    Some(&targets[0]),
                );
                workspace.show_prepared_attached_pane(
                    ui,
                    &config,
                    &catalog,
                    &targets[1],
                    "Other",
                    "Project (Workspace)",
                    AttachedPaneAvailability::Available,
                    None,
                );
            },
            workspace,
        );
        harness.run();
        drain_protocol(harness.state_mut());

        harness.event(egui::Event::Text("x".to_owned()));
        harness.run();

        assert!(
            drain_protocol(harness.state_mut())
                .into_iter()
                .all(|command| { !matches!(command, RuntimeCommand::WriteInput { .. }) })
        );
    }

    #[test]
    fn mismatched_attached_input_owner는_다음_render_pass에서_폐기된다() {
        let owner = AttachedPaneTarget {
            workspace_id: "workspace-b".to_owned(),
            tab: tab_id("owner-tab"),
            pane: pane_id("owner-pane"),
            session: SessionId(7),
        };
        let rendered = AttachedPaneTarget {
            workspace_id: "workspace-b".to_owned(),
            tab: tab_id("rendered-tab"),
            pane: pane_id("rendered-pane"),
            session: SessionId(8),
        };
        let mut workspace = WorkspaceUi::new();
        workspace.native_printable_key_downs =
            vec![crate::native_key_monitor::NativePrintableKeyDown::for_test(
                '.',
            )];
        workspace.native_clipboard_paste_requested = true;
        workspace.native_clipboard_copy_requested = true;
        workspace.set_prepared_attached_input_owner(Some(&owner), 41);

        assert!(!workspace.take_prepared_attached_input(&rendered, 41));
        assert_eq!(
            workspace.prepared_attached_input_owner.as_ref(),
            Some(&owner)
        );
        assert_eq!(workspace.native_printable_key_downs.len(), 1);

        assert!(!workspace.take_prepared_attached_input(&owner, 42));
        assert!(workspace.prepared_attached_input_owner.is_none());
        assert!(workspace.native_printable_key_downs.is_empty());
        assert!(!workspace.native_clipboard_paste_requested);
        assert!(!workspace.native_clipboard_copy_requested);
    }

    #[test]
    fn 다음_attached_prepare는_이전_input_owner를_지운다() {
        let catalog = catalog();
        let config = TerminalConfig::default();
        let target = AttachedPaneTarget {
            workspace_id: "workspace-b".to_owned(),
            tab: tab_id("foreign"),
            pane: pane_id("pane"),
            session: SessionId(7),
        };
        let owner_enabled = std::rc::Rc::new(std::cell::Cell::new(true));
        let callback_owner_enabled = std::rc::Rc::clone(&owner_enabled);
        let mut workspace = WorkspaceUi::new();
        workspace.mux = Some(mux(
            "foreign",
            vec![tab(
                "foreign",
                vec![pane("pane", SessionId(7))],
                LayoutNode::Pane(pane_id("pane")),
            )],
            "pane",
        ));
        workspace.sessions.entry(SessionId(7)).or_default().snapshot = Some(snapshot("ready"));
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, workspace: &mut WorkspaceUi| {
                let owner = callback_owner_enabled.get().then_some(&target);
                workspace.prepare_attached_panes(
                    ui.ctx(),
                    &[],
                    &catalog,
                    std::slice::from_ref(&target),
                    owner,
                );
                workspace.show_prepared_attached_pane(
                    ui,
                    &config,
                    &catalog,
                    &target,
                    "Other",
                    "Project (Workspace)",
                    AttachedPaneAvailability::Available,
                    None,
                );
            },
            workspace,
        );
        harness.run();
        drain_protocol(harness.state_mut());
        owner_enabled.set(false);

        harness.event(egui::Event::Text("x".to_owned()));
        harness.run();

        assert!(
            drain_protocol(harness.state_mut())
                .into_iter()
                .all(|command| { !matches!(command, RuntimeCommand::WriteInput { .. }) })
        );
    }

    #[test]
    fn foreign_header_reorder_output은_attachment_id와_bounded_destination만_보유한다() {
        use crate::ui::cross_workspace::{CrossWorkspacePaneState, WorkspacePaneTarget};

        let mut state = CrossWorkspacePaneState::default();
        let id = state
            .attach_right(
                "workspace-a",
                WorkspacePaneTarget::new(
                    "workspace-b",
                    1,
                    tab_id("foreign"),
                    pane_id("pane"),
                    SessionId(7),
                ),
                420.0,
                6,
            )
            .appended_id()
            .expect("attachment id");

        let reorder = AttachedPaneReorder::new(id, usize::MAX);

        assert_eq!(reorder.attachment_id, id);
        assert_eq!(reorder.destination_index, 5);
        assert_eq!(
            std::mem::size_of::<AttachedPaneReorder>(),
            std::mem::size_of_val(&id) + std::mem::size_of::<usize>()
        );
        let source = include_str!("workspace.rs");
        assert!(source.contains("dnd_set_drag_payload(header_context.attachment_id)"));
    }

    #[test]
    fn hidden_frame은_native_input을_drain하고_replay하지_않는다() {
        let catalog = catalog();
        let context = egui::Context::default();
        let drained = std::cell::Cell::new(false);
        let mut workspace = WorkspaceUi::new();

        workspace.update_hidden_with_native_input(&context, &[], &catalog, || {
            drained.set(true);
            crate::native_key_monitor::NativeKeyDownBatch {
                printable: vec![crate::native_key_monitor::NativePrintableKeyDown::for_test(
                    '.',
                )],
                clipboard_paste: true,
                clipboard_copy: true,
            }
        });

        assert!(drained.get());
        assert!(workspace.native_printable_key_downs.is_empty());
        assert!(!workspace.native_clipboard_paste_requested);
        assert!(!workspace.native_clipboard_copy_requested);

        workspace.prepare_frame_with_native_input(&context, &[], &catalog, true, || {
            crate::native_key_monitor::NativeKeyDownBatch::default()
        });
        assert!(workspace.native_printable_key_downs.is_empty());
        assert!(!workspace.native_clipboard_paste_requested);
        assert!(!workspace.native_clipboard_copy_requested);
    }

    #[test]
    fn attached_selection은_terminal_dnd_payload를_시작하지_않는다() {
        assert!(pane_allows_terminal_dnd(PaneRenderMode::Local {
            input_enabled: true,
        }));
        assert!(!pane_allows_terminal_dnd(PaneRenderMode::Attached {
            input_enabled: true,
            workspace_id: "workspace-b",
            workspace_label: "Other",
            session: SessionId(7),
        }));

        let source = include_str!("workspace.rs");
        let drag_payload = source
            .find("dnd_set_drag_payload(TerminalTextDragPayload")
            .expect("terminal drag payload");
        assert!(source[..drag_payload]
            .ends_with("pane_allows_terminal_dnd(mode) && !text.is_empty() {\n                        output\n                            .response\n                            ."));
    }

    #[test]
    fn input_disabled_stale_pending_focus는_소비되지_않는다() {
        let catalog = catalog();
        let config = TerminalConfig::default();
        let session = SessionId(7);
        let target_pane = pane_id("pane");
        let mut workspace = WorkspaceUi::new();
        workspace.mux = Some(mux(
            "primary",
            vec![tab(
                "primary",
                vec![pane("pane", session)],
                LayoutNode::Pane(target_pane.clone()),
            )],
            "pane",
        ));
        workspace.last_focused_pane = Some(target_pane.clone());
        workspace.pending_focus = Some(target_pane.clone());
        workspace.sessions.entry(session).or_default().snapshot = Some(snapshot("ready"));
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, workspace: &mut WorkspaceUi| {
                workspace.show_with_input(ui, &config, &[], &catalog, false);
            },
            workspace,
        );

        harness.run();

        assert_eq!(harness.state().pending_focus, Some(target_pane));
        assert!(
            drain_protocol(harness.state_mut())
                .iter()
                .all(|command| matches!(command, RuntimeCommand::ResizeTracked { .. }))
        );
    }

    #[test]
    fn attached_without_snapshot은_workspace_specific_unavailable을_표시한다() {
        use egui_kittest::kittest::Queryable;

        let catalog = catalog();
        let unavailable = catalog.t(
            "workspace.cross_pane.input_unavailable",
            &[("workspace", "Other")],
        );
        let connecting = catalog.t("workspace.connecting", &[]);
        let config = TerminalConfig::default();
        let target = AttachedPaneTarget {
            workspace_id: "workspace-b".to_owned(),
            tab: tab_id("foreign"),
            pane: pane_id("pane"),
            session: SessionId(7),
        };
        let mut workspace = WorkspaceUi::new();
        workspace.mux = Some(mux(
            "foreign",
            vec![tab(
                "foreign",
                vec![pane("pane", SessionId(7))],
                LayoutNode::Pane(pane_id("pane")),
            )],
            "pane",
        ));
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, workspace: &mut WorkspaceUi| {
                workspace.show_attached_pane(
                    ui,
                    &config,
                    &[],
                    &catalog,
                    &target,
                    "Other",
                    AttachedPaneAvailability::Available,
                    false,
                );
            },
            workspace,
        );

        harness.run();

        assert!(harness.query_by_label(&unavailable).is_some());
        assert!(harness.query_by_label(&connecting).is_none());
        assert!(harness.state().staged_terminal_resizes.is_empty());
        assert!(drain_protocol(harness.state_mut()).is_empty());

        // 이미 크기를 전송한 세션이 viewport만 기다리는 경우에는 그 폭을 유지해
        // 행 크기를 조정한다. 연결 안내가 보여도 기존 크기 조정 경로는 살아 있다.
        harness
            .state_mut()
            .sent_sizes
            .insert(SessionId(7), (80, 24));
        harness.run();
        let ctx = harness.ctx.clone();
        let candidate = harness
            .state()
            .staged_terminal_resizes
            .get(&SessionId(7))
            .copied()
            .expect("attached pane resize candidate");
        harness
            .state_mut()
            .flush_render_side_effects_for_pass(&ctx, candidate.pass, false);
        assert_eq!(
            harness.state().pending_resize_target[&SessionId(7)].0,
            candidate.cols,
            "viewport가 없어도 현재 pane 폭으로 행 크기 변경을 예약해야 한다"
        );
        assert!(
            drain_protocol(harness.state_mut()).is_empty(),
            "debounce가 끝나기 전에 리사이즈를 보내면 안 된다"
        );

        harness
            .state_mut()
            .pending_resize_target
            .get_mut(&SessionId(7))
            .expect("pending attached resize")
            .3 = std::time::Instant::now()
            .checked_sub(RESIZE_DRAG_DEBOUNCE + std::time::Duration::from_millis(1))
            .unwrap();
        harness.run();
        let pass = harness
            .state()
            .staged_terminal_resizes
            .get(&SessionId(7))
            .expect("stable attached pane resize candidate")
            .pass;
        harness
            .state_mut()
            .flush_render_side_effects_for_pass(&ctx, pass, false);
        assert!(
            drain_protocol(harness.state_mut())
                .iter()
                .any(|command| matches!(
                    command,
                    RuntimeCommand::ResizeTracked {
                        session: SessionId(7),
                    cols,
                    ..
                } if *cols == candidate.cols
                )),
            "debounce가 끝나면 현재 pane 폭으로 tracked resize를 보내야 한다"
        );
    }

    #[test]
    fn input_disabled_attached_surface는_text_event를_emit하지_않는다() {
        let catalog = catalog();
        let config = TerminalConfig::default();
        let session = SessionId(7);
        let mut workspace = WorkspaceUi::new();
        workspace.mux = Some(mux(
            "foreign",
            vec![tab(
                "foreign",
                vec![pane("pane", session)],
                LayoutNode::Pane(pane_id("pane")),
            )],
            "pane",
        ));
        workspace.last_focused_pane = Some(pane_id("pane"));
        let view = workspace.sessions.entry(session).or_default();
        view.snapshot = Some(snapshot("ready"));
        view.snapshot_gen = 1;
        let target = AttachedPaneTarget {
            workspace_id: "workspace-b".to_owned(),
            tab: tab_id("foreign"),
            pane: pane_id("pane"),
            session,
        };
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, workspace: &mut WorkspaceUi| {
                workspace.show_attached_pane(
                    ui,
                    &config,
                    &[],
                    &catalog,
                    &target,
                    "Other",
                    AttachedPaneAvailability::Available,
                    false,
                );
            },
            workspace,
        );
        harness.run();
        harness.event(egui::Event::Text("x".to_owned()));
        harness.run();

        assert!(
            !drain_protocol(harness.state_mut())
                .iter()
                .any(|command| matches!(command, RuntimeCommand::WriteInput { .. }))
        );
    }

    #[test]
    fn attached_detach는_close나_kill없이_output만_반환한다() {
        use egui_kittest::kittest::Queryable;

        let catalog = catalog();
        let target = AttachedPaneTarget {
            workspace_id: "workspace-b".to_owned(),
            tab: tab_id("foreign"),
            pane: pane_id("pane"),
            session: SessionId(7),
        };
        let mut workspace = WorkspaceUi::new();
        workspace.mux = Some(mux(
            "foreign",
            vec![tab(
                "foreign",
                vec![pane("pane", SessionId(7))],
                LayoutNode::Pane(pane_id("pane")),
            )],
            "pane",
        ));
        let config = TerminalConfig::default();
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, state: &mut (WorkspaceUi, AttachedPaneOutput)| {
                let frame = state.0.show_attached_pane(
                    ui,
                    &config,
                    &[],
                    &catalog,
                    &target,
                    "Other",
                    AttachedPaneAvailability::Available,
                    false,
                );
                state.1.focus_requested |= frame.focus_requested;
                state.1.detach_requested |= frame.detach_requested;
                state.1.target_present |= frame.target_present;
            },
            (workspace, AttachedPaneOutput::default()),
        );
        harness.run();
        harness.get_by_label("×").click();
        harness.run();

        assert!(harness.state().1.detach_requested);
        assert!(
            !drain_protocol(&mut harness.state_mut().0)
                .iter()
                .any(|command| matches!(command, RuntimeCommand::ClosePane { .. }))
        );
    }

    #[test]
    fn attached_absolute_ids는_workspace와_session으로_namespace된다() {
        let pane = pane_id("same");
        let a = PaneRenderMode::Attached {
            input_enabled: false,
            workspace_id: "workspace-a",
            workspace_label: "A",
            session: SessionId(7),
        };
        let b = PaneRenderMode::Attached {
            input_enabled: false,
            workspace_id: "workspace-b",
            workspace_label: "B",
            session: SessionId(7),
        };
        let reused_session = PaneRenderMode::Attached {
            input_enabled: false,
            workspace_id: "workspace-b",
            workspace_label: "B",
            session: SessionId(9),
        };

        assert_ne!(
            pane_interaction_id(a, "pane_bg", &pane),
            pane_interaction_id(b, "pane_bg", &pane)
        );
        assert_ne!(
            pane_interaction_id(b, "pane_bg", &pane),
            pane_interaction_id(reused_session, "pane_bg", &pane)
        );
    }

    #[test]
    fn input_disabled_attached_terminal_click은_pure_focus만_요청한다() {
        let catalog = catalog();
        let config = TerminalConfig::default();
        let session = SessionId(7);
        let mut workspace = WorkspaceUi::new();
        workspace.mux = Some(mux(
            "foreign",
            vec![tab(
                "foreign",
                vec![pane("pane", session)],
                LayoutNode::Pane(pane_id("pane")),
            )],
            "pane",
        ));
        let view = workspace.sessions.entry(session).or_default();
        view.snapshot = Some(snapshot("ready"));
        view.snapshot_gen = 1;
        let target = AttachedPaneTarget {
            workspace_id: "workspace-b".to_owned(),
            tab: tab_id("foreign"),
            pane: pane_id("pane"),
            session,
        };
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, state: &mut (WorkspaceUi, AttachedPaneOutput, bool)| {
                let frame = state.0.show_attached_pane(
                    ui,
                    &config,
                    &[],
                    &catalog,
                    &target,
                    "Other",
                    AttachedPaneAvailability::Available,
                    false,
                );
                state.1.focus_requested |= frame.focus_requested;
                state.1.detach_requested |= frame.detach_requested;
                state.1.target_present |= frame.target_present;
                state.2 |= state.0.take_terminal_focus_claimed();
            },
            (workspace, AttachedPaneOutput::default(), false),
        );
        harness.run();
        let terminal_point = egui::pos2(80.0, TERMINAL_PANE_HEADER_HEIGHT + 40.0);
        harness.input_mut().events.extend([
            egui::Event::PointerMoved(terminal_point),
            egui::Event::PointerButton {
                pos: terminal_point,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
            egui::Event::PointerButton {
                pos: terminal_point,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            },
        ]);
        harness.run();

        assert!(harness.state().1.focus_requested);
        assert!(harness.state().2);
        let commands = drain_protocol(&mut harness.state_mut().0);
        assert!(
            commands
                .iter()
                .all(|command| matches!(command, RuntimeCommand::ResizeTracked { .. }))
        );
    }

    #[test]
    fn input_disabled_primary_terminal_click은_primary_focus만_요청한다() {
        let catalog = catalog();
        let config = TerminalConfig::default();
        let session = SessionId(7);
        let mut workspace = WorkspaceUi::new();
        workspace.mux = Some(mux(
            "primary",
            vec![tab(
                "primary",
                vec![pane("pane", session)],
                LayoutNode::Pane(pane_id("pane")),
            )],
            "pane",
        ));
        workspace.last_focused_pane = Some(pane_id("pane"));
        let view = workspace.sessions.entry(session).or_default();
        view.snapshot = Some(snapshot("ready"));
        view.snapshot_gen = 1;
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, state: &mut (WorkspaceUi, WorkspaceSurfaceOutput, bool)| {
                let frame = state.0.show_with_input(ui, &config, &[], &catalog, false);
                state.1.focus_requested |= frame.focus_requested;
                if frame.local_focus_claimed.is_some() {
                    state.1.local_focus_claimed = frame.local_focus_claimed;
                }
                state.2 |= state.0.take_terminal_focus_claimed();
            },
            (workspace, WorkspaceSurfaceOutput::default(), false),
        );
        harness.run();
        let terminal_point = egui::pos2(80.0, TERMINAL_PANE_HEADER_HEIGHT + 40.0);
        harness.input_mut().events.extend([
            egui::Event::PointerMoved(terminal_point),
            egui::Event::PointerButton {
                pos: terminal_point,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
            egui::Event::PointerButton {
                pos: terminal_point,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            },
        ]);
        harness.run();

        assert!(harness.state().1.focus_requested);
        assert_eq!(harness.state().1.local_focus_claimed, Some(pane_id("pane")));
        assert!(harness.state().2);
        let commands = drain_protocol(&mut harness.state_mut().0);
        assert!(
            commands
                .iter()
                .any(|command| matches!(command, RuntimeCommand::FocusPane { pane } if pane == &pane_id("pane")))
        );
    }

    #[test]
    fn 기존_close_now는_여전히_runtime_close_pane을_보낸다() {
        let target = pane_id("pane");
        let mut workspace = WorkspaceUi::new();

        workspace.close_pane_now(target.clone());

        assert!(drain_protocol(&mut workspace).iter().any(
            |command| matches!(command, RuntimeCommand::ClosePane { pane } if pane == &target)
        ));
    }

    #[test]
    fn kittest_빈_workspace의_새세션은_agent_launcher만_요청한다() {
        use egui_kittest::kittest::Queryable;

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let button_label = catalog.t("workspace.new_shell", &[]);
        let config = TerminalConfig::default();
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, workspace: &mut WorkspaceUi| {
                workspace.show_with_input(ui, &config, &[], &catalog, true);
            },
            WorkspaceUi::new(),
        );
        harness.run();
        harness.get_by_label(&button_label).click();
        harness.run();

        assert!(harness.state_mut().take_new_session_requested().is_some());
        assert!(drain_protocol(harness.state_mut()).is_empty());
    }

    fn painted_tab_text(output: &egui::FullOutput, label: &str) -> (egui::Color32, egui::Rect) {
        output
            .shapes
            .iter()
            .find_map(|shape| {
                if let egui::Shape::Text(text) = &shape.shape
                    && text.galley.text() == label
                {
                    Some((
                        text.galley.job.sections[0].format.color,
                        egui::Rect::from_min_size(text.pos, text.galley.size()),
                    ))
                } else {
                    None
                }
            })
            .expect("tab title must be painted")
    }

    #[test]
    fn tab_title_hover_matches_line_without_affecting_other_tabs_or_input() {
        for focused in [false, true] {
            for aux_active in [false, true] {
                let catalog = catalog();
                let config = TerminalConfig::default();
                let snapshot = pane("p", SessionId(7));
                let mut workspace = WorkspaceUi::new();
                workspace.workspace_accent = egui::Color32::from_rgb(0xb9, 0x8a, 0x53);
                workspace.set_aux_tabs(vec![PaneAuxTab {
                    kind: PaneAuxTabKind::History,
                    label: "History".to_owned(),
                    active: aux_active,
                }]);
                workspace.aux_tab_pane = Some(pane_id("p"));
                let mut harness = egui_kittest::Harness::builder()
                    .with_size(egui::vec2(800.0, 400.0))
                    .build_ui_state(
                        move |ui, workspace: &mut WorkspaceUi| {
                            workspace.render_pane_header(
                                ui,
                                egui::Rect::from_min_size(
                                    egui::pos2(8.0, 8.0),
                                    egui::vec2(760.0, TERMINAL_PANE_HEADER_HEIGHT),
                                ),
                                &snapshot,
                                focused,
                                &config,
                                &catalog,
                                true,
                            );
                        },
                        workspace,
                    );
                harness.run();
                let (normal_session, session_rect) = painted_tab_text(harness.output(), "p");
                let (normal_aux, aux_rect) = painted_tab_text(harness.output(), "History");
                let line_color = harness
                    .output()
                    .shapes
                    .iter()
                    .find_map(|shape| {
                        if let egui::Shape::LineSegment { points, stroke } = &shape.shape
                            && points[0].x == points[1].x
                            && points[0].y < points[1].y
                        {
                            Some(stroke.color)
                        } else {
                            None
                        }
                    })
                    .expect("session tab divider must be painted");
                assert_ne!(normal_session, line_color);
                assert_ne!(normal_aux, line_color);

                // Hover either label or its nested close button; only this tab changes.
                for point in [
                    session_rect.center(),
                    egui::pos2(session_rect.right() + 14.0, 20.0),
                ] {
                    harness.hover_at(point);
                    harness.run();
                    assert_eq!(
                        painted_tab_text(harness.output(), "p").0,
                        line_color,
                        "session hover must match its line; focused={focused}, aux_active={aux_active}"
                    );
                    assert_eq!(painted_tab_text(harness.output(), "History").0, normal_aux);
                }
                for point in [
                    aux_rect.center(),
                    egui::pos2(aux_rect.right() + PANE_AUX_TAB_CLOSE_GAP, 20.0),
                ] {
                    harness.hover_at(point);
                    harness.run();
                    assert_eq!(painted_tab_text(harness.output(), "History").0, line_color);
                    assert_eq!(painted_tab_text(harness.output(), "p").0, normal_session);
                }
                harness.hover_at(egui::pos2(400.0, 80.0));
                harness.run();
                assert_eq!(painted_tab_text(harness.output(), "p").0, normal_session);
                assert_eq!(painted_tab_text(harness.output(), "History").0, normal_aux);
                assert!(harness.state_mut().take_new_session_requested().is_none());
                assert!(
                    drain_protocol(harness.state_mut()).is_empty(),
                    "hover must not send runtime input"
                );
            }
        }
    }

    #[test]
    fn tab_title_hover_session_less_strip_matches_its_line_and_restores_color() {
        let catalog = catalog();
        let empty = catalog.t("workspace.tab.no_session", &[]);
        let mut workspace = WorkspaceUi::new();
        workspace.workspace_accent = egui::Color32::from_rgb(0xb9, 0x8a, 0x53);
        workspace.set_aux_tabs(vec![PaneAuxTab {
            kind: PaneAuxTabKind::History,
            label: "History".to_owned(),
            active: true,
        }]);
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, workspace: &mut WorkspaceUi| {
                workspace.show_session_less_aux_tabs(ui, &catalog, true);
            },
            workspace,
        );
        harness.run();
        let line_color = harness
            .output()
            .shapes
            .iter()
            .find_map(|shape| {
                if let egui::Shape::LineSegment { points, stroke } = &shape.shape
                    && points[1].x > points[0].x
                    && points[0].y == points[1].y
                {
                    Some(stroke.color)
                } else {
                    None
                }
            })
            .expect("selected auxiliary tab line must be painted");
        for label in [&empty, "History"] {
            let (normal, rect) = painted_tab_text(harness.output(), label);
            harness.hover_at(rect.center());
            harness.run();
            assert_eq!(painted_tab_text(harness.output(), label).0, line_color);
            harness.hover_at(egui::pos2(400.0, 80.0));
            harness.run();
            assert_eq!(painted_tab_text(harness.output(), label).0, normal);
        }
        assert!(harness.state_mut().take_new_session_requested().is_none());
        assert!(drain_protocol(harness.state_mut()).is_empty());
    }

    #[test]
    fn tab_title_hover_attached_header_matches_source_line_without_detaching() {
        let catalog = catalog();
        let target = AttachedPaneTarget {
            workspace_id: "other".to_owned(),
            tab: tab_id("t"),
            pane: pane_id("p"),
            session: SessionId(7),
        };
        let mut attachments = crate::ui::cross_workspace::CrossWorkspacePaneState::default();
        let id = attachments
            .attach_right(
                "primary",
                crate::ui::cross_workspace::WorkspacePaneTarget::new(
                    "other",
                    1,
                    tab_id("t"),
                    pane_id("p"),
                    SessionId(7),
                ),
                420.0,
                1,
            )
            .appended_id()
            .unwrap();
        let line_color = egui::Color32::from_rgb(0xa9, 0x82, 0x54);
        let header_context = AttachedPaneHeaderContext::new(id, 0).with_identity_color(line_color);
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, state: &mut (WorkspaceUi, bool)| {
                let (detach, reorder) = state.0.render_attached_pane_header(
                    ui,
                    egui::Rect::from_min_size(
                        egui::pos2(8.0, 8.0),
                        egui::vec2(360.0, TERMINAL_PANE_HEADER_HEIGHT),
                    ),
                    &target,
                    "Other",
                    "Project (Other)",
                    &catalog,
                    Some(header_context),
                );
                state.1 |= detach || reorder.is_some();
            },
            (WorkspaceUi::new(), false),
        );
        harness.run();
        let (normal, rect) = painted_tab_text(harness.output(), "Project (Other)");
        for point in [rect.center(), egui::pos2(353.0, 20.0)] {
            harness.hover_at(point);
            harness.run();
            assert_eq!(
                painted_tab_text(harness.output(), "Project (Other)").0,
                line_color
            );
        }
        harness.hover_at(egui::pos2(400.0, 80.0));
        harness.run();
        assert_eq!(
            painted_tab_text(harness.output(), "Project (Other)").0,
            normal
        );
        assert!(!harness.state().1, "hover must not detach or reorder");
        assert!(drain_protocol(&mut harness.state_mut().0).is_empty());
    }

    #[test]
    fn tab_strip_blank_click_requests_launcher_without_spawning_shell() {
        let catalog = catalog();
        let config = TerminalConfig::default();
        let mut workspace = WorkspaceUi::new();
        workspace.mux = Some(mux(
            "t",
            vec![tab(
                "t",
                vec![pane("p", SessionId(7))],
                LayoutNode::Pane(pane_id("p")),
            )],
            "p",
        ));
        let pane = pane("p", SessionId(7));
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(800.0, 400.0))
            .build_ui_state(
                move |ui, workspace: &mut WorkspaceUi| {
                    workspace.render_pane_header(
                        ui,
                        egui::Rect::from_min_size(ui.min_rect().min, egui::vec2(760.0, 32.0)),
                        &pane,
                        true,
                        &config,
                        &catalog,
                        true,
                    );
                },
                workspace,
            );
        harness.run();
        harness.hover_at(egui::pos2(300.0, 24.0));
        harness.run();
        harness.drag_at(egui::pos2(300.0, 24.0));
        harness.run();
        harness.drop_at(egui::pos2(300.0, 24.0));
        harness.run();
        assert!(
            harness.state_mut().take_new_session_requested()
                == Some(NewSessionRequest::SplitRight(pane_id("p"))),
            "blank strip must open launcher for the clicked pane"
        );
        assert!(
            !drain_protocol(harness.state_mut())
                .iter()
                .any(|command| matches!(
                    command,
                    RuntimeCommand::SpawnShell { .. } | RuntimeCommand::SpawnAgent { .. }
                ))
        );
    }

    #[test]
    fn tab_strip_visible_blank_is_clickable_across_its_full_height_and_width() {
        let catalog = catalog();
        let config = TerminalConfig::default();
        let snapshot = pane("p", SessionId(7));
        let header = egui::Rect::from_min_size(
            egui::pos2(8.0, 8.0),
            egui::vec2(760.0, TERMINAL_PANE_HEADER_HEIGHT),
        );
        let mut workspace = WorkspaceUi::new();
        workspace.mux = Some(mux(
            "t",
            vec![tab(
                "t",
                vec![pane("p", SessionId(7))],
                LayoutNode::Pane(pane_id("p")),
            )],
            "p",
        ));
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(800.0, 400.0))
            .build_ui_state(
                move |ui, workspace: &mut WorkspaceUi| {
                    workspace
                        .render_pane_header(ui, header, &snapshot, true, &config, &catalog, true);
                },
                workspace,
            );
        harness.run();
        // Includes the top/bottom of the blank strip and the empty right margin
        // beyond the toolbar. Actual title/X/tool icons remain separate targets.
        for x in [100.0, 300.0, 670.0, header.right() - 1.0] {
            for y in [header.top() + 1.0, header.center().y, header.bottom() - 1.0] {
                let point = egui::pos2(x, y);
                harness.hover_at(point);
                harness.run();
                harness.drag_at(point);
                harness.run();
                harness.drop_at(point);
                harness.run();
                assert!(
                    harness.state_mut().take_new_session_requested()
                        == Some(NewSessionRequest::SplitRight(pane_id("p"))),
                    "blank click missed at {point:?}"
                );
                assert!(
                    drain_protocol(harness.state_mut())
                        .iter()
                        .all(|command| matches!(command, RuntimeCommand::FocusPane { .. })),
                    "blank must not close, split or spawn before launcher selection"
                );
            }
        }
    }

    #[test]
    fn tab_strip_aux_body_blank_opens_launcher_with_terminal_input_disabled() {
        for blocked_by_modal in [false, true] {
            let catalog = catalog();
            let config = TerminalConfig::default();
            let snapshot = pane("p", SessionId(7));
            let mut workspace = WorkspaceUi::new();
            workspace.mux = Some(mux(
                "t",
                vec![tab(
                    "t",
                    vec![pane("p", SessionId(7))],
                    LayoutNode::Pane(pane_id("p")),
                )],
                "p",
            ));
            workspace.set_aux_tabs(vec![PaneAuxTab {
                kind: PaneAuxTabKind::History,
                label: "History".to_owned(),
                active: true,
            }]);
            workspace.aux_tab_pane = Some(pane_id("p"));
            let mut harness = egui_kittest::Harness::builder()
                .with_size(egui::vec2(800.0, 400.0))
                .build_ui_state(
                    move |ui, workspace: &mut WorkspaceUi| {
                        super::super::popup::set_pending_modal(ui.ctx(), blocked_by_modal);
                        workspace.render_pane_header(
                            ui,
                            egui::Rect::from_min_size(
                                egui::pos2(8.0, 8.0),
                                egui::vec2(760.0, TERMINAL_PANE_HEADER_HEIGHT),
                            ),
                            &snapshot,
                            true,
                            &config,
                            &catalog,
                            false,
                        );
                    },
                    workspace,
                );
            harness.run();
            let point = egui::pos2(300.0, 20.0);
            harness.hover_at(point);
            harness.run();
            harness.drag_at(point);
            harness.run();
            harness.drop_at(point);
            harness.run();
            assert_eq!(
                harness.state_mut().take_new_session_requested(),
                (!blocked_by_modal).then(|| NewSessionRequest::SplitRight(pane_id("p"))),
                "auxiliary body blocks PTY input, but the launcher must remain available unless a modal blocks it"
            );
            assert!(
                drain_protocol(harness.state_mut())
                    .iter()
                    .all(|command| matches!(command, RuntimeCommand::FocusPane { .. })),
                "opening the selector must not spawn or send PTY input"
            );
        }
    }

    #[test]
    fn tab_strip_inactive_surface_requests_focus_without_launcher() {
        let catalog = catalog();
        let config = TerminalConfig::default();
        let pane = pane("p", SessionId(7));
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, state: &mut (WorkspaceUi, bool)| {
                let output = state.0.render_pane_header(
                    ui,
                    egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(760.0, 32.0)),
                    &pane,
                    false,
                    &config,
                    &catalog,
                    false,
                );
                state.1 |= output.focus_requested;
            },
            (WorkspaceUi::new(), false),
        );
        harness.run();
        let pos = egui::pos2(300.0, 16.0);
        harness.hover_at(pos);
        harness.run();
        harness.drag_at(pos);
        harness.run();
        harness.drop_at(pos);
        harness.run();
        assert!(harness.state().1);
        assert!(harness.state_mut().0.take_new_session_requested().is_none());
        assert!(
            !drain_protocol(&mut harness.state_mut().0)
                .iter()
                .any(|command| matches!(
                    command,
                    RuntimeCommand::SpawnShell { .. } | RuntimeCommand::SpawnAgent { .. }
                ))
        );
    }

    #[test]
    fn tab_strip_divider_is_painted_without_auxiliary_tabs() {
        for ppp in [1.0, 1.25, 1.5, 2.0, 3.0] {
            let ctx = egui::Context::default();
            ctx.set_pixels_per_point(ppp);
            let catalog = catalog();
            let config = TerminalConfig::default();
            let pane = pane("p", SessionId(7));
            let mut workspace = WorkspaceUi::new();
            workspace.workspace_accent = egui::Color32::from_rgb(0x9b, 0x77, 0x60);
            let header = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(760.0, 32.0));
            let mut expected_x = 0.0;
            let output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(800.0, 400.0),
                    )),
                    ..Default::default()
                },
                |ui| {
                    let width = ui
                        .painter()
                        .layout_no_wrap(
                            "p".into(),
                            egui::FontId::proportional(13.0),
                            egui::Color32::WHITE,
                        )
                        .size()
                        .x;
                    let close = pane_header_buttons(header, width, 4, 0.0).close;
                    expected_x = crate::ui::snap_line_to_pixel(
                        pane_header_active_boundary(header, close),
                        crate::ui::designall::SEPARATOR_WIDTH,
                        ui.ctx().pixels_per_point(),
                    );
                    workspace.render_pane_header(ui, header, &pane, true, &config, &catalog, true);
                },
            );
            assert!(
                output.shapes.iter().any(|shape| matches!(&shape.shape,
            egui::Shape::LineSegment { points, .. } if
                (points[0].x - expected_x).abs() < 0.1 && (points[1].x - expected_x).abs() < 0.1
                && points[1].y > points[0].y)),
                "missing vertical divider after X"
            );
            let vertical = output
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::Shape::LineSegment { points, stroke }
                        if (points[0].x - expected_x).abs() < 0.1
                            && (points[1].x - expected_x).abs() < 0.1
                            && points[1].y > points[0].y =>
                    {
                        Some((*points, *stroke))
                    }
                    _ => None,
                })
                .unwrap();
            let top = output
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::Shape::LineSegment { points, stroke }
                        if points[1].x > points[0].x && (points[1].y - points[0].y).abs() < 0.1 =>
                    {
                        Some((*points, *stroke))
                    }
                    _ => None,
                })
                .unwrap();
            output.drop_without_applying_deltas();
            assert_eq!(
                vertical.1, top.1,
                "X divider must use the top border stroke"
            );
            assert_eq!(
                vertical.0[0], top.0[1],
                "top and X divider must share their endpoint"
            );
            assert_eq!(vertical.0[1].y, header.bottom());
        }
    }

    #[test]
    fn tab_strip_session_less_blank_click_requests_launcher() {
        let catalog = catalog();
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, workspace: &mut WorkspaceUi| {
                workspace.show_session_less_aux_tabs(ui, &catalog, true);
            },
            WorkspaceUi::new(),
        );
        harness.run();
        let pos = egui::pos2(300.0, 16.0);
        harness.hover_at(pos);
        harness.run();
        harness.drag_at(pos);
        harness.run();
        harness.drop_at(pos);
        harness.run();
        assert!(harness.state_mut().take_new_session_requested().is_some());
        assert!(drain_protocol(harness.state_mut()).is_empty());
    }

    #[test]
    fn 상단_터미널_툴바는_검색_새셸_좌우_상하분할을_정확히_호출한다() {
        let mut ui = WorkspaceUi::new();
        ui.mux = Some(mux(
            "t",
            vec![tab(
                "t",
                vec![pane("p", SessionId(7))],
                LayoutNode::Pane(pane_id("p")),
            )],
            "p",
        ));
        let config = TerminalConfig::default();
        let target = pane_id("p");

        ui.activate_terminal_toolbar(TerminalToolbarIcon::Search, &target, &config);
        assert_eq!(
            ui.search.as_ref().map(|search| search.session),
            Some(SessionId(7))
        );
        ui.activate_terminal_toolbar(TerminalToolbarIcon::NewTerminal, &target, &config);
        assert_eq!(
            ui.take_new_session_requested(),
            Some(NewSessionRequest::NewTab)
        );
        ui.activate_terminal_toolbar(TerminalToolbarIcon::SplitColumns, &target, &config);
        ui.activate_terminal_toolbar(TerminalToolbarIcon::SplitRows, &target, &config);

        let commands = drain_protocol(&mut ui);
        assert!(matches!(
            &commands[0],
            RuntimeCommand::SplitPane {
                pane,
                direction: SplitDirection::Horizontal,
                ..
            } if pane == &target
        ));
        assert!(matches!(
            &commands[1],
            RuntimeCommand::SplitPane {
                pane,
                direction: SplitDirection::Vertical,
                ..
            } if pane == &target
        ));
    }

    /// 세션 헤더 Search 버튼의 갈래 조건(2026-08-18 스펙) — Search 아이콘이면서
    /// 보조 본문이 활성일 때만 보조 검색으로 간다. 다른 도구는 보조 본문이 활성이어도
    /// 항상 기존 경로(`activate_terminal_toolbar`)로 간다.
    #[test]
    fn search_click_targets_aux_search는_search_아이콘이면서_보조본문_활성일_때만_참이다() {
        assert!(search_click_targets_aux_search(
            TerminalToolbarIcon::Search,
            true
        ));
        assert!(!search_click_targets_aux_search(
            TerminalToolbarIcon::Search,
            false
        ));
        assert!(!search_click_targets_aux_search(
            TerminalToolbarIcon::NewTerminal,
            true
        ));
        assert!(!search_click_targets_aux_search(
            TerminalToolbarIcon::SplitColumns,
            true
        ));
        assert!(!search_click_targets_aux_search(
            TerminalToolbarIcon::SplitRows,
            true
        ));
    }

    /// ④ 헤더가 극단적으로 좁아 `layout_aux_tabs`가 활성 문서 탭까지 접어(빈 배치)
    /// 돌려줘도, `any_aux_tab_active`는 App이 넘긴 원본 목록만 보고 true를 돌려줘야
    /// 한다 — `render_pane_header`가 이 값으로 세션 제목 밝기를 정하기 때문에,
    /// placements가 아니라 이 값을 쓰지 않으면 세션이 선택된 것처럼 잘못 칠해진다.
    #[test]
    fn any_aux_tab_active는_레이아웃이_아니라_실제_활성_여부를_따른다() {
        let header = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(900.0, 24.0));
        let close = egui::Rect::from_center_size(egui::pos2(120.0, 12.0), egui::vec2(20.0, 20.0));
        let tabs = [aux_tab(
            PaneAuxTabKind::Document(DocumentTabId(1)),
            "note.md",
            true,
        )];
        // 세_탭의_축약_순서를_좌표로_고정한다의 ⓒ 단계와 같은 폭(150.0) — 활성 문서마저
        // 최소 라벨 폭을 못 채워 배치가 빈다.
        let placements = layout_aux_tabs(header, close, 150.0, &tabs, |_| 30.0);
        assert!(
            placements.is_empty(),
            "전제: 이 폭에서 배치는 비어야 한다 {placements:?}"
        );
        assert!(
            any_aux_tab_active(&tabs),
            "배치가 비어도 실제로는 문서가 활성이다"
        );
    }

    /// 헤더가 실제로 배치한 것과 같은 기하로 Search 버튼(도구 4개 중 첫 번째) 중심을
    /// 다시 계산한다. `aux_tab_geometry_for_test`와 같은 관례 — 테스트 헤더(520pt)는
    /// 항상 도구 4개가 다 보여야 한다(좁아지면 왼쪽 도구부터 숨는데, Search가 바로
    /// 그 왼쪽 끝이라 좁은 헤더에서는 이 헬퍼를 쓰면 안 된다).
    fn search_toolbar_button_center(
        ws: &WorkspaceUi,
        header: egui::Rect,
        snapshot: &runtime::PaneSnapshot,
    ) -> egui::Pos2 {
        let context = egui::Context::default();
        let mut center = None;
        context
            .run_ui(egui::RawInput::default(), |ui| {
                let font = egui::FontId::proportional(13.0);
                let aux_reserved: f32 = ws
                    .aux_tabs
                    .iter()
                    .map(|tab| {
                        let natural = ui
                            .painter()
                            .layout_no_wrap(tab.label.clone(), font.clone(), egui::Color32::WHITE)
                            .size()
                            .x;
                        pane_aux_tab_width(pane_aux_tab_label_width(
                            header.width(),
                            natural,
                            ws.aux_tabs.len(),
                        )) + PANE_AUX_TAB_RIGHT_PAD
                    })
                    .sum();
                let title_width = ui
                    .painter()
                    .layout_no_wrap(snapshot.title.clone(), font, egui::Color32::WHITE)
                    .size()
                    .x;
                let buttons = pane_header_buttons(header, title_width, 4, aux_reserved);
                assert_eq!(
                    buttons.toolbar.len(),
                    4,
                    "테스트 헤더는 도구 4개가 모두 보여야 한다"
                );
                center = Some(buttons.toolbar[0].center());
            })
            .drop_without_applying_deltas();
        center.expect("Search 버튼 rect를 계산해야 한다")
    }

    /// 회귀 방지 계약: 보조 본문(이력·Git)이 비활성이면 Search 버튼은 지금까지처럼
    /// 터미널 검색을 연다 — `aux_search_toggle_requested`가 오르면 안 된다.
    #[test]
    fn kittest_보조본문_비활성이면_검색버튼은_기존_터미널검색을_연다() {
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let config = TerminalConfig::default();
        let mut ws = WorkspaceUi::new();
        ws.mux = Some(mux(
            "t",
            vec![tab(
                "t",
                vec![pane("p", SessionId(7))],
                LayoutNode::Pane(pane_id("p")),
            )],
            "p",
        ));
        let header = egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(520.0, TERMINAL_PANE_HEADER_HEIGHT),
        );
        let snapshot = pane("p", SessionId(7));
        let search_center = search_toolbar_button_center(&ws, header, &snapshot);

        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, state: &mut (WorkspaceUi, bool)| {
                let output = state
                    .0
                    .render_pane_header(ui, header, &snapshot, true, &config, &catalog, true);
                // `aux_search_toggle_requested`는 프레임마다 새로 계산되는 값이라
                // `clicked()`가 참인 한 프레임에만 켜진다 — harness가 한 `run()`
                // 안에서 내부적으로 여러 번 그릴 수 있어 `|=`로 누적한다(덮어쓰면
                // 클릭 프레임 다음 그리기에서 다시 꺼진다).
                state.1 |= output.aux_search_toggle_requested;
            },
            (ws, false),
        );
        harness.run();

        harness.hover_at(search_center);
        harness.run();
        harness.drag_at(search_center);
        harness.run();
        harness.drop_at(search_center);
        harness.run();

        assert!(
            !harness.state().1,
            "보조 본문이 비활성이면 aux_search_toggle_requested가 오르면 안 된다"
        );
        assert_eq!(
            harness
                .state()
                .0
                .search
                .as_ref()
                .map(|search| search.session),
            Some(SessionId(7)),
            "보조 본문이 비활성이면 Search 버튼은 여전히 기존 터미널 검색을 연다(회귀 방지)"
        );
    }

    /// 보조 본문이 활성인 헤더에서는 Search 버튼이 `aux_search_toggle_requested`를
    /// 올리고, 기존 터미널 검색(`ws.search`)은 열지 않는다 — App이 그 intent로
    /// `aux_search.toggle()`을 부른다(스펙 "진입"). `input_enabled: false`로 App이
    /// 이 프레임에 실제로 넘기는 값(입력 소유권 fail-closed)을 재현한다.
    #[test]
    fn kittest_보조본문_활성이면_검색버튼은_보조검색_토글_의도를_올린다() {
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let config = TerminalConfig::default();
        let mut ws = WorkspaceUi::new();
        ws.mux = Some(mux(
            "t",
            vec![tab(
                "t",
                vec![pane("p", SessionId(7))],
                LayoutNode::Pane(pane_id("p")),
            )],
            "p",
        ));
        ws.set_aux_tabs(vec![PaneAuxTab {
            kind: PaneAuxTabKind::History,
            label: "History".to_owned(),
            active: true,
        }]);
        ws.aux_tab_pane = Some(pane_id("p"));
        let header = egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(520.0, TERMINAL_PANE_HEADER_HEIGHT),
        );
        let snapshot = pane("p", SessionId(7));
        let search_center = search_toolbar_button_center(&ws, header, &snapshot);

        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, state: &mut (WorkspaceUi, bool)| {
                let output = state
                    .0
                    .render_pane_header(ui, header, &snapshot, true, &config, &catalog, false);
                state.1 |= output.aux_search_toggle_requested;
            },
            (ws, false),
        );
        harness.run();

        harness.hover_at(search_center);
        harness.run();
        harness.drag_at(search_center);
        harness.run();
        harness.drop_at(search_center);
        harness.run();

        assert!(
            harness.state().1,
            "보조 본문이 활성이면 Search 버튼은 aux_search_toggle_requested를 올려야 한다"
        );
        assert!(
            harness.state().0.search.is_none(),
            "보조 검색으로 갈 때는 기존 터미널 검색을 열면 안 된다"
        );
    }

    /// 2026-08-20 사용자: `Claude · claude-opus-5`처럼 같은 낱말이 한 줄에 두 번
    /// 나올 필요가 없다. 모델명이 provider를 품으면 라벨을 뺀다.
    #[test]
    fn 모델명이_provider를_품으면_라벨을_빼서_중복을_없앤다() {
        let display = crate::agent_detect::AgentDisplay {
            kind: crate::agent_detect::AgentKind::Claude,
            model: Some("claude-opus-5".to_owned()),
            effort: None,
            context_pct: None,
            last_agent_summary: None,
            user_instruction: None,
        };

        assert_eq!(agent_info_line(&display), "claude-opus-5");
        assert!(model_implies_provider("Claude", "claude-opus-5"));
        assert!(!model_implies_provider("Codex", "gpt-5.6-sol"));
        // 모델이 아예 없으면 라벨만 남아야 한다 — 빈 줄이 되면 안 된다.
        let no_model = crate::agent_detect::AgentDisplay {
            model: None,
            ..display
        };
        assert_eq!(agent_info_line(&no_model), "Claude");
    }

    #[test]
    fn agent_info_line은_전송배지_없이_provider와_모델을_보여준다() {
        let display = crate::agent_detect::AgentDisplay {
            kind: crate::agent_detect::AgentKind::Codex,
            model: Some("gpt-test".to_owned()),
            effort: Some("high".to_owned()),
            context_pct: Some(69),
            last_agent_summary: Some("PR #124 코드 리뷰 완료".to_owned()),
            user_instruction: Some("PR #124를 검토해".to_owned()),
        };

        assert_eq!(
            agent_info_line(&display),
            "Codex · gpt-test · high · ctx 69%",
            "모델명이 provider를 안 품으면 라벨을 남긴다 — 어느 에이전트인지 알려주는 유일한 단서다"
        );
        let catalog = catalog();
        assert_eq!(
            agent_activity_line(
                &display,
                Some("deppy-sijo"),
                Some(SessionStatus::Running),
                &catalog
            ),
            "PR #124 코드 리뷰 완료"
        );

        let waiting_for_first_response = crate::agent_detect::AgentDisplay {
            last_agent_summary: None,
            ..display.clone()
        };
        assert_eq!(
            agent_activity_line(
                &waiting_for_first_response,
                Some("deppy-sijo"),
                Some(SessionStatus::Running),
                &catalog
            ),
            "PR #124를 검토해"
        );

        let no_transcript_context = crate::agent_detect::AgentDisplay {
            user_instruction: None,
            ..waiting_for_first_response
        };
        assert_eq!(
            agent_activity_line(
                &no_transcript_context,
                Some("deppy-sijo"),
                Some(SessionStatus::Running),
                &catalog
            ),
            "deppy-sijo"
        );
        assert_eq!(
            session_status_label(Some(SessionStatus::Idle), &catalog),
            "Awaiting instruction"
        );
    }

    #[test]
    fn 한글_파일명은_wide_spacer를_넘어_한_단어로_잡힌다() {
        // "a nant-성과.pdf b" — 한글은 wide+spacer 2셀. 스페이서를 공백 취급하면
        // 단어가 첫 한글에서 끊긴다 (2026-07-14 "nant-성과분석.pdf 안 열림" 원인).
        fn push(cells: &mut Vec<TerminalCell>, c: char, wide: bool, spacer: bool) {
            cells.push(TerminalCell::new(
                c,
                [255; 3],
                [0; 3],
                wide,
                spacer,
                Default::default(),
            ));
        }
        let cols = 20usize;
        let mut cells = Vec::new();
        for c in "a nant-".chars() {
            push(&mut cells, c, false, false);
        }
        for c in ['성', '과'] {
            push(&mut cells, c, true, false);
            push(&mut cells, ' ', false, true);
        }
        for c in ".pdf b".chars() {
            push(&mut cells, c, false, false);
        }
        while cells.len() < cols {
            push(&mut cells, ' ', false, false);
        }
        let snap = TerminalViewportSnapshot {
            cols: cols as u16,
            rows: 1,
            cursor: CursorSnapshot {
                col: 0,
                row: 0,
                shape: CursorShape::Block,
                visible: true,
            },
            visible_cells: cells.into(),
            graphemes: Default::default(),
            dirty_ranges: Vec::new(),
            title: None,
            scroll_offset: 0,
            is_alt_screen: false,
        };
        // '성'(idx 7) 위 hover — 파일명 전체가 한 단어여야 한다(폴더 cd·URL 열기 판정용).
        // 더블클릭은 2026-08-17부터 `line_range_at`(행 전체)을 쓴다.
        let (s, e) = word_range_at(&snap, 7).expect("단어");
        assert_eq!(renderer_egui::selection_text(&snap, s, e), "nant-성과.pdf");
        // 공백(idx 1)은 여전히 단어가 아니다
        assert!(word_range_at(&snap, 1).is_none());
    }

    /// 행 문자열 목록으로 스냅샷 하나 — 부족한 칸은 공백으로 채운다.
    fn line_snap(cols: usize, lines: &[&str]) -> TerminalViewportSnapshot {
        let mut cells = Vec::new();
        for line in lines {
            let mut width = 0usize;
            for c in line.chars() {
                // 픽스처에서는 비ASCII를 2칸(wide)으로 본다 — 한글 검증에 충분하다.
                let wide = !c.is_ascii();
                cells.push(TerminalCell::new(
                    c,
                    [255; 3],
                    [0; 3],
                    wide,
                    false,
                    Default::default(),
                ));
                width += 1;
                if wide {
                    cells.push(TerminalCell::new(
                        ' ',
                        [255; 3],
                        [0; 3],
                        false,
                        true,
                        Default::default(),
                    ));
                    width += 1;
                }
            }
            while width < cols {
                cells.push(TerminalCell::new(
                    ' ',
                    [255; 3],
                    [0; 3],
                    false,
                    false,
                    Default::default(),
                ));
                width += 1;
            }
        }
        TerminalViewportSnapshot {
            cols: cols as u16,
            rows: lines.len() as u16,
            cursor: CursorSnapshot {
                col: 0,
                row: 0,
                shape: CursorShape::Block,
                visible: true,
            },
            visible_cells: cells.into(),
            graphemes: Default::default(),
            dirty_ranges: Vec::new(),
            title: None,
            scroll_offset: 0,
            is_alt_screen: false,
        }
    }

    #[test]
    fn 행_선택은_행_전체를_잡고_끝_공백은_뺀다() {
        let snap = line_snap(12, &["ls -la", "second row"]);
        let (s, e) = line_range_at(&snap, 3).expect("행");
        assert_eq!(s, 0, "행 시작(0열)부터다 — 앞 들여쓰기도 행의 일부다");
        assert_eq!(
            renderer_egui::selection_text(&snap, s, e),
            "ls -la",
            "끝의 빈 칸은 선택에 넣지 않는다"
        );
    }

    /// 3연클릭이 방금 만든 행 선택을 지우면 안 된다. egui `double_clicked()`는 count==2
    /// 에서만 참이라, 트리플을 함께 받지 않으면 else-if 사슬 끝의 단일 클릭 분기가
    /// `selection = None`을 실행한다(2026-08-18 리뷰).
    ///
    /// 제스처 배선은 렌더 안에 있어 순수 함수로 뽑을 수 없다 — 소스 계약으로 고정한다.
    /// 이 테스트가 지키는 것은 **배선**이고, 행 범위 계산 자체는 위 `line_range_at`
    /// 테스트들이 값으로 검증한다.
    #[test]
    fn 트리플클릭도_행_선택으로_받는다() {
        let source = include_str!("workspace.rs");
        let production = source.split_once("#[cfg(test)]\nmod tests").unwrap().0;
        let branch = production
            .split_once("line_range_at(&snapshot, cell_at(pos))")
            .expect("행 선택 분기가 있어야 한다")
            .0;
        // 분기 조건은 그 호출 **직전**에 온다 — 뒤에서부터 가장 가까운 조건을 본다.
        let condition = branch
            .rsplit_once("if (")
            .expect("더블/트리플을 함께 받는 조건이어야 한다")
            .1;
        assert!(
            condition.contains("double_clicked()") && condition.contains("triple_clicked()"),
            "더블·트리플 둘 다 받아야 3연클릭이 선택을 지우지 않는다: {condition:?}"
        );
    }

    #[test]
    fn 행_선택은_클릭한_행만_잡는다() {
        let snap = line_snap(12, &["first", "second row"]);
        // 두 번째 행(base 12)의 아무 칸이나
        let (s, e) = line_range_at(&snap, 12 + 4).expect("행");
        assert_eq!(s, 12);
        assert_eq!(renderer_egui::selection_text(&snap, s, e), "second row");
    }

    #[test]
    fn 빈_행은_선택하지_않는다() {
        // 빈 줄을 더블클릭해도 아무 일도 일어나지 않는다(공백 위 단어 선택과 같은 감각).
        let snap = line_snap(12, &["", "   "]);
        assert!(line_range_at(&snap, 3).is_none());
        assert!(line_range_at(&snap, 12 + 1).is_none());
    }

    /// 2칸 글자가 행 끝에 안 들어가 다음 줄로 밀리면 그 행 마지막 칸에 **필러**가 남는다
    /// (alacritty `LEADING_WIDE_CHAR_SPACER` / ghostty `SpacerHead`). 눈에는 빈 행인데
    /// `wide_spacer` 비트만 서 있어서, 구분하지 않으면 더블클릭에 강조 막대가 생긴다
    /// (2026-08-18 리뷰가 실제 백엔드로 실측).
    #[test]
    fn 행_끝_wrap_필러만_있는_행은_선택하지_않는다() {
        let mut snap = line_snap(6, &[""]);
        let cells: &mut Vec<TerminalCell> = &mut snap.visible_cells.to_vec();
        // 마지막 칸만 필러로 만든다 — 앞 칸은 소유자(wide)가 아니라 그냥 공백이다.
        cells[5].set_wide_spacer(true);
        snap.visible_cells = cells.clone().into();

        assert!(
            !snap.is_trailing_wide_spacer(5),
            "앞 칸이 소유자가 아니면 진짜 뒷칸이 아니다"
        );
        assert!(
            line_range_at(&snap, 2).is_none(),
            "눈에 빈 행은 더블클릭해도 선택하지 않는다"
        );
    }

    #[test]
    fn 행_선택은_wide_문자로_끝나도_자리_채움까지_포함한다() {
        // 한글로 끝나는 행에서 자리 채움 셀을 빼면 마지막 글자가 잘려 보인다.
        let snap = line_snap(12, &["ok 한글"]);
        let (s, e) = line_range_at(&snap, 0).expect("행");
        assert_eq!(renderer_egui::selection_text(&snap, s, e), "ok 한글");
    }

    #[test]
    fn 페이스트_fallback은_같은_제스처의_native또는_text처리_직후에만_스킵된다() {
        let now = std::time::Instant::now();
        let old = now.checked_sub(PASTE_GESTURE_WINDOW * 2);
        // native key-down은 이전 이력과 무관하게 새 제스처를 시작한다.
        assert!(!should_skip_paste_task(
            ClipboardPasteTrigger::NativeKeyDown,
            false,
            Some(now),
            Some(now),
        ));
        // press에서 Event::Paste 텍스트를 이미 보냈고 fallback이 없다 → key-up 스킵.
        assert!(should_skip_paste_task(
            ClipboardPasteTrigger::EguiShortcut,
            false,
            Some(now),
            None,
        ));
        // native key-down에서 이미지 작업을 시작한 뒤의 key-up도 스킵.
        assert!(should_skip_paste_task(
            ClipboardPasteTrigger::EguiShortcut,
            false,
            None,
            Some(now),
        ));
        // 같은 프레임에 Paste가 왔다(fallback 보유) → 태스크가 fallback을 쓰므로 진행.
        assert!(!should_skip_paste_task(
            ClipboardPasteTrigger::EguiShortcut,
            true,
            Some(now),
            Some(now),
        ));
        // 처리 이력이 없거나 오래됐다 → fallback이 유일한 감지 경로라 진행.
        assert!(!should_skip_paste_task(
            ClipboardPasteTrigger::EguiShortcut,
            false,
            None,
            None,
        ));
        if let Some(old) = old {
            assert!(!should_skip_paste_task(
                ClipboardPasteTrigger::EguiShortcut,
                false,
                Some(old),
                Some(old),
            ));
        }
    }

    /// 세션 행 점프 강조는 **새 기구를 만들지 않고** 기존 `session_flash`에 얹는다.
    /// 그래야 여러 pane 사이를 점프할 때 `FOCUS_FLASH`와 같은 pane에 테두리가 두 겹으로
    /// 그려지지 않는다(2026-08-18).
    #[test]
    fn flash_pane은_그_pane의_세션을_기존_플래시_기구에_넣는다() {
        let mut ws = WorkspaceUi::new();
        let session = SessionId(7);
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        ws.apply_warm_events(
            &[RuntimeEvent::MuxUpdated {
                snapshot: mux(
                    "t1",
                    vec![tab(
                        "t1",
                        vec![pane("p1", session)],
                        LayoutNode::Pane(pane_id("p1")),
                    )],
                    "p1",
                ),
            }],
            &catalog,
        );

        // 포커스 변경으로 들어간 플래시가 있으면 이 테스트의 전제가 흐려진다 — 비우고 시작한다.
        ws.session_flash.clear();
        ws.flash_pane(&pane_id("p1"));
        assert!(
            ws.session_flash.contains_key(&session),
            "점프한 pane의 세션에 플래시가 들어가야 한다"
        );
    }

    #[test]
    fn flash_pane은_모르는_pane이면_아무것도_하지_않는다() {
        let mut ws = WorkspaceUi::new();
        let session = SessionId(7);
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        ws.apply_warm_events(
            &[RuntimeEvent::MuxUpdated {
                snapshot: mux(
                    "t1",
                    vec![tab(
                        "t1",
                        vec![pane("p1", session)],
                        LayoutNode::Pane(pane_id("p1")),
                    )],
                    "p1",
                ),
            }],
            &catalog,
        );

        ws.session_flash.clear();
        ws.flash_pane(&pane_id("없는pane"));
        assert!(ws.session_flash.is_empty(), "모르는 pane은 무시한다");
    }

    fn tab_id(name: &str) -> MuxTabId {
        MuxTabId(name.to_owned())
    }

    fn pane(id: &str, session: SessionId) -> PaneSnapshot {
        PaneSnapshot {
            id: pane_id(id),
            session_id: Some(session),
            title: id.to_owned(),
            persistent_session_id: None,
        }
    }

    fn tab(id: &str, panes: Vec<PaneSnapshot>, layout: LayoutNode) -> TabSnapshot {
        TabSnapshot {
            id: tab_id(id),
            title: id.to_owned(),
            layout,
            panes,
        }
    }

    fn mux(active: &str, tabs: Vec<TabSnapshot>, focused: &str) -> Arc<MuxSnapshot> {
        Arc::new(MuxSnapshot {
            tabs,
            active_tab: Some(tab_id(active)),
            focused_pane: Some(pane_id(focused)),
        })
    }

    #[test]
    fn composer_growth_keeps_nested_pane_sizes_and_window_resize_still_changes_them() {
        let config = TerminalConfig::default();
        let catalog = catalog();
        let mut workspace = WorkspaceUi::new();
        let sessions = [SessionId(71), SessionId(72), SessionId(73)];
        let panes = ["upper", "lower-left", "lower-right"];
        workspace.mux = Some(mux(
            "primary",
            vec![tab(
                "primary",
                panes
                    .iter()
                    .zip(sessions)
                    .map(|(name, session)| pane(name, session))
                    .collect(),
                LayoutNode::Split {
                    direction: SplitDirection::Vertical,
                    ratio: 0.4,
                    first: Box::new(LayoutNode::Pane(pane_id(panes[0]))),
                    second: Box::new(LayoutNode::Split {
                        direction: SplitDirection::Horizontal,
                        ratio: 0.5,
                        first: Box::new(LayoutNode::Pane(pane_id(panes[1]))),
                        second: Box::new(LayoutNode::Pane(pane_id(panes[2]))),
                    }),
                },
            )],
            panes[0],
        ));
        for session in sessions {
            workspace.sessions.entry(session).or_default().snapshot = Some(snapshot("ready"));
        }
        let history = std::env::temp_dir().join(format!(
            "deppy-composer-layout-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui,
                  (workspace, composer, sizes): &mut (
                WorkspaceUi,
                super::super::composer::ComposerUi,
                HashMap<SessionId, (u16, u16)>,
            )| {
                let connectors = connector_contract::ConnectorSnapshot::default();
                let context = super::super::composer::ComposerContext {
                    workspace_id: "composer-ws",
                    draft_key: "composer-ws",
                    runtime_generation: 1,
                    send_key: crate::config::ComposerSendKey::Enter,
                    can_send: true,
                    agent: None,
                    workspace_root: None,
                    collapse_shortcut: None,
                    connector_snapshot: &connectors,
                };
                let frame = egui::Frame::NONE.inner_margin(egui::Margin::symmetric(10, 9));
                let before = ui.available_height();
                egui::Panel::bottom("composer-test")
                    .frame(frame)
                    .resizable(false)
                    .show(ui, |ui| {
                        composer.render(ui, &catalog, &context);
                    });
                workspace.set_composer_height_expansion(
                    before
                        - ui.available_height()
                        - frame.total_margin().sum().y
                        - composer.compact_height(),
                );
                workspace.show_with_input(ui, &config, &[], &catalog, false);
                if !ui.is_sizing_pass() {
                    *sizes = workspace
                        .staged_terminal_resizes
                        .iter()
                        .map(|(session, resize)| (*session, (resize.cols, resize.rows)))
                        .collect();
                }
            },
            (
                workspace,
                super::super::composer::ComposerUi::new(history),
                HashMap::new(),
            ),
        );
        harness.run_steps(20);
        let compact = harness.state().2.clone();
        assert_eq!(compact.len(), 3);
        harness.state_mut().1.request_focus();
        harness.run_steps(20);
        assert_eq!(harness.state().2, compact);
        harness
            .state_mut()
            .1
            .insert_text("composer-ws", &"긴 한글 입력\n".repeat(40));
        harness.run_steps(20);
        assert_eq!(harness.state().2, compact);
        harness.event(egui::Event::PointerMoved(egui::pos2(20.0, 20.0)));
        harness.event(egui::Event::PointerButton {
            pos: egui::pos2(20.0, 20.0),
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        });
        harness.event(egui::Event::PointerButton {
            pos: egui::pos2(20.0, 20.0),
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        });
        harness.run_steps(20);
        assert_eq!(harness.state().2, compact);
        harness.set_size(egui::vec2(600.0, 420.0));
        harness.run_steps(20);
        assert_ne!(
            harness.state().2,
            compact,
            "actual window resizing must still resize PTYs"
        );
    }

    #[test]
    fn pane_너비가_바뀌면_열수도_따라가고_적용전_snapshot은_보존한다() {
        let config = TerminalConfig::default();
        let catalog = catalog();
        let session = SessionId(71);
        // 이전 전송 폭이나 snapshot 폭에 고정되지 않고 현재 pane 폭을 따른다.
        for (has_snapshot, sent_cols) in [(true, None), (true, Some(100)), (false, Some(100))] {
            let mut workspace = WorkspaceUi::new();
            workspace.mux = Some(mux(
                "t",
                vec![tab(
                    "t",
                    vec![pane("p", session)],
                    LayoutNode::Pane(pane_id("p")),
                )],
                "p",
            ));
            let original =
                shaped_snapshot(80, 24, "| 기존 표 | 오른쪽 경계 | 기존 줄바꿈을 유지한다 |");
            if has_snapshot {
                workspace
                    .sessions
                    .entry(session)
                    .or_default()
                    .install_snapshot(Arc::clone(&original));
            }
            if let Some(cols) = sent_cols {
                workspace.sent_sizes.insert(session, (cols, 24));
            }
            let ctx = egui::Context::default();
            let mut previous = None;
            for width in [300.0, 900.0, 240.0, 900.0] {
                ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(width, 500.0),
                        )),
                        ..Default::default()
                    },
                    |ui| {
                        workspace.show_with_input(ui, &config, &[], &catalog, false);
                    },
                )
                .textures_delta
                .clear();
                let target = workspace
                    .staged_terminal_resizes
                    .get(&session)
                    .expect("크기 요청이 staging되어야 한다");
                if let Some((previous_width, previous_cols)) = previous {
                    assert_eq!(
                        target.cols > previous_cols,
                        width > previous_width,
                        "pane 폭이 바뀌면 열 수도 같은 방향으로 바뀌어야 한다"
                    );
                    assert_ne!(target.cols, previous_cols);
                }
                previous = Some((width, target.cols));
                if has_snapshot {
                    assert!(Arc::ptr_eq(
                        workspace.sessions[&session].snapshot.as_ref().unwrap(),
                        &original
                    ));
                }
            }
        }
    }

    #[test]
    fn 복원_화면이_늦어도_추측한_폭을_먼저_보내지_않는다() {
        let config = TerminalConfig::default();
        let catalog = catalog();
        let session = SessionId(72);
        let mut workspace = WorkspaceUi::new();
        workspace.mux = Some(mux(
            "t",
            vec![tab(
                "t",
                vec![pane("p", session)],
                LayoutNode::Pane(pane_id("p")),
            )],
            "p",
        ));
        let ctx = egui::Context::default();
        let render = |workspace: &mut WorkspaceUi| {
            ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(300.0, 500.0),
                    )),
                    ..Default::default()
                },
                |ui| {
                    workspace.show_with_input(ui, &config, &[], &catalog, false);
                },
            )
            .textures_delta
            .clear();
        };

        // MuxUpdated와 Viewport가 서로 다른 프레임에 도착해도 첫 화면을 받은 뒤
        // 현재 pane 크기에 맞춘 Resize를 예약한다.
        render(&mut workspace);
        assert!(
            !workspace.staged_terminal_resizes.contains_key(&session),
            "첫 snapshot 없이 추측한 폭으로 Resize를 예약하면 안 된다"
        );
        workspace
            .sessions
            .entry(session)
            .or_default()
            .install_snapshot(shaped_snapshot(120, 24, "| 복원한 표의 원래 폭 |"));
        render(&mut workspace);
        assert!(workspace.staged_terminal_resizes[&session].cols < 120);
    }

    #[test]
    fn 복원_열수와_무관하게_pane에_맞추고_resize_셀_상한을_지킨다() {
        let config = TerminalConfig::default();
        let catalog = catalog();
        let session = SessionId(73);
        let mut pane_cols = None;
        for cols in [1, 80, 328, 360, 500] {
            let mut workspace = WorkspaceUi::new();
            workspace.mux = Some(mux(
                "t",
                vec![tab(
                    "t",
                    vec![pane("p", session)],
                    LayoutNode::Pane(pane_id("p")),
                )],
                "p",
            ));
            workspace
                .sessions
                .entry(session)
                .or_default()
                .install_snapshot(shaped_snapshot(cols, 24, "| 넓게 저장된 기존 표 |"));
            let ctx = egui::Context::default();
            ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(240.0, 900.0),
                    )),
                    ..Default::default()
                },
                |ui| {
                    workspace.show_with_input(ui, &config, &[], &catalog, false);
                },
            )
            .textures_delta
            .clear();
            let target = &workspace.staged_terminal_resizes[&session];
            if let Some(expected) = pane_cols {
                assert_eq!(
                    target.cols, expected,
                    "같은 pane 폭은 복원 열 수와 무관해야 한다"
                );
            }
            pane_cols = Some(target.cols);
            // 숫자를 테스트에 복사하지 않고 실제 런타임 명령 입장 검사를 통과해야 한다.
            let mut command = RuntimeCommand::Resize {
                session,
                cols: target.cols,
                rows: target.rows,
            };
            assert!(
                runtime::prepare_runtime_command_for_retention(&mut command).is_ok(),
                "축소 화면의 Resize {}×{}가 런타임 상한에 거부됐다",
                target.cols,
                target.rows
            );
        }
    }

    fn snapshot(text: &str) -> Arc<TerminalViewportSnapshot> {
        shaped_snapshot(12, 2, text)
    }

    fn shaped_snapshot(cols: usize, rows: usize, text: &str) -> Arc<TerminalViewportSnapshot> {
        let mut cells = Vec::with_capacity(cols * rows);
        let chars: Vec<char> = text.chars().collect();
        for idx in 0..cols * rows {
            cells.push(TerminalCell::new(
                chars.get(idx).copied().unwrap_or(' '),
                [255, 255, 255],
                [0, 0, 0],
                false,
                false,
                Default::default(),
            ));
        }
        Arc::new(TerminalViewportSnapshot {
            cols: cols as u16,
            rows: rows as u16,
            cursor: CursorSnapshot {
                col: 0,
                row: 0,
                shape: CursorShape::Block,
                visible: true,
            },
            visible_cells: cells.into(),
            graphemes: Default::default(),
            dirty_ranges: Vec::new(),
            title: None,
            scroll_offset: 0,
            is_alt_screen: false,
        })
    }

    fn catalog() -> i18n::Catalog {
        i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap()
    }

    /// warm 워크스페이스는 이벤트를 replay용으로만 쌓아 regex status와 mux 구조가
    /// warm 진입 시점에 얼어붙었다 — 표시 상태 3종(mux/status/종료 결과)은 즉시
    /// 반영해야 fleet 카드와 사이드바가 warm을 정확히 보여준다(백로그 4).
    #[test]
    fn fleet_review_fix_first_open_and_hidden_submit_state_is_scoped_and_pruned() {
        for hidden in [false, true] {
            let mut ui = WorkspaceUi::new();
            let catalog = catalog();
            let both = mux(
                "a",
                vec![tab(
                    "a",
                    vec![pane("pa", SessionId(1)), pane("pb", SessionId(2))],
                    LayoutNode::Pane(pane_id("pa")),
                )],
                "pa",
            );
            let events = [
                RuntimeEvent::MuxUpdated {
                    snapshot: Arc::clone(&both),
                },
                RuntimeEvent::SessionInputSubmitted {
                    session: SessionId(1),
                    at_micros: 200_000_000,
                },
                RuntimeEvent::SessionInputSubmitted {
                    session: SessionId(1),
                    at_micros: 100_000_000,
                },
                RuntimeEvent::SessionInputSubmitted {
                    session: SessionId(9),
                    at_micros: 300_000_000,
                },
            ];
            if hidden {
                ui.apply_warm_events(&events, &catalog);
            } else {
                ui.handle_events(&events, &catalog);
            }
            assert_eq!(ui.last_input_submission(SessionId(1)), Some(200_000_000));
            assert_eq!(ui.last_input_submission(SessionId(2)), None);
            assert_eq!(ui.last_input_submission(SessionId(9)), None);
            let empty = mux("a", Vec::new(), "pa");
            if hidden {
                ui.apply_warm_events(&[RuntimeEvent::MuxUpdated { snapshot: empty }], &catalog);
            } else {
                ui.handle_events(&[RuntimeEvent::MuxUpdated { snapshot: empty }], &catalog);
            }
            assert_eq!(ui.last_input_submission(SessionId(1)), None);
        }
    }

    #[test]
    fn warm_이벤트는_mux와_status와_종료결과를_즉시_반영한다() {
        let mut ui = WorkspaceUi::new();
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let both = mux(
            "a",
            vec![tab(
                "a",
                vec![pane("pa", SessionId(1)), pane("pb", SessionId(2))],
                LayoutNode::Pane(pane_id("pa")),
            )],
            "pa",
        );
        ui.apply_warm_events(
            &[
                RuntimeEvent::MuxUpdated {
                    snapshot: Arc::clone(&both),
                },
                RuntimeEvent::SessionStatusChanged {
                    session: SessionId(1),
                    status: SessionStatus::Running,
                },
                RuntimeEvent::SessionExited {
                    session: SessionId(2),
                    exit_code: Some(1),
                },
            ],
            &catalog,
        );
        assert_eq!(
            ui.last_session_status(SessionId(1)),
            Some(SessionStatus::Running)
        );
        assert_eq!(
            ui.last_session_status(SessionId(2)),
            Some(SessionStatus::Error)
        );

        // mux에 없는 세션은 무시한다 — handle_events와 같은 liveness 규칙.
        ui.apply_warm_events(
            &[RuntimeEvent::SessionStatusChanged {
                session: SessionId(9),
                status: SessionStatus::Running,
            }],
            &catalog,
        );
        assert_eq!(ui.last_session_status(SessionId(9)), None);

        // 사라진 pane의 상태는 다음 mux 스냅샷에서 정리된다.
        let only_pa = mux(
            "a",
            vec![tab(
                "a",
                vec![pane("pa", SessionId(1))],
                LayoutNode::Pane(pane_id("pa")),
            )],
            "pa",
        );
        ui.apply_warm_events(&[RuntimeEvent::MuxUpdated { snapshot: only_pa }], &catalog);
        assert_eq!(ui.last_session_status(SessionId(2)), None);
        assert_eq!(
            ui.last_session_status(SessionId(1)),
            Some(SessionStatus::Running)
        );
    }

    #[test]
    fn durable_event_barrier_is_inert_for_workspace_ui() {
        let mut ui = WorkspaceUi::new();
        let catalog = catalog();
        let initial_mux = mux(
            "a",
            vec![tab(
                "a",
                vec![pane("pa", SessionId(1))],
                LayoutNode::Pane(pane_id("pa")),
            )],
            "pa",
        );
        ui.apply_warm_events(
            &[RuntimeEvent::MuxUpdated {
                snapshot: Arc::clone(&initial_mux),
            }],
            &catalog,
        );
        let mux_before = Arc::clone(ui.mux().unwrap());
        let focused_before = ui.focused_session();
        let session_count_before = ui.sessions.len();
        let pending_spawns_before = ui.pending_spawns();

        ui.handle_events(
            &[RuntimeEvent::DurableEventBarrierReached { correlation_id: 41 }],
            &catalog,
        );
        ui.apply_warm_events(
            &[RuntimeEvent::DurableEventBarrierReached { correlation_id: 41 }],
            &catalog,
        );

        assert!(Arc::ptr_eq(ui.mux().unwrap(), &mux_before));
        assert_eq!(ui.focused_session(), focused_before);
        assert_eq!(ui.sessions.len(), session_count_before);
        assert_eq!(ui.pending_spawns(), pending_spawns_before);
        assert!(ui.protocol_intents.is_empty());
    }

    #[test]
    fn session_maintenance_events는_workspace_ui에서_상태를_바꾸지_않는다() {
        let mut ui = WorkspaceUi::new();
        let catalog = catalog();
        let initial_mux = mux(
            "a",
            vec![tab(
                "a",
                vec![pane("pa", SessionId(1))],
                LayoutNode::Pane(pane_id("pa")),
            )],
            "pa",
        );
        ui.apply_warm_events(
            &[RuntimeEvent::MuxUpdated {
                snapshot: Arc::clone(&initial_mux),
            }],
            &catalog,
        );
        ui.error = Some("keep workspace state".to_owned());
        let mux_before = Arc::clone(ui.mux().unwrap());
        let focused_before = ui.focused_session();
        let session_count_before = ui.sessions.len();

        let events = [
            RuntimeEvent::UnattachedSessionsInspected { count: 7 },
            RuntimeEvent::UnattachedSessionsKilled { count: 3 },
        ];
        ui.handle_events(&events, &catalog);
        ui.apply_warm_events(&events, &catalog);

        assert!(Arc::ptr_eq(ui.mux().unwrap(), &mux_before));
        assert_eq!(ui.focused_session(), focused_before);
        assert_eq!(ui.sessions.len(), session_count_before);
        assert_eq!(ui.error.as_deref(), Some("keep workspace state"));
        assert!(ui.protocol_intents.is_empty());
    }

    #[test]
    fn split_target는_focus_없으면_활성탭_첫_pane으로_폴백한다() {
        let tab_a = tab(
            "a",
            vec![pane("pa", SessionId(1))],
            LayoutNode::Pane(pane_id("pa")),
        );
        let tab_b = tab(
            "b",
            vec![pane("pb", SessionId(2))],
            LayoutNode::Pane(pane_id("pb")),
        );
        let tabs = vec![tab_a, tab_b];

        // focus가 있으면 그대로 쓴다.
        let with_focus = MuxSnapshot {
            tabs: tabs.clone(),
            active_tab: Some(tab_id("a")),
            focused_pane: Some(pane_id("pb")),
        };
        assert_eq!(split_target_pane(&with_focus), Some(pane_id("pb")));

        // focus가 없으면 활성 탭(b)의 첫 pane으로 폴백.
        let no_focus = MuxSnapshot {
            tabs: tabs.clone(),
            active_tab: Some(tab_id("b")),
            focused_pane: None,
        };
        assert_eq!(split_target_pane(&no_focus), Some(pane_id("pb")));

        // focus·active_tab 둘 다 없으면 첫 탭의 첫 pane.
        let nothing = MuxSnapshot {
            tabs,
            active_tab: None,
            focused_pane: None,
        };
        assert_eq!(split_target_pane(&nothing), Some(pane_id("pa")));

        // 빈 워크스페이스면 분할 대상이 없다.
        let empty = MuxSnapshot {
            tabs: vec![],
            active_tab: None,
            focused_pane: None,
        };
        assert_eq!(split_target_pane(&empty), None);
    }

    #[test]
    fn muxupdated_뒤_hidden_viewport는_snapshot을_되살리지_않는다() {
        let hidden = SessionId(1);
        let visible = SessionId(2);
        let mut ui = WorkspaceUi::new();
        let catalog = catalog();

        let tab_a = tab(
            "a",
            vec![pane("pa", hidden)],
            LayoutNode::Pane(pane_id("pa")),
        );
        let tab_b = tab(
            "b",
            vec![pane("pb", visible)],
            LayoutNode::Pane(pane_id("pb")),
        );

        ui.handle_events(
            &[
                RuntimeEvent::MuxUpdated {
                    snapshot: mux("a", vec![tab_a.clone(), tab_b.clone()], "pa"),
                },
                RuntimeEvent::Viewport {
                    session: hidden,
                    snapshot: snapshot("old visible"),
                    bracketed_paste: false,
                },
            ],
            &catalog,
        );
        assert!(ui.sessions.get(&hidden).unwrap().snapshot.is_some());

        ui.handle_events(
            &[
                RuntimeEvent::MuxUpdated {
                    snapshot: mux("b", vec![tab_a, tab_b], "pb"),
                },
                RuntimeEvent::Viewport {
                    session: hidden,
                    snapshot: snapshot("stale hidden"),
                    bracketed_paste: false,
                },
            ],
            &catalog,
        );

        assert!(
            ui.sessions
                .get(&hidden)
                .is_none_or(|view| view.snapshot.is_none()),
            "hidden tab의 stale Viewport가 UI snapshot cache를 되살리면 안 된다"
        );
    }

    #[test]
    fn active_tab_split의_visible_pane들은_viewport를_받는다() {
        let left = SessionId(10);
        let right = SessionId(11);
        let mut ui = WorkspaceUi::new();
        let catalog = catalog();
        let split = LayoutNode::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(LayoutNode::Pane(pane_id("left"))),
            second: Box::new(LayoutNode::Pane(pane_id("right"))),
        };
        let active = tab(
            "active",
            vec![pane("left", left), pane("right", right)],
            split,
        );

        ui.handle_events(
            &[
                RuntimeEvent::MuxUpdated {
                    snapshot: mux("active", vec![active], "left"),
                },
                RuntimeEvent::Viewport {
                    session: left,
                    snapshot: snapshot("left"),
                    bracketed_paste: false,
                },
                RuntimeEvent::Viewport {
                    session: right,
                    snapshot: snapshot("right"),
                    bracketed_paste: true,
                },
            ],
            &catalog,
        );

        assert!(ui.sessions.get(&left).unwrap().snapshot.is_some());
        assert!(ui.sessions.get(&right).unwrap().snapshot.is_some());
        assert!(ui.sessions.get(&right).unwrap().bracketed_paste);
        assert!(!ui.session_bracketed_paste(left));
        assert!(ui.session_bracketed_paste(right));
    }

    /// LastOutputExtracted는 명시적인 copy 요청에만 반응한다.
    #[test]
    fn last_output_extracted는_copy요청만_클립보드에_예약한다() {
        let mut ui = WorkspaceUi::new();
        let catalog = catalog();
        let source = SessionId(1);

        // intent가 없으면 stale 응답 — 무시된다.
        ui.handle_events(
            &[RuntimeEvent::LastOutputExtracted {
                session: source,
                text: "stale".to_owned(),
                truncated: false,
            }],
            &catalog,
        );
        assert!(ui.pending_copy.is_none());

        // Copy 요청 — pending_copy에 예약되고 요청은 1회용으로 소비된다.
        ui.last_output_copy_pending.insert(source);
        ui.handle_events(
            &[RuntimeEvent::LastOutputExtracted {
                session: source,
                text: "out".to_owned(),
                truncated: false,
            }],
            &catalog,
        );
        assert_eq!(ui.pending_copy.as_deref(), Some("out"));
        assert!(ui.last_output_copy_pending.is_empty());
    }

    #[test]
    fn last_output이_비면_native_effect대신_notice_intent를_발행한다() {
        let mut ui = WorkspaceUi::new();
        let catalog = catalog();
        let source = SessionId(2);
        ui.last_output_copy_pending.insert(source);

        ui.handle_events(
            &[RuntimeEvent::LastOutputExtracted {
                session: source,
                text: String::new(),
                truncated: false,
            }],
            &catalog,
        );

        assert!(ui.pending_copy.is_none());
        assert!(ui.last_output_copy_pending.is_empty());
        let notice = ui.take_notice_intent().expect("no-output notice intent");
        assert_eq!(notice.summary(), catalog.t("shell.no_output_marks", &[]));
        assert_eq!(notice.body(), "");
        let debug = format!("{notice:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains(notice.summary()));
        assert!(ui.take_notice_intent().is_none());
    }

    #[test]
    fn workspace_notice는_capacity_one_latest_overwrite다() {
        let mut ui = WorkspaceUi::new();
        ui.stage_notice("first".to_owned(), "first body");
        ui.stage_notice("second".to_owned(), "second body");

        let notice = ui.take_notice_intent().expect("latest notice");
        assert_eq!(notice.summary(), "second");
        assert_eq!(notice.body(), "second body");
        assert!(ui.take_notice_intent().is_none());
    }

    #[test]
    fn workspace_notice_payload는_field와_total_byte_budget을_지킨다() {
        assert!(WorkspaceNotice::try_new("summary".to_owned(), "").is_some());
        assert!(
            WorkspaceNotice::try_new(
                "s".repeat(WORKSPACE_NOTICE_SUMMARY_MAX_BYTES),
                &"b".repeat(1024),
            )
            .is_some()
        );
        assert!(
            WorkspaceNotice::try_new("s".repeat(WORKSPACE_NOTICE_SUMMARY_MAX_BYTES + 1), "",)
                .is_none()
        );
        assert!(
            WorkspaceNotice::try_new(
                "summary".to_owned(),
                &"b".repeat(WORKSPACE_NOTICE_BODY_MAX_BYTES + 1),
            )
            .is_none()
        );
        assert!(WorkspaceNotice::try_new("s".repeat(3500), &"b".repeat(1800)).is_none());
        assert!(WorkspaceNotice::try_new(String::new(), "").is_none());
        assert!(WorkspaceNotice::try_new("contains\0nul".to_owned(), "").is_none());
        assert!(WorkspaceNotice::try_new("summary".to_owned(), "contains\0nul").is_none());

        let mut ui = WorkspaceUi::new();
        ui.stage_notice("retained".to_owned(), "");
        ui.stage_notice("x".repeat(WORKSPACE_NOTICE_SUMMARY_MAX_BYTES + 1), "");
        assert_eq!(
            ui.take_notice_intent()
                .expect("invalid overwrite keeps valid notice")
                .summary(),
            "retained"
        );
    }

    #[test]
    fn workspace_production_source에는_platform_notification_effect가_없다() {
        let source = include_str!("workspace.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("production source");
        for forbidden in [
            "platform::notify(",
            "notify_rust",
            "osascript",
            "RuntimeClient",
            "send_command(",
            "RuntimeCommandSink",
        ] {
            assert!(
                !production.contains(forbidden),
                "workspace production source contains a direct effect: {forbidden}"
            );
        }
    }

    #[test]
    fn workspace_production_title_path에는_project_name_filesystem_probe가_없다() {
        let source = include_str!("workspace.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("production source");
        for forbidden in [
            "agent_detect::project_display_name",
            "std::fs::",
            "fs::metadata(",
            "read_dir(",
            "canonicalize(",
        ] {
            assert!(
                !production.contains(forbidden),
                "workspace production title path contains filesystem lookup: {forbidden}"
            );
        }
    }

    #[test]
    fn environment_context_same_session_number_keeps_original_runtime_cwd() {
        let session = SessionId(7);
        let mut primary = WorkspaceUi::new();
        let mut attached = WorkspaceUi::new();
        primary.session_cwds.insert(session, "/primary".into());
        attached.session_cwds.insert(session, "/attached".into());
        let first = primary.environment_open_request(Some(session), None);
        let second = attached.environment_open_request(Some(session), None);
        assert_eq!(first.cwd.as_deref(), Some("/primary"));
        assert_eq!(second.cwd.as_deref(), Some("/attached"));
        assert!(
            attached
                .environment_open_request(Some(SessionId(999)), None)
                .cwd
                .is_none()
        );
    }

    #[test]
    fn precomputed_project_name_snapshot은_exact_session_cwd에만_적용된다() {
        let catalog = i18n::Catalog::load("en-US").unwrap();
        let session = SessionId(7);
        let mut ui = WorkspaceUi::new();
        ui.set_session_cwds(
            HashMap::from([(session, "/workspace/repo".to_owned())]),
            crate::config::SessionNameStyle::Repo,
        );
        let snapshot = SessionProjectNameSnapshot::try_new(
            1,
            vec![(
                session,
                "/workspace/repo".to_owned(),
                "precomputed-repo".to_owned(),
            )],
        )
        .unwrap();
        ui.set_session_project_names(snapshot);

        assert_eq!(
            ui.resolve_session_title("workspace.spawn.shell 1", Some(session), None, &catalog),
            "precomputed-repo"
        );
        assert_eq!(
            ui.session_project_context(Some(session)),
            Some("precomputed-repo".to_owned())
        );
        ui.set_session_cwds(
            HashMap::from([(session, "/workspace/other".to_owned())]),
            crate::config::SessionNameStyle::Repo,
        );
        assert_eq!(
            ui.session_project_context(Some(session)),
            Some("other".to_owned())
        );
        assert!(ui.session_project_names.entries.is_empty());
        assert_eq!(
            ui.resolve_session_title(
                "workspace.spawn.shell 1",
                Some(session),
                Some("osc-title"),
                &catalog,
            ),
            "osc-title"
        );
        ui.set_session_cwds(HashMap::new(), crate::config::SessionNameStyle::Repo);
        ui.set_project_name(Some("active-workspace".to_owned()));
        assert_eq!(
            ui.session_project_context(Some(session)),
            None,
            "세션 cwd가 없으면 활성 workspace 이름을 해당 세션의 작업으로 단정하지 않는다"
        );
    }

    /// 2026-08-19 사용자 보고 재현: Crawler 워크스페이스의 세션인데 cwd가 우연히 다른
    /// 이름(Design)의 폴더를 가리켜, 프로젝트명만 단독으로 보이면 "다른 워크스페이스의
    /// 세션이 섞여 들어왔다"는 착각을 준다. 워크스페이스 자체 이름과 cwd 프로젝트명이
    /// 다르면 "프로젝트명 (워크스페이스명)"으로 소속을 함께 밝혀야 한다.
    #[test]
    fn cwd_project_name이_워크스페이스_자체_이름과_다르면_소속을_함께_보여준다() {
        let catalog = i18n::Catalog::load("en-US").unwrap();
        let session = SessionId(20);
        let mut ui = WorkspaceUi::new();
        ui.set_project_name(Some("Crawler".to_owned()));
        ui.set_session_cwds(
            HashMap::from([(session, "/projects/colon35/Design".to_owned())]),
            crate::config::SessionNameStyle::Folder,
        );
        let snapshot = SessionProjectNameSnapshot::try_new(
            1,
            vec![(
                session,
                "/projects/colon35/Design".to_owned(),
                "Design".to_owned(),
            )],
        )
        .unwrap();
        ui.set_session_project_names(snapshot);

        assert_eq!(
            ui.resolve_session_title("workspace.spawn.shell 1", Some(session), None, &catalog),
            "Design (Crawler)",
            "세션 제목이 다른 워크스페이스 이름과 같은 단어로만 보이면 세션이 섞였다고 오해한다"
        );
        assert_eq!(
            ui.session_project_context(Some(session)),
            Some("Design (Crawler)".to_owned()),
            "에이전트 행 1행(headline)에 쓰이는 project_context도 같은 규칙을 따라야 한다"
        );
    }

    /// 워크스페이스 루트 그대로 작업 중이라 cwd 프로젝트명이 워크스페이스 자체 이름과
    /// 같은, 가장 흔한 경우는 지금처럼 프로젝트명만 보여준다 — 정보 중복이 없다.
    #[test]
    fn cwd_project_name이_워크스페이스_자체_이름과_같으면_그대로_보여준다() {
        let catalog = i18n::Catalog::load("en-US").unwrap();
        let session = SessionId(21);
        let mut ui = WorkspaceUi::new();
        ui.set_project_name(Some("Crawler".to_owned()));
        ui.set_session_cwds(
            HashMap::from([(session, "/projects/Crawler".to_owned())]),
            crate::config::SessionNameStyle::Folder,
        );
        let snapshot = SessionProjectNameSnapshot::try_new(
            1,
            vec![(
                session,
                "/projects/Crawler".to_owned(),
                "Crawler".to_owned(),
            )],
        )
        .unwrap();
        ui.set_session_project_names(snapshot);

        assert_eq!(
            ui.resolve_session_title("workspace.spawn.shell 1", Some(session), None, &catalog),
            "Crawler"
        );
        assert_eq!(
            ui.session_project_context(Some(session)),
            Some("Crawler".to_owned())
        );
    }

    #[test]
    fn project_name_snapshot은_bounded이고_clone과_same_revision_setter는_arc만_공유한다() {
        let session = SessionId(1);
        let snapshot = SessionProjectNameSnapshot::try_new(
            1,
            vec![(session, "/workspace".to_owned(), "repo".to_owned())],
        )
        .unwrap();
        let clone = snapshot.clone();
        assert!(Arc::ptr_eq(&snapshot.entries, &clone.entries));

        let mut ui = WorkspaceUi::new();
        ui.set_session_cwds(
            HashMap::from([(session, "/workspace".to_owned())]),
            crate::config::SessionNameStyle::Folder,
        );
        ui.set_session_project_names(snapshot);
        let retained = Arc::clone(&ui.session_project_names.entries);
        ui.set_session_project_names(clone);
        assert!(Arc::ptr_eq(&retained, &ui.session_project_names.entries));

        let too_many = (0..=SESSION_PROJECT_NAME_MAX_ITEMS)
            .map(|index| {
                (
                    SessionId(index as u64 + 1),
                    format!("/workspace/{index}"),
                    format!("repo-{index}"),
                )
            })
            .collect();
        assert_eq!(
            SessionProjectNameSnapshot::try_new(2, too_many).unwrap_err(),
            SessionProjectNameSnapshotError::TooManyItems
        );
        assert_eq!(
            SessionProjectNameSnapshot::try_new(
                2,
                vec![
                    (session, "/workspace/a".to_owned(), "a".to_owned()),
                    (session, "/workspace/b".to_owned(), "b".to_owned()),
                ],
            )
            .unwrap_err(),
            SessionProjectNameSnapshotError::DuplicateSession
        );
    }

    #[test]
    fn protocol_payload_caps_accept_exact_and_reject_plus_one() {
        let exact_scrollback = RuntimeCommand::SpawnShell {
            cols: 80,
            rows: 24,
            scrollback_lines: WORKSPACE_PROTOCOL_SCROLLBACK_MAX_LINES,
        };
        assert_eq!(
            workspace_protocol_command_is_valid(&exact_scrollback),
            Ok(())
        );
        let too_much_scrollback = RuntimeCommand::SpawnShell {
            cols: 80,
            rows: 24,
            scrollback_lines: WORKSPACE_PROTOCOL_SCROLLBACK_MAX_LINES + 1,
        };
        assert_eq!(
            workspace_protocol_command_is_valid(&too_much_scrollback),
            Err(WorkspaceProtocolErrorCode::InvalidCommand)
        );

        let exact_input = RuntimeCommand::WriteInput {
            session: SessionId(1),
            bytes: vec![b'x'; WORKSPACE_PROTOCOL_INPUT_MAX_BYTES],
        };
        assert_eq!(workspace_protocol_command_is_valid(&exact_input), Ok(()));
        let too_large_input = RuntimeCommand::WriteInput {
            session: SessionId(1),
            bytes: vec![b'x'; WORKSPACE_PROTOCOL_INPUT_MAX_BYTES + 1],
        };
        assert_eq!(
            workspace_protocol_command_is_valid(&too_large_input),
            Err(WorkspaceProtocolErrorCode::PayloadTooLarge)
        );

        let exact_query = RuntimeCommand::SearchScrollback {
            session: SessionId(1),
            query: "q".repeat(WORKSPACE_PROTOCOL_QUERY_MAX_BYTES),
            max_matches: SEARCH_MAX_MATCHES,
        };
        assert_eq!(workspace_protocol_command_is_valid(&exact_query), Ok(()));
        let too_large_query = RuntimeCommand::SearchScrollback {
            session: SessionId(1),
            query: "q".repeat(WORKSPACE_PROTOCOL_QUERY_MAX_BYTES + 1),
            max_matches: SEARCH_MAX_MATCHES,
        };
        assert_eq!(
            workspace_protocol_command_is_valid(&too_large_query),
            Err(WorkspaceProtocolErrorCode::PayloadTooLarge)
        );
        let too_many_matches = RuntimeCommand::SearchScrollback {
            session: SessionId(1),
            query: "q".to_owned(),
            max_matches: SEARCH_MAX_MATCHES + 1,
        };
        assert_eq!(
            workspace_protocol_command_is_valid(&too_many_matches),
            Err(WorkspaceProtocolErrorCode::InvalidCommand)
        );

        let exact_path = RuntimeCommand::ResizeSplit {
            tab: MuxTabId("t".repeat(WORKSPACE_PROTOCOL_ID_MAX_BYTES)),
            path: vec![0; WORKSPACE_PROTOCOL_SPLIT_PATH_MAX_ITEMS],
            ratio: 0.5,
        };
        assert_eq!(workspace_protocol_command_is_valid(&exact_path), Ok(()));
        let root_path = RuntimeCommand::ResizeSplit {
            tab: MuxTabId("t".to_owned()),
            path: Vec::new(),
            ratio: 0.5,
        };
        assert_eq!(
            workspace_protocol_command_is_valid(&root_path),
            Ok(()),
            "빈 path는 최상위 split divider를 가리킨다"
        );
        let too_deep_path = RuntimeCommand::ResizeSplit {
            tab: MuxTabId("t".to_owned()),
            path: vec![0; WORKSPACE_PROTOCOL_SPLIT_PATH_MAX_ITEMS + 1],
            ratio: 0.5,
        };
        assert_eq!(
            workspace_protocol_command_is_valid(&too_deep_path),
            Err(WorkspaceProtocolErrorCode::InvalidCommand)
        );
    }

    #[test]
    fn protocol_queue_and_inflight_share_the_exact_eight_slot_cap() {
        let mut ui = WorkspaceUi::new();
        for delta in 1..=WORKSPACE_PROTOCOL_CAP {
            assert_eq!(
                ui.queue_protocol_intent(RuntimeCommand::Scroll {
                    session: SessionId(delta as u64),
                    delta: 1,
                }),
                Ok(())
            );
        }
        assert_eq!(ui.protocol_intents.len(), WORKSPACE_PROTOCOL_CAP);

        let first = ui.take_protocol_intent().expect("first intent");
        let operation = first.operation();
        let generation = first.generation();
        assert_eq!(ui.protocol_intents.len(), WORKSPACE_PROTOCOL_CAP - 1);
        assert_eq!(ui.protocol_inflight.len(), 1);
        assert_eq!(
            ui.queue_protocol_intent(RuntimeCommand::Scroll {
                session: SessionId(99),
                delta: 1,
            }),
            Err(WorkspaceProtocolErrorCode::Busy)
        );

        ui.complete_protocol(WorkspaceProtocolCompletion {
            operation,
            generation: generation.wrapping_add(1),
            result: Err(WorkspaceProtocolErrorCode::DeliveryFailed),
        });
        assert_eq!(
            ui.protocol_inflight.len(),
            1,
            "stale completion must not release"
        );
        assert_eq!(
            ui.queue_protocol_intent(RuntimeCommand::Scroll {
                session: SessionId(100),
                delta: 1,
            }),
            Err(WorkspaceProtocolErrorCode::Busy)
        );

        ui.complete_protocol(WorkspaceProtocolCompletion {
            operation,
            generation,
            result: Err(WorkspaceProtocolErrorCode::DeliveryFailed),
        });
        assert!(ui.protocol_inflight.is_empty());
        assert!(
            ui.queue_protocol_intent(RuntimeCommand::Scroll {
                session: SessionId(101),
                delta: 1,
            })
            .is_ok()
        );

        ui.complete_protocol(WorkspaceProtocolCompletion {
            operation,
            generation,
            result: Ok(()),
        });
        assert_eq!(ui.protocol_intents.len(), WORKSPACE_PROTOCOL_CAP);
        assert_eq!(
            ui.queue_protocol_intent(RuntimeCommand::Scroll {
                session: SessionId(102),
                delta: 1,
            }),
            Err(WorkspaceProtocolErrorCode::Busy),
            "duplicate completion must not release another slot"
        );

        let success = ui.take_protocol_intent().expect("success intent");
        let success_operation = success.operation();
        let success_generation = success.generation();
        ui.complete_protocol(WorkspaceProtocolCompletion {
            operation: success_operation,
            generation: success_generation,
            result: Ok(()),
        });
        assert!(ui.protocol_inflight.is_empty());
        assert!(
            ui.queue_protocol_intent(RuntimeCommand::Scroll {
                session: SessionId(103),
                delta: 1,
            })
            .is_ok()
        );
    }

    #[test]
    fn adjacent_terminal_input_coalesces_without_exceeding_one_mib() {
        let mut ui = WorkspaceUi::new();
        let session = SessionId(7);
        assert!(
            ui.queue_protocol_intent(RuntimeCommand::WriteInput {
                session,
                bytes: vec![b'a'; WORKSPACE_PROTOCOL_INPUT_MAX_BYTES - 1],
            })
            .is_ok()
        );
        assert!(
            ui.queue_protocol_intent(RuntimeCommand::WriteInput {
                session,
                bytes: vec![b'b'],
            })
            .is_ok()
        );
        assert_eq!(ui.protocol_intents.len(), 1);
        assert_eq!(
            ui.queue_protocol_intent(RuntimeCommand::WriteInput {
                session,
                bytes: vec![b'c']
            }),
            Ok(())
        );
        assert_eq!(ui.protocol_intents.len(), 2);
        let commands = drain_protocol(&mut ui);
        assert!(matches!(
            &commands[0],
            RuntimeCommand::WriteInput { bytes, .. }
                if bytes.len() == WORKSPACE_PROTOCOL_INPUT_MAX_BYTES
                    && bytes.last() == Some(&b'b')
        ));
        assert!(matches!(&commands[1], RuntimeCommand::WriteInput { bytes, .. } if bytes == b"c"));
    }

    /// 창 드래그 재현 — 세션의 첫 크기는 지연 없이 즉시 나가지만(세션 생성/split과 동일
    /// 취급), 그 뒤로 목표가 프레임마다 계속 바뀌는 동안(=드래그 진행 중)은 어떤 중간
    /// 크기도 PTY에 전송되면 안 된다(전송되면 매 중간 크기마다 alacritty가 reflow하며
    /// 화면이 깜빡인다). 목표가 안정된 뒤 debounce가 지나야 그 최종 크기 하나만 더
    /// 전송된다.
    #[test]
    fn 리사이즈_드래그_중_중간_크기는_보내지_않고_안정된_최종크기만_한번_보낸다() {
        let mut ui = WorkspaceUi::new();
        let session = SessionId(11);
        let ctx = egui::Context::default();

        // pane이 처음 나타날 때의 크기 — 첫 mismatch는 지연 없이 바로 나간다.
        ui.queue_terminal_resize_debounced(&ctx, session, 80, 24);
        assert_eq!(ui.sent_sizes.get(&session), Some(&(80, 24)));
        drain_protocol(&mut ui);
        let token = ui.sessions[&session].resize_request.as_ref().unwrap().token;
        let stamp = runtime::ResizeStamp {
            epoch: 1,
            owner_epoch: token.owner_epoch,
            token: Some(token),
            cols: 80,
            rows: 24,
        };
        ui.sessions
            .get_mut(&session)
            .unwrap()
            .observe_resize_applied(stamp, std::time::Instant::now());

        // 드래그 시작 — 프레임마다 다른 목표. 직전 전송값(80,24)과 달라 mismatch지만,
        // 이미 한 번 보낸 뒤이므로 여기서부터는 debounce가 걸려야 한다(전송 안 됨).
        for (cols, rows) in [(100u16, 30u16), (101, 30), (105, 32), (110, 33)] {
            ui.queue_terminal_resize_debounced(&ctx, session, cols, rows);
            assert_eq!(
                ui.sent_sizes.get(&session),
                Some(&(80, 24)),
                "드래그가 안정되기 전에는 새 크기가 전송되면 안 된다"
            );
            assert!(
                ui.protocol_intents.is_empty(),
                "중간 크기가 큐에 들어가면 안 된다"
            );
        }

        // 드래그 종료 — 마지막 목표(110, 33)로 안정된다. debounce가 지나기 전엔 여전히 보류.
        ui.queue_terminal_resize_debounced(&ctx, session, 110, 33);
        assert_eq!(ui.sent_sizes.get(&session), Some(&(80, 24)));

        std::thread::sleep(RESIZE_DRAG_DEBOUNCE + std::time::Duration::from_millis(30));
        // 실제 앱에서는 request_repaint_after가 예약한 repaint가 이 시점에 App::ui()를
        // 다시 불러 이 경로를 재실행시킨다 — 여기서는 그 프레임을 직접 흉내낸다.
        ui.queue_terminal_resize_debounced(&ctx, session, 110, 33);

        assert_eq!(
            ui.sent_sizes.get(&session),
            Some(&(110, 33)),
            "드래그가 끝나면 최종 크기가 반드시 전달돼야 한다"
        );
        assert!(matches!(
            &drain_protocol(&mut ui)[0],
            RuntimeCommand::ResizeTracked { session: s, cols: 110, rows: 33, .. } if *s == session
        ));
    }

    /// 창 테두리를 **사람이 실제로 끄는 속도**로 드래그하면 같은 cols/rows가 한 프레임
    /// 이상 유지되다가 다음 칸으로 넘어간다. 그 "머무는" 프레임에서 보류가 지워지면
    /// 다음 칸이 「첫 mismatch」로 오인돼 즉시 전송되고, 그리드 한 칸마다 PTY가 reflow하며
    /// 자식이 SIGWINCH로 화면을 전부 다시 그린다 — 그것이 창 리사이즈 깜빡임이다
    /// (2026-09-06 사용자 보고). 첫 크기를 이미 보낸 세션의 그 뒤 변경은 목표가 안정될
    /// 때까지 단 한 번도 전송되면 안 된다.
    #[test]
    fn 느린_창_드래그는_칸마다_머물러도_중간_크기를_보내지_않는다() {
        let mut ui = WorkspaceUi::new();
        let session = SessionId(14);
        let ctx = egui::Context::default();

        // 세션이 처음 나타날 때의 크기 — 지연 없이 즉시 나간다.
        ui.queue_terminal_resize_debounced(&ctx, session, 80, 24);
        assert_eq!(ui.sent_sizes.get(&session), Some(&(80, 24)));
        drain_protocol(&mut ui);
        let token = ui.sessions[&session].resize_request.as_ref().unwrap().token;
        let stamp = runtime::ResizeStamp {
            epoch: 1,
            owner_epoch: token.owner_epoch,
            token: Some(token),
            cols: 80,
            rows: 24,
        };
        ui.sessions
            .get_mut(&session)
            .unwrap()
            .observe_resize_applied(stamp, std::time::Instant::now());
        // 창이 그 크기에 머무는 평범한 프레임 — 최상단 가드가 보류를 지운다. 드래그를
        // 시작하기 전의 앱은 항상 이 상태다.
        ui.queue_terminal_resize_debounced(&ctx, session, 80, 24);

        // 느린 드래그 — 한 칸 바뀌고, 그 크기로 프레임이 몇 번 더 지나가고, 또 한 칸.
        for cols in [81u16, 82, 83] {
            for _ in 0..3 {
                ui.queue_terminal_resize_debounced(&ctx, session, cols, 24);
            }
            assert_eq!(
                ui.sent_sizes.get(&session),
                Some(&(80, 24)),
                "느린 드래그의 중간 크기 {cols}가 전송됐다"
            );
            assert!(
                ui.protocol_intents.is_empty(),
                "느린 드래그의 중간 크기 {cols}가 큐에 들어갔다"
            );
        }

        // 드래그가 멈추면 최종 크기 하나만 전달된다.
        std::thread::sleep(RESIZE_DRAG_DEBOUNCE + std::time::Duration::from_millis(30));
        ui.queue_terminal_resize_debounced(&ctx, session, 83, 24);
        assert_eq!(ui.sent_sizes.get(&session), Some(&(83, 24)));
        assert!(matches!(
            &drain_protocol(&mut ui)[0],
            RuntimeCommand::ResizeTracked { session: s, cols: 83, rows: 24, .. } if *s == session
        ));
    }

    /// 창 폭이 계속 바뀌어도 문자 격자로 반올림한 cols/rows는 한동안 같을 수 있다.
    /// 이 구간을 드래그 종료로 오인하면 debounce 만료 직후 중간 Resize가 나간다.
    #[test]
    fn 같은_격자_안에서_viewport가_움직이면_리사이즈를_확정하지_않는다() {
        let mut ui = WorkspaceUi::new();
        let session = SessionId(15);
        let ctx = egui::Context::default();
        ui.sent_sizes.insert(session, (80, 24));

        let run_resize = |ui: &mut WorkspaceUi, viewport_width: f32| {
            let mut input = egui::RawInput::default();
            input
                .viewports
                .get_mut(&egui::ViewportId::ROOT)
                .expect("root viewport")
                .inner_rect = Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(viewport_width, 600.0),
            ));
            ctx.run_ui(input, |viewport_ui| {
                ui.queue_terminal_resize_debounced(viewport_ui.ctx(), session, 81, 24);
            })
            .drop_without_applying_deltas();
        };

        run_resize(&mut ui, 800.0);
        ui.pending_resize_target.get_mut(&session).unwrap().3 = std::time::Instant::now()
            .checked_sub(RESIZE_DRAG_DEBOUNCE + std::time::Duration::from_millis(1))
            .unwrap();
        run_resize(&mut ui, 801.0);

        assert_eq!(ui.sent_sizes.get(&session), Some(&(80, 24)));
        assert!(
            ui.protocol_intents.is_empty(),
            "같은 문자 격자 안의 viewport 이동을 드래그 종료로 오인했다"
        );
    }

    /// 세션이 막 생기거나 split 직후처럼 크기가 한 번만 바뀌는 경우(드래그가 아님)는
    /// 지연 없이 즉시 전송돼야 한다 — 모든 resize에 디바운스 지연을 강제하지 않는다.
    #[test]
    fn 세션_생성같은_단일_리사이즈는_지연_없이_즉시_전송된다() {
        let mut ui = WorkspaceUi::new();
        let session = SessionId(12);
        let ctx = egui::Context::default();

        ui.queue_terminal_resize_debounced(&ctx, session, 80, 24);

        assert_eq!(ui.sent_sizes.get(&session), Some(&(80, 24)));
        assert!(matches!(
            &drain_protocol(&mut ui)[0],
            RuntimeCommand::ResizeTracked { session: s, cols: 80, rows: 24, .. } if *s == session
        ));
    }

    /// 디바운스 만료 시점에 프로토콜 큐가 가득 차 전송이 삼켜지면 **보류를 지우면 안 된다**.
    /// 지우면 다음 프레임이 None 분기로 떨어져 즉시 재전송하고, 그 실패가 다시 디바운스
    /// 분기로 와서 repaint 예약을 무한히 갱신한다 — 큐가 계속 막혀 있는 동안 앱이 영영
    /// 유휴로 못 내려간다. 보류가 남아 있으면 elapsed가 만료 상태로 고정돼 repaint를
    /// 예약하지 않고, 자연히 오는 프레임에서만 재시도한다.
    #[test]
    fn 큐가_막혀_전송이_삼켜지면_보류를_남겨_repaint_루프를_만들지_않는다() {
        let mut ui = WorkspaceUi::new();
        let session = SessionId(13);
        let ctx = egui::Context::default();

        // 첫 크기는 즉시 나간다.
        ui.queue_terminal_resize_debounced(&ctx, session, 80, 24);
        assert_eq!(ui.sent_sizes.get(&session), Some(&(80, 24)));
        drain_protocol(&mut ui);
        let token = ui.sessions[&session].resize_request.as_ref().unwrap().token;
        let stamp = runtime::ResizeStamp {
            epoch: 1,
            owner_epoch: token.owner_epoch,
            token: Some(token),
            cols: 80,
            rows: 24,
        };
        ui.sessions
            .get_mut(&session)
            .unwrap()
            .observe_resize_applied(stamp, std::time::Instant::now());

        // 목표가 바뀐다 — 여기서부터 디바운스가 걸린다(전송 안 됨).
        ui.queue_terminal_resize_debounced(&ctx, session, 100, 30);
        assert_eq!(ui.sent_sizes.get(&session), Some(&(80, 24)));

        // 디바운스가 만료되기 전에 큐를 가득 채운다.
        for delta in 1..=WORKSPACE_PROTOCOL_CAP {
            assert_eq!(
                ui.queue_protocol_intent(RuntimeCommand::Scroll {
                    session: SessionId(1000 + delta as u64),
                    delta: 1,
                }),
                Ok(())
            );
        }
        std::thread::sleep(RESIZE_DRAG_DEBOUNCE + std::time::Duration::from_millis(30));

        // 만료 후 전송 시도 — 큐가 가득 차 삼켜진다.
        ui.queue_terminal_resize_debounced(&ctx, session, 100, 30);
        assert_eq!(
            ui.sent_sizes.get(&session),
            Some(&(80, 24)),
            "큐가 가득 차면 전송되지 않는다"
        );
        assert_eq!(
            ui.pending_resize_target
                .get(&session)
                .map(|(cols, rows, _, _)| (*cols, *rows)),
            Some((100, 30)),
            "전송이 삼켜졌으면 보류가 남아야 한다"
        );

        // 큐가 풀리면 자연히 오는 다음 프레임에서 최종 크기가 전달된다.
        drain_protocol(&mut ui);
        ui.queue_terminal_resize_debounced(&ctx, session, 100, 30);
        assert_eq!(ui.sent_sizes.get(&session), Some(&(100, 30)));
        assert!(
            !ui.pending_resize_target.contains_key(&session),
            "전송이 성사되면 보류가 사라진다"
        );
    }

    #[test]
    fn pending_resize_for_same_session_is_latest_only() {
        let mut ui = WorkspaceUi::new();
        let session = SessionId(7);

        ui.queue_protocol_intent(RuntimeCommand::Resize {
            session,
            cols: 80,
            rows: 24,
        })
        .unwrap();
        ui.queue_protocol_intent(RuntimeCommand::Resize {
            session,
            cols: 120,
            rows: 40,
        })
        .unwrap();

        assert_eq!(ui.protocol_intents.len(), 1);
        assert!(matches!(
            &drain_protocol(&mut ui)[0],
            RuntimeCommand::Resize {
                session: queued_session,
                cols: 120,
                rows: 40,
                ..
            } if *queued_session == session
        ));
    }

    #[test]
    fn automatic_resize_retries_silently_after_queue_pressure() {
        let mut ui = WorkspaceUi::new();
        let session = SessionId(99);
        for index in 0..WORKSPACE_PROTOCOL_CAP {
            ui.queue_protocol_intent(RuntimeCommand::Scroll {
                session: SessionId(index as u64 + 1),
                delta: 1,
            })
            .unwrap();
        }

        ui.queue_terminal_resize(session, 120, 40);

        assert_eq!(ui.sent_sizes.get(&session), None);
        assert_eq!(ui.error, None);
        assert_eq!(ui.protocol_intents.len(), WORKSPACE_PROTOCOL_CAP);

        let completed = ui.take_protocol_intent().expect("one queued command");
        ui.complete_protocol(WorkspaceProtocolCompletion {
            operation: completed.operation(),
            generation: completed.generation(),
            result: Ok(()),
        });
        ui.queue_terminal_resize(session, 120, 40);

        assert_eq!(ui.sent_sizes.get(&session), Some(&(120, 40)));
        assert!(ui.protocol_intents.iter().any(|intent| matches!(
            &intent.command,
            RuntimeCommand::ResizeTracked {
                session: queued_session,
                cols: 120,
                rows: 40,
                ..
            } if *queued_session == session
        )));
    }

    /// 기본 8칸의 포화는 terminal pressure reserve에 보관한다. 자연히 풀릴
    /// 백프레셔에는 배너를 띄우지 않고 새 gesture를 원래 FIFO 끝에 유지한다.
    #[test]
    fn send_keep_selection이_큐_포화만으로는_배너를_띄우지_않는다() {
        let mut ui = WorkspaceUi::new();
        for index in 0..WORKSPACE_PROTOCOL_CAP {
            ui.queue_protocol_intent(RuntimeCommand::Scroll {
                session: SessionId(index as u64 + 1),
                delta: 1,
            })
            .unwrap();
        }

        let paste = "원래 붙여넣기\nexact bytes".as_bytes().to_vec();
        let delivered = ui.send_keep_selection(RuntimeCommand::WriteInput {
            session: SessionId(99),
            bytes: paste.clone(),
        });

        assert!(
            delivered,
            "bounded pressure reserve must retain the original paste gesture"
        );
        assert_eq!(ui.error, None, "큐 포화는 배너를 띄우지 않아야 한다");
        assert!(!ui.protocol_request_lost);
        let commands = drain_protocol(&mut ui);
        assert_eq!(commands.len(), WORKSPACE_PROTOCOL_CAP + 1);
        assert!(matches!(commands.last(), Some(RuntimeCommand::WriteInput {
            session: SessionId(99), bytes,
        }) if bytes == &paste));
    }

    /// spawn_shell_at도 동일 원칙 — spawn 상한 포화는 배너 없이 조용히 거부된다.
    #[test]
    fn spawn_shell_at이_큐_포화만으로는_배너를_띄우지_않는다() {
        let mut ui = WorkspaceUi::new();
        for index in 0..WORKSPACE_PROTOCOL_CAP {
            ui.spawn_shell_at(1_000, Some(format!("/tmp/deppy-{index}")));
        }
        assert_eq!(ui.error, None, "정상 spawn 8개는 배너를 띄우면 안 된다");

        ui.spawn_shell_at(1_000, Some("/tmp/deppy-overflow".to_owned()));

        assert_eq!(ui.error, None, "spawn 상한 포화도 배너를 띄우면 안 된다");
        assert!(!ui.protocol_request_lost);
    }

    /// 회귀 — complete_protocol의 Err(Busy)/Err(DeliveryFailed)는 대부분 stale/종료
    /// 레이스라 배너를 띄우면 "이유 모를 배너가 가끔 뜬다"는 원래 버그를 재현한다.
    /// 진단은 tracing으로만 남긴다("terminal protocol delivery failed" 버그).
    #[test]
    fn complete_protocol의_busy와_delivery_failed는_배너를_띄우지_않는다() {
        let mut ui = WorkspaceUi::new();
        ui.queue_protocol_intent(RuntimeCommand::Scroll {
            session: SessionId(1),
            delta: 1,
        })
        .unwrap();
        let intent = ui.take_protocol_intent().unwrap();
        ui.complete_protocol(WorkspaceProtocolCompletion {
            operation: intent.operation(),
            generation: intent.generation(),
            result: Err(WorkspaceProtocolErrorCode::Busy),
        });
        assert_eq!(ui.error, None);

        ui.queue_protocol_intent(RuntimeCommand::Scroll {
            session: SessionId(2),
            delta: 1,
        })
        .unwrap();
        let intent = ui.take_protocol_intent().unwrap();
        ui.complete_protocol(WorkspaceProtocolCompletion {
            operation: intent.operation(),
            generation: intent.generation(),
            result: Err(WorkspaceProtocolErrorCode::DeliveryFailed),
        });
        assert_eq!(ui.error, None);
        assert!(!ui.protocol_request_lost);
    }

    /// 진짜 유실(PayloadTooLarge)만 protocol_request_lost 플래그를 세운다 — send_keep_selection
    /// 은 catalog가 없어 문구를 못 만들므로 플래그만 세우고 show_with_input이 렌더 시점에
    /// catalog로 채운다. 이 키는 5개 로케일 모두에서 실제 한국어/현지어 문구로 존재해야 한다.
    #[test]
    fn send_keep_selection의_payload_too_large는_유실_플래그를_세운다() {
        let mut ui = WorkspaceUi::new();

        let delivered = ui.send_keep_selection(RuntimeCommand::WriteInput {
            session: SessionId(1),
            bytes: vec![b'x'; WORKSPACE_PROTOCOL_INPUT_MAX_BYTES + 1],
        });

        assert!(!delivered);
        assert!(ui.protocol_request_lost, "실제 유실은 플래그로 남아야 한다");

        let ko = i18n::Catalog::load("ko-KR").unwrap();
        let message = ko.t("workspace.protocol_request_lost", &[]);
        assert_ne!(
            message, "workspace.protocol_request_lost",
            "키가 아니라 실제 문구여야 한다"
        );
        assert!(
            !message.is_ascii(),
            "한국어 배너여야 한다 (영어 원문 그대로 노출 금지): {message}"
        );
    }

    #[test]
    fn protocol_debug_redacts_terminal_input_and_search_query() {
        let mut ui = WorkspaceUi::new();
        ui.queue_protocol_intent(RuntimeCommand::WriteInput {
            session: SessionId(1),
            bytes: b"private terminal input".to_vec(),
        })
        .unwrap();
        let input = ui.take_protocol_intent().unwrap();
        let debug = format!("{input:?}");
        assert!(debug.contains("write_input"));
        assert!(!debug.contains("private terminal input"));
    }

    #[test]
    fn spawn_count_changes_only_after_exact_successful_completion() {
        let mut ui = WorkspaceUi::new();
        ui.spawn_shell(1_000);
        let intent = ui.take_protocol_intent().unwrap();
        let operation = intent.operation();
        let generation = intent.generation();
        assert_eq!(ui.pending_spawns(), 0);
        ui.complete_protocol(WorkspaceProtocolCompletion {
            operation,
            generation: generation.wrapping_add(1),
            result: Ok(()),
        });
        assert_eq!(ui.pending_spawns(), 0);
        ui.complete_protocol(WorkspaceProtocolCompletion {
            operation,
            generation,
            result: Ok(()),
        });
        assert_eq!(ui.pending_spawns(), 1);
        ui.complete_protocol(WorkspaceProtocolCompletion {
            operation,
            generation,
            result: Ok(()),
        });
        assert_eq!(ui.pending_spawns(), 1);
    }

    #[test]
    fn warm_spawn_completion_success_resolves_pending_cd_once() {
        let mut ui = WorkspaceUi::new();
        ui.spawn_shell_at(1_000, Some("/tmp/deppy-spawn".to_owned()));
        assert!(matches!(
            drain_protocol(&mut ui).as_slice(),
            [RuntimeCommand::SpawnShell { .. }]
        ));
        assert_eq!(ui.pending_spawns(), 1);

        ui.apply_warm_events(
            &[RuntimeEvent::ShellSpawned {
                session: SessionId(7),
            }],
            &i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap(),
        );

        assert_eq!(ui.pending_spawns(), 0);
        assert!(ui.pending_spawn_cwds.is_empty());
        assert!(matches!(
            drain_protocol(&mut ui).as_slice(),
            [RuntimeCommand::WriteInput {
                session: SessionId(7),
                ..
            }]
        ));
    }

    #[test]
    fn warm_spawn_completion_failure_clears_pending_cd() {
        let mut ui = WorkspaceUi::new();
        ui.spawn_shell_at(1_000, Some("/tmp/deppy-spawn".to_owned()));
        assert!(matches!(
            drain_protocol(&mut ui).as_slice(),
            [RuntimeCommand::SpawnShell { .. }]
        ));
        assert_eq!(ui.pending_spawns(), 1);

        ui.apply_warm_events(
            &[RuntimeEvent::SpawnFailed {
                kind: SpawnKind::Shell,
                message: runtime::MessagePayload::new("shell.failed"),
            }],
            &i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap(),
        );

        assert_eq!(ui.pending_spawns(), 0);
        assert!(ui.pending_spawn_cwds.is_empty());
        assert!(drain_protocol(&mut ui).is_empty());
    }

    #[test]
    fn warm_unrelated_spawn_failure_preserves_later_pending_cd() {
        let mut ui = WorkspaceUi::new();
        ui.spawn_shell(1_000);
        ui.spawn_shell_at(1_000, Some("/tmp/deppy-spawn".to_owned()));
        assert_eq!(drain_protocol(&mut ui).len(), 2);
        assert_eq!(ui.pending_spawns(), 2);

        ui.apply_warm_events(
            &[RuntimeEvent::SpawnFailed {
                kind: SpawnKind::Shell,
                message: runtime::MessagePayload::new("shell.failed"),
            }],
            &i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap(),
        );
        assert_eq!(ui.pending_spawns(), 1);
        assert!(matches!(
            ui.pending_spawn_cwds.front(),
            Some(PendingShellSpawn::Awaiting { cwd: Some(cwd) })
                if cwd == "/tmp/deppy-spawn"
        ));

        ui.apply_warm_events(
            &[RuntimeEvent::ShellSpawned {
                session: SessionId(7),
            }],
            &i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap(),
        );
        assert!(matches!(
            drain_protocol(&mut ui).as_slice(),
            [RuntimeCommand::WriteInput {
                session: SessionId(7),
                ..
            }]
        ));
    }

    #[test]
    fn warm_multiple_cwd_spawns_preserve_order_across_normal_failure() {
        let mut ui = WorkspaceUi::new();
        ui.spawn_shell_at(1_000, Some("/tmp/deppy-first".to_owned()));
        ui.spawn_shell(1_000);
        ui.spawn_shell_at(1_000, Some("/tmp/deppy-second".to_owned()));
        assert_eq!(drain_protocol(&mut ui).len(), 3);
        assert_eq!(ui.pending_spawns(), 3);

        ui.apply_warm_events(
            &[
                RuntimeEvent::ShellSpawned {
                    session: SessionId(7),
                },
                RuntimeEvent::SpawnFailed {
                    kind: SpawnKind::Shell,
                    message: runtime::MessagePayload::new("shell.failed"),
                },
                RuntimeEvent::ShellSpawned {
                    session: SessionId(9),
                },
            ],
            &i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap(),
        );

        assert_eq!(ui.pending_spawns(), 0);
        let commands = drain_protocol(&mut ui);
        assert_eq!(commands.len(), 2);
        assert!(matches!(
            &commands[0],
            RuntimeCommand::WriteInput { session, bytes }
                if *session == SessionId(7)
                    && String::from_utf8_lossy(bytes).contains("/tmp/deppy-first")
        ));
        assert!(matches!(
            &commands[1],
            RuntimeCommand::WriteInput { session, bytes }
                if *session == SessionId(9)
                    && String::from_utf8_lossy(bytes).contains("/tmp/deppy-second")
        ));
    }

    #[test]
    fn unresolved_spawn_tracking_is_bounded_by_protocol_cap() {
        let mut ui = WorkspaceUi::new();
        for index in 0..WORKSPACE_PROTOCOL_CAP {
            ui.spawn_shell_at(1_000, Some(format!("/tmp/deppy-{index}")));
            assert_eq!(drain_protocol(&mut ui).len(), 1);
        }
        assert_eq!(ui.pending_spawns(), WORKSPACE_PROTOCOL_CAP as u32);

        ui.spawn_shell_at(1_000, Some("/tmp/deppy-overflow".to_owned()));

        assert!(drain_protocol(&mut ui).is_empty());
        assert_eq!(ui.pending_spawns(), WORKSPACE_PROTOCOL_CAP as u32);
    }

    #[test]
    fn saturated_protocol_retries_spawn_cd_once_after_capacity_frees() {
        let mut ui = WorkspaceUi::new();
        ui.spawn_shell_at(1_000, Some("/tmp/deppy-retry".to_owned()));
        assert_eq!(drain_protocol(&mut ui).len(), 1);
        for index in 0..WORKSPACE_PROTOCOL_CAP {
            ui.queue_protocol_intent(RuntimeCommand::Scroll {
                session: SessionId(index as u64 + 20),
                delta: 1,
            })
            .unwrap();
        }

        ui.apply_warm_events(
            &[RuntimeEvent::ShellSpawned {
                session: SessionId(7),
            }],
            &i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap(),
        );

        let first = ui.take_protocol_intent().unwrap();
        let first_operation = first.operation();
        let first_generation = first.generation();
        assert!(matches!(
            first.into_command(),
            RuntimeCommand::Scroll { .. }
        ));
        ui.complete_protocol(WorkspaceProtocolCompletion {
            operation: first_operation,
            generation: first_generation,
            result: Ok(()),
        });

        let commands = drain_protocol(&mut ui);
        let cwd_writes: Vec<_> = commands
            .iter()
            .filter(|command| {
                matches!(
                    command,
                    RuntimeCommand::WriteInput { session, bytes }
                        if *session == SessionId(7)
                            && String::from_utf8_lossy(bytes).contains("/tmp/deppy-retry")
                )
            })
            .collect();
        assert_eq!(cwd_writes.len(), 1);
        assert!(drain_protocol(&mut ui).is_empty());
    }

    /// 「마지막 출력 복사」만 남고 제거된 agent 전송 항목은 다시 나타나지 않는다.
    #[test]
    fn kittest_마지막_출력_메뉴가_추출을_요청한다() {
        use egui_kittest::kittest::Queryable;
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let session = SessionId(7);
        let mut ws = WorkspaceUi::new();
        ws.mux = Some(mux(
            "a",
            vec![tab(
                "a",
                vec![pane("pa", session)],
                LayoutNode::Pane(pane_id("pa")),
            )],
            "pa",
        ));
        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, ws: &mut WorkspaceUi| {
                ws.last_output_menu_items(ui, SessionId(7), &catalog);
            },
            ws,
        );
        harness.run();
        assert!(
            harness
                .query_by_label("Send last output to agent")
                .is_none(),
            "제거된 last-output agent 메뉴가 다시 나타나면 안 된다"
        );
        harness
            .get_by_label(&catalog.t("workspace.menu.copy_last_output", &[]))
            .click();
        harness.run();
        assert!(
            harness
                .state()
                .last_output_copy_pending
                .contains(&SessionId(7))
        );
        assert!(drain_protocol(harness.state_mut()).iter().any(|command| {
            matches!(
                command,
                RuntimeCommand::ExtractLastOutput { session } if *session == SessionId(7)
            )
        }));
    }

    #[test]
    fn kittest_세션_폴더_메뉴가_트리이동과_finder_요청을_쌓는다() {
        use egui_kittest::kittest::Queryable;
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, ws: &mut WorkspaceUi| {
                ws.session_folder_menu_items(ui, SessionId(7), &catalog);
            },
            WorkspaceUi::new(),
        );
        harness.run();
        harness
            .get_by_label(&catalog.t("workspace.menu.reveal_in_tree", &[]))
            .click();
        harness.run();
        assert_eq!(
            harness.state_mut().take_session_folder_request(),
            Some(SessionFolderRequest::RevealInTree(SessionId(7))),
            "트리 이동 요청이 쌓여야 한다"
        );
        assert_eq!(
            harness.state_mut().take_session_folder_request(),
            None,
            "take는 1회 소비다"
        );
        harness
            .get_by_label(&catalog.t("sidebar.menu.open_folder", &[]))
            .click();
        harness.run();
        assert_eq!(
            harness.state_mut().take_session_folder_request(),
            Some(SessionFolderRequest::OpenInFinder(SessionId(7))),
            "Finder 요청이 쌓여야 한다"
        );
    }

    /// kittest 재현 — 최소 pane(50px) 헤더에서 닫기(×) 자리를 클릭하면 분할이
    /// 아니라 닫기 확인이 떠야 한다. compact 헤더(3e3e909)에서는 강제 표시된
    /// Split 버튼이 닫기 히트박스를 덮고 interact가 나중 등록이라 SplitPane이
    /// 나갔다 (codex 리뷰 P2).
    #[test]
    fn kittest_좁은_pane_닫기_클릭은_분할이_아니라_닫기를_요청한다() {
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let config = TerminalConfig::default();
        let mut ws = WorkspaceUi::new();
        ws.mux = Some(mux(
            "t",
            vec![tab(
                "t",
                vec![pane("p", SessionId(7))],
                LayoutNode::Pane(pane_id("p")),
            )],
            "p",
        ));
        let header = egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(TERMINAL_PANE_MIN_SIZE, TERMINAL_PANE_HEADER_HEIGHT),
        );
        let snapshot = pane("p", SessionId(7));
        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, ws: &mut WorkspaceUi| {
                ws.render_pane_header(ui, header, &snapshot, true, &config, &catalog, true);
            },
            ws,
        );
        harness.run();
        // 닫기 중심 x의 가능 범위는 clamp상 [25, 43.9] → 어떤 제목 폭에서도 닫기
        // 히트박스(20px)에 들어가는 공통 구간은 x ∈ [33.9, 35). 34.5를 클릭한다.
        let hit = egui::pos2(34.5, TERMINAL_PANE_HEADER_HEIGHT * 0.5);
        harness.hover_at(hit);
        harness.run();
        harness.drag_at(hit);
        harness.run();
        harness.drop_at(hit);
        harness.run();
        assert_eq!(
            harness.state().confirm_close,
            Some(pane_id("p")),
            "닫기 자리 클릭은 닫기 확인을 띄워야 한다"
        );
        assert!(
            !drain_protocol(harness.state_mut())
                .iter()
                .any(|command| matches!(command, RuntimeCommand::SplitPane { .. })),
            "닫기 자리 클릭이 분할을 실행하면 안 된다"
        );
    }

    #[test]
    fn path_insert_paste_bytes_required_fixtures는_bracketed와_no_enter를_지킨다() {
        use crate::ui::file_tree::ShellKind;

        let fixtures = [
            "src/main.rs",
            "プロジェクト/設定ファイル.rs",
            "项目/配置文件.rs",
            "專案/設定檔.rs",
            "프로젝트/설정파일.rs",
            "project/🚀-deploy/config.json",
        ];

        for fixture in fixtures {
            let path = Path::new(fixture);
            for shell in [
                ShellKind::Posix,
                ShellKind::Fish,
                ShellKind::PowerShell,
                ShellKind::Cmd,
            ] {
                let raw = crate::ui::file_tree::shell_path_insert_bytes_for(path, shell);
                assert_eq!(
                    path_insert_paste_bytes(path, shell, false),
                    raw,
                    "{fixture} {shell:?}"
                );
                assert_eq!(raw.last(), Some(&b' '), "{fixture} {shell:?}");
                assert!(!raw.contains(&b'\n'), "{fixture} {shell:?}");
                assert!(!raw.contains(&b'\r'), "{fixture} {shell:?}");

                let wrapped = path_insert_paste_bytes(path, shell, true);
                assert!(wrapped.starts_with(b"\x1b[200~"), "{fixture} {shell:?}");
                assert!(wrapped.ends_with(b"\x1b[201~"), "{fixture} {shell:?}");
                let inner = &wrapped[b"\x1b[200~".len()..wrapped.len() - b"\x1b[201~".len()];
                assert_eq!(inner, raw.as_slice(), "{fixture} {shell:?}");
                assert_eq!(inner.last(), Some(&b' '), "{fixture} {shell:?}");
                assert!(!inner.contains(&b'\n'), "{fixture} {shell:?}");
                assert!(!inner.contains(&b'\r'), "{fixture} {shell:?}");
            }
        }
    }

    #[test]
    fn paths_insert_paste_bytes는_클립보드_file_list를_다중_path로_삽입한다() {
        use crate::ui::file_tree::ShellKind;

        let paths = vec![
            std::path::PathBuf::from("images/sample image.png"),
            std::path::PathBuf::from("프로젝트/🚀-deploy/설정 파일.png"),
        ];
        for shell in [
            ShellKind::Posix,
            ShellKind::Fish,
            ShellKind::PowerShell,
            ShellKind::Cmd,
        ] {
            let mut expected = Vec::new();
            for path in &paths {
                expected.extend(crate::ui::file_tree::shell_path_insert_bytes_for(
                    path, shell,
                ));
            }
            assert_eq!(
                paths_insert_paste_bytes(&paths, shell, false),
                expected,
                "{shell:?}"
            );
            assert_eq!(expected.last(), Some(&b' '), "{shell:?}");
            assert!(!expected.contains(&b'\n'), "{shell:?}");
            assert!(!expected.contains(&b'\r'), "{shell:?}");

            let wrapped = paths_insert_paste_bytes(&paths, shell, true);
            assert!(wrapped.starts_with(b"\x1b[200~"), "{shell:?}");
            assert!(wrapped.ends_with(b"\x1b[201~"), "{shell:?}");
            let inner = &wrapped[b"\x1b[200~".len()..wrapped.len() - b"\x1b[201~".len()];
            assert_eq!(inner, expected.as_slice(), "{shell:?}");
        }
    }

    #[test]
    fn clipboard_terminal_paste는_file_list를_text_flavor보다_우선한다() {
        use crate::ui::file_tree::ShellKind;

        let paths = vec![std::path::PathBuf::from("images/sample image.png")];
        let text = Some(b"images/sample image.png".to_vec());

        assert_eq!(
            clipboard_terminal_paste_bytes(Some(&paths), || text, ShellKind::Posix, false),
            Some(paths_insert_paste_bytes(&paths, ShellKind::Posix, false))
        );
    }

    #[test]
    fn clipboard_terminal_paste는_file_list가_없으면_text_paste로_fallback한다() {
        use crate::ui::file_tree::ShellKind;

        let text = input_mapper::paste_bytes("plain text".as_bytes(), true);

        assert_eq!(
            clipboard_terminal_paste_bytes(None, || Some(text.clone()), ShellKind::Posix, true),
            Some(text)
        );
    }

    #[cfg(unix)]
    #[test]
    fn workspace_raw_path_caps_use_encoded_os_bytes() {
        use std::os::unix::ffi::OsStringExt as _;

        let mut raw = vec![b'x'; WORKSPACE_PATH_MAX_BYTES];
        *raw.last_mut().unwrap() = 0xff;
        let path = PathBuf::from(std::ffi::OsString::from_vec(raw));
        let expected = path.as_os_str().as_encoded_bytes().len();

        let payload = WorkspacePathPayload::try_new(path.clone()).unwrap();
        assert_eq!(payload.bytes, expected);
        let clipboard = TerminalClipboardPayload::try_new(vec![path], None).unwrap();
        assert_eq!(clipboard.path_bytes, expected);
    }

    #[test]
    fn native_io_상한과_만료가_붙여넣기_컨텍스트를_해제한다() {
        let mut ui = WorkspaceUi::new();
        let catalog = i18n::Catalog::load("ko-KR").unwrap();
        ui.resolve_path_cached(SessionId(77), "src/main.rs");
        for _ in 0..=WORKSPACE_IO_QUEUE_CAP {
            ui.request_terminal_clipboard(
                SessionId(77),
                false,
                crate::ui::file_tree::ShellKind::Posix,
                None,
            );
        }
        assert_eq!(ui.pending_pastes.len(), WORKSPACE_IO_QUEUE_CAP);
        assert_eq!(ui.io_intents.len(), WORKSPACE_IO_QUEUE_CAP + 1);
        assert_eq!(
            take_error_message(&mut ui, &catalog),
            Some(catalog.t("workspace.native_busy", &[]))
        );
        assert!(take_error_message(&mut ui, &catalog).is_none());
        for pending in &mut ui.pending_pastes {
            pending.requested_at =
                std::time::Instant::now() - PASTE_TASK_TTL - std::time::Duration::from_secs(1);
        }
        assert!(matches!(
            ui.take_io_intent(),
            Some(WorkspaceIoIntent::ResolvePath { .. })
        ));
        assert!(ui.pending_pastes.is_empty());
        assert!(ui.io_intents.is_empty());
        assert_eq!(
            take_error_message(&mut ui, &catalog),
            Some(catalog.t("workspace.clipboard_expired", &[]))
        );
        ui.error = Some("기존 작업 오류".to_owned());
        ui.protocol_request_lost = true;
        assert_eq!(
            take_error_message(&mut ui, &catalog),
            Some(catalog.t("workspace.protocol_request_lost", &[]))
        );
        assert_eq!(
            take_error_message(&mut ui, &catalog).as_deref(),
            Some("기존 작업 오류")
        );
        assert!(take_error_message(&mut ui, &catalog).is_none());
    }

    #[test]
    fn native_io_경로_미리보기가_붙여넣기를_막지_않는다() {
        let mut ui = WorkspaceUi::new();
        let session = SessionId(77);
        ui.resolve_path_cached(session, "src/main.rs");
        ui.request_terminal_clipboard(session, false, crate::ui::file_tree::ShellKind::Posix, None);
        assert!(matches!(
            ui.take_io_intent(),
            Some(WorkspaceIoIntent::ReadTerminalClipboard { .. })
        ));
        assert!(ui.error.is_none());
        assert!(matches!(
            ui.take_io_intent(),
            Some(WorkspaceIoIntent::ResolvePath { .. })
        ));
    }

    #[test]
    fn native_io_파일_클립보드는_텍스트_읽기와_변환을_생략한다() {
        use crate::ui::file_tree::ShellKind;

        let paths = vec![PathBuf::from("/tmp/clipboard image.png")];
        let payload = TerminalClipboardPayload::read_with(paths.clone(), || {
            panic!("파일이 있으면 OS 텍스트를 읽으면 안 된다")
        })
        .unwrap();
        let (returned_paths, text) = payload.into_parts();
        assert_eq!(returned_paths, paths);
        assert!(text.is_none());
        assert_eq!(
            clipboard_terminal_paste_bytes(
                Some(&returned_paths),
                || panic!("파일이 있으면 텍스트 버퍼를 만들면 안 된다"),
                ShellKind::Posix,
                true,
            ),
            Some(paths_insert_paste_bytes(&paths, ShellKind::Posix, true))
        );

        let payload =
            TerminalClipboardPayload::read_with(Vec::new(), || Some("한글 text".to_owned()))
                .unwrap();
        let (paths, text) = payload.into_parts();
        assert!(paths.is_empty());
        assert_eq!(
            clipboard_terminal_paste_bytes(
                None,
                || text.map(|value| terminal_text_paste_bytes(&value, true)),
                ShellKind::Posix,
                true,
            ),
            Some(terminal_text_paste_bytes("한글 text", true))
        );
    }

    #[test]
    fn native_io_연속_붙여넣기는_각_요청의_세션을_보존한다() {
        let mut ui = WorkspaceUi::new();
        let mut requests = Vec::new();
        for session in [SessionId(77), SessionId(78)] {
            ui.request_terminal_clipboard(
                session,
                false,
                crate::ui::file_tree::ShellKind::Posix,
                None,
            );
            let Some(WorkspaceIoIntent::ReadTerminalClipboard {
                operation,
                generation,
            }) = ui.take_io_intent()
            else {
                panic!("붙여넣기 요청이 사라졌다");
            };
            requests.push((operation, generation));
        }
        // cwd 갱신은 경로 조회만 무효화해야 하며 붙여넣기 결과는 버리면 안 된다.
        ui.invalidate_path_resolution();
        for (operation, generation) in requests {
            ui.complete_io(WorkspaceIoCompletion::TerminalClipboardRead {
                operation,
                generation,
                result: TerminalClipboardPayload::try_new(Vec::new(), Some("text".to_owned())),
            });
        }
        let commands = drain_protocol(&mut ui);
        assert_eq!(commands.len(), 2);
        for (command, expected) in commands.iter().zip([SessionId(77), SessionId(78)]) {
            assert!(
                matches!(command, RuntimeCommand::WriteInput { session, bytes }
                if *session == expected && bytes == b"text")
            );
        }
    }

    #[test]
    fn native_io_열기_실패는_원인에_맞는_알림으로_전달한다() {
        let catalog = i18n::Catalog::load("ko-KR").unwrap();

        let mut busy = WorkspaceUi::new();
        for n in 0..WORKSPACE_IO_QUEUE_CAP {
            busy.request_open_url(&format!("https://example.com/{n}"));
        }
        busy.request_open_url("https://example.com/overflow");
        assert_eq!(
            take_error_message(&mut busy, &catalog),
            Some(catalog.t("workspace.native_busy", &[]))
        );

        let mut rejected = WorkspaceUi::new();
        rejected.request_open_url("file:///tmp/not-allowed");
        assert_eq!(
            take_error_message(&mut rejected, &catalog),
            Some(catalog.t("workspace.url_rejected", &[]))
        );

        let mut completed = WorkspaceUi::new();
        completed.complete_io(WorkspaceIoCompletion::OpenUrlFailed);
        assert_eq!(
            take_error_message(&mut completed, &catalog),
            Some(catalog.t("workspace.url_rejected", &[]))
        );
        completed.complete_io(WorkspaceIoCompletion::OpenPathFailed);
        assert_eq!(
            take_error_message(&mut completed, &catalog),
            Some(catalog.t("workspace.path_rejected", &[]))
        );

        let mut clipboard = WorkspaceUi::new();
        clipboard.request_terminal_clipboard(
            SessionId(77),
            false,
            crate::ui::file_tree::ShellKind::Posix,
            None,
        );
        let Some(WorkspaceIoIntent::ReadTerminalClipboard {
            operation,
            generation,
        }) = clipboard.take_io_intent()
        else {
            panic!("clipboard intent가 있어야 한다");
        };
        clipboard.complete_io(WorkspaceIoCompletion::TerminalClipboardRead {
            operation,
            generation,
            result: Err(WorkspaceIoErrorCode::ClipboardTooLarge),
        });
        assert_eq!(
            take_error_message(&mut clipboard, &catalog),
            Some(catalog.t("workspace.clipboard_too_large", &[]))
        );
    }

    #[test]
    fn native_io_입력압박은_한_에피소드에_알림_한번만_만든다() {
        let catalog = i18n::Catalog::load("ko-KR").unwrap();
        let session = SessionId(77);
        let snapshot = mux(
            "a",
            vec![tab(
                "a",
                vec![pane("pa", session)],
                LayoutNode::Pane(pane_id("pa")),
            )],
            "pa",
        );
        let mut ui = WorkspaceUi::new();
        ui.handle_events(&[RuntimeEvent::MuxUpdated { snapshot }], &catalog);
        let pressure = |queued_bytes: usize, queued_messages: usize| runtime::PtyInputPressure {
            attempted_bytes: queued_bytes.saturating_add(1),
            queued_bytes,
            queued_messages,
            max_bytes: 1024,
            max_messages: 8,
            reason: runtime::PtyInputRejectReason::QueueFull,
        };

        ui.handle_events(
            &[RuntimeEvent::PtyInputPressure {
                session,
                pressure: pressure(128, 1),
            }],
            &catalog,
        );
        let first = ui.take_error_notice(&catalog).expect("첫 압박 알림");
        assert_eq!(first.kind, WorkspaceErrorKind::InputPressure);

        ui.handle_events(
            &[RuntimeEvent::PtyInputPressure {
                session,
                pressure: pressure(256, 2),
            }],
            &catalog,
        );
        assert!(ui.take_error_notice(&catalog).is_none());

        ui.handle_events(
            &[RuntimeEvent::PtyInputPressure {
                session,
                pressure: pressure(0, 0),
            }],
            &catalog,
        );
        ui.handle_events(
            &[RuntimeEvent::PtyInputPressure {
                session,
                pressure: pressure(64, 1),
            }],
            &catalog,
        );
        assert_eq!(
            ui.take_error_notice(&catalog).map(|notice| notice.kind),
            Some(WorkspaceErrorKind::InputPressure)
        );
    }

    #[test]
    fn terminal_clipboard_completion은_요청_세션에_정확히_한번만_전송된다() {
        use crate::ui::file_tree::ShellKind;

        let mut ui = WorkspaceUi::new();
        let session = SessionId(77);
        let paths = vec![std::path::PathBuf::from("/tmp/clipboard image.png")];
        ui.request_terminal_clipboard(session, true, ShellKind::Posix, None);
        let intent = ui.take_io_intent().expect("clipboard intent");
        let (operation, generation) = match intent {
            WorkspaceIoIntent::ReadTerminalClipboard {
                operation,
                generation,
            } => (operation, generation),
            other => panic!("unexpected intent: {other:?}"),
        };
        ui.complete_io(WorkspaceIoCompletion::TerminalClipboardRead {
            operation,
            generation: generation.wrapping_add(1),
            result: TerminalClipboardPayload::try_new(paths.clone(), None),
        });
        assert!(drain_protocol(&mut ui).is_empty());
        ui.complete_io(WorkspaceIoCompletion::TerminalClipboardRead {
            operation,
            generation,
            result: TerminalClipboardPayload::try_new(paths.clone(), None),
        });
        ui.complete_io(WorkspaceIoCompletion::TerminalClipboardRead {
            operation,
            generation,
            result: TerminalClipboardPayload::try_new(paths.clone(), None),
        });

        let commands = drain_protocol(&mut ui);
        assert_eq!(commands.len(), 1);
        assert!(matches!(
            &commands[0],
            RuntimeCommand::WriteInput {
                session: target,
                bytes,
            } if *target == session
                && *bytes == paths_insert_paste_bytes(&paths, ShellKind::Posix, true)
        ));
    }

    #[test]
    fn generated_pane_titles_render_through_catalog_and_legacy_korean_titles() {
        let catalog = i18n::Catalog::load("en-US").unwrap();
        assert_eq!(
            display_pane_title("workspace.spawn.shell 3", &catalog),
            "Shell 3"
        );
        assert_eq!(
            display_pane_title("workspace.spawn.agent 4", &catalog),
            "Agent 4"
        );
        assert_eq!(display_pane_title("셸 5", &catalog), "Shell 5");
        assert_eq!(display_pane_title("custom title", &catalog), "custom title");
    }

    #[test]
    fn terminal_text_dnd_paste는_raw_text를_보존한다() {
        let fixtures = [
            "src/main.rs",
            "プロジェクト/設定ファイル.rs",
            "项目/配置文件.rs",
            "專案/設定檔.rs",
            "프로젝트/설정파일.rs",
            "project/🚀-deploy/config.json",
            "line one\nline two",
        ];

        for fixture in fixtures {
            assert_eq!(
                terminal_text_paste_bytes(fixture, false),
                fixture.as_bytes(),
                "{fixture}"
            );
            let wrapped = terminal_text_paste_bytes(fixture, true);
            assert!(wrapped.starts_with(b"\x1b[200~"), "{fixture}");
            assert!(wrapped.ends_with(b"\x1b[201~"), "{fixture}");
            let inner = &wrapped[b"\x1b[200~".len()..wrapped.len() - b"\x1b[201~".len()];
            assert_eq!(inner, fixture.as_bytes(), "{fixture}");
        }
    }

    #[test]
    fn 터미널_선택_공백정리_복사는_화면행과_line_continuation을_한줄로_합친다() {
        let selected = r#"! curl -s
https://example.test/login \
        -c /tmp/cookies.txt \
        --data-urlencode "method=login" --data-urlencode "tenant=502"

\
        --data-urlencode "id=user" --data-urlencode
"pw=secret  value!""#;

        assert_eq!(
            clean_terminal_selection_for_copy(selected),
            "! curl -s https://example.test/login -c /tmp/cookies.txt --data-urlencode \"method=login\" --data-urlencode \"tenant=502\" --data-urlencode \"id=user\" --data-urlencode \"pw=secret  value!\""
        );
        assert_eq!(
            clean_terminal_selection_for_copy("  echo   'a  b'  "),
            "echo   'a  b'",
            "행 내부와 인용 값의 공백은 보존해야 한다"
        );
        assert_eq!(
            clean_terminal_selection_for_copy("printf '\\\\'\\\\\nnext"),
            "printf '\\\\'\\\\ next",
            "짝수 trailing backslash는 line-continuation이 아니므로 보존해야 한다"
        );
    }

    #[test]
    fn terminal_text_dnd는_선택범위_내부에서만_시작한다() {
        assert!(selection_range_contains(3, 7, 3));
        assert!(selection_range_contains(3, 7, 5));
        assert!(selection_range_contains(3, 7, 7));
        assert!(!selection_range_contains(3, 7, 2));
        assert!(!selection_range_contains(3, 7, 8));
    }

    #[test]
    fn 드래그_오토스크롤_속도는_초과거리_비례_부호는_scroll_delta_규약() {
        // pane 세로 범위 [100, 500], 셀 높이 16
        // 경계 안 → 0
        assert_eq!(drag_autoscroll_rate(300.0, 100.0, 500.0, 16.0), 0.0);
        assert_eq!(drag_autoscroll_rate(100.0, 100.0, 500.0, 16.0), 0.0);
        assert_eq!(drag_autoscroll_rate(500.0, 100.0, 500.0, 16.0), 0.0);
        // 위로 벗어남 → 양수(과거로), 1셀 초과 = 8행/초
        assert_eq!(drag_autoscroll_rate(84.0, 100.0, 500.0, 16.0), 8.0);
        // 아래로 벗어남 → 음수(최신으로), 조금 벗어나면 느리게
        assert_eq!(drag_autoscroll_rate(504.0, 100.0, 500.0, 16.0), -2.0);
        // 많이 벗어나면 빠르게, 최대 60행/초로 clamp
        assert_eq!(drag_autoscroll_rate(2000.0, 100.0, 500.0, 16.0), -60.0);
        assert_eq!(drag_autoscroll_rate(-2000.0, 100.0, 500.0, 16.0), 60.0);
    }

    /// 휠은 평상시 선택을 해제하지만(화면 freeze 때문), **드래그 중에는 보존**해야
    /// 한 화면을 넘는 범위를 이어 잡을 수 있다(2026-08-18 사용자 요청).
    #[test]
    fn 휠은_드래그_중에만_선택을_보존한다() {
        // 드래그 중이고 그 세션의 선택이 살아 있을 때만 보존한다. 조건이 앵커를 보정하는
        // `dragged()` 분기와 **같아야** 스크롤과 선택이 어긋나지 않는다.
        assert!(
            wheel_scroll_keeps_selection(true, true),
            "드래그 중이면 보존"
        );
        // 버튼을 뗀 뒤의 휠은 기존대로 해제한다 — 안 그러면 선택이 남아 화면이 멈춘 듯 보인다.
        assert!(
            !wheel_scroll_keeps_selection(false, true),
            "드래그가 아니면 해제"
        );
        // 선택이 없으면 보존할 것도 없다(다른 세션의 선택이어도 마찬가지).
        assert!(
            !wheel_scroll_keeps_selection(true, false),
            "그 세션 선택이 없으면 해제"
        );
        assert!(!wheel_scroll_keeps_selection(false, false));
    }

    #[test]
    fn 드래그_오토스크롤_앵커는_스크롤량만큼_이동하고_화면_경계에서_clamp() {
        // 10열 × 5행, 앵커 = 2행 3열(idx 23)
        // 과거로 1행(내용 아래로 이동) → 앵커도 1행 아래
        assert_eq!(shift_selection_cell(23, 1, 10, 5), 33);
        // 최신으로 2행 → 앵커 2행 위
        assert_eq!(shift_selection_cell(23, -2, 10, 5), 3);
        // 위로 화면 밖 → 첫 행 첫 열
        assert_eq!(shift_selection_cell(23, -5, 10, 5), 0);
        // 아래로 화면 밖 → 마지막 행 마지막 열
        assert_eq!(shift_selection_cell(23, 5, 10, 5), 49);
    }

    #[test]
    fn image_paste_shortcut은_platform_clipboard_paste만_잡는다() {
        let ctrl_v = egui::Event::Key {
            key: egui::Key::V,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::CTRL,
        };
        let ctrl_shift_v = egui::Event::Key {
            key: egui::Key::V,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::CTRL | egui::Modifiers::SHIFT,
        };
        let cmd_v = egui::Event::Key {
            key: egui::Key::V,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers {
                mac_cmd: true,
                command: true,
                ..egui::Modifiers::NONE
            },
        };

        // 이미지-only clipboard의 Cmd+V PRESS는 egui가 소비한다. 이 함수는 native
        // key-down 감시가 없을 때를 위한 release fallback만 잡는다.
        let cmd_v_release = egui::Event::Key {
            key: egui::Key::V,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers: egui::Modifiers {
                mac_cmd: true,
                command: true,
                ..egui::Modifiers::NONE
            },
        };
        if cfg!(target_os = "macos") {
            assert!(is_clipboard_paste_shortcut(&cmd_v_release));
            assert!(!is_clipboard_paste_shortcut(&cmd_v)); // press는 AppKit monitor가 담당
            assert!(!is_clipboard_paste_shortcut(&ctrl_v));
            assert!(!is_clipboard_paste_shortcut(&ctrl_shift_v));
        } else {
            assert!(!is_clipboard_paste_shortcut(&cmd_v));
            assert!(!is_clipboard_paste_shortcut(&ctrl_v));
            assert!(is_clipboard_paste_shortcut(&ctrl_shift_v));
        }
    }

    #[test]
    fn terminal_keyboard는_textedit_popup_window가_없을때만_활성이다() {
        assert!(terminal_keyboard_input_allowed(false, false, false, false));
        assert!(!terminal_keyboard_input_allowed(true, false, false, false));
        assert!(!terminal_keyboard_input_allowed(false, true, false, false));
        assert!(!terminal_keyboard_input_allowed(false, false, true, false));
        // 검색 닫힘/터미널 클릭 직후에는 사라진 TextEdit의 stale focus보다 terminal refocus가
        // 우선이라 첫 문장부호·한글 조합이 빠지지 않는다.
        assert!(terminal_keyboard_input_allowed(true, false, false, true));
        assert!(!terminal_keyboard_input_allowed(true, true, false, true));
        assert!(!terminal_keyboard_input_allowed(true, false, true, true));
    }

    #[test]
    fn 진행중_ime는_일시적_비textedit_포커스에서도_이벤트를_계속_받는다() {
        assert!(terminal_accepts_ime_events(
            true, false, true, false, false, false, false
        ));
        assert!(!terminal_accepts_ime_events(
            true, false, true, false, true, false, false
        ));
        assert!(!terminal_accepts_ime_events(
            true, false, true, false, false, true, false
        ));
        assert!(!terminal_accepts_ime_events(
            true, false, true, false, false, false, true
        ));
        assert!(!terminal_accepts_ime_events(
            true, true, true, false, true, false, false
        ));
        assert!(!terminal_accepts_ime_events(
            true, true, true, false, false, true, false
        ));
        assert!(!terminal_accepts_ime_events(
            true, true, true, false, false, false, true
        ));
    }

    /// 조합이 **시작되는** 프레임은 egui 공식 소유권도 직전 프레임 preedit도 없다.
    /// 그 프레임을 거절하면 `self.preedit`이 채워지지 않고, renderer가 뒤이은 입력 없는
    /// 프레임에서 조합이 끝난 줄 알고 포커스를 복구하다 IME를 강제 중단한다(자모 분리).
    #[test]
    fn 조합이_시작되는_프레임은_소유권과_직전_preedit이_없어도_받는다() {
        // 이번 프레임 preedit만 근거인 경우 — 받아야 한다.
        assert!(terminal_accepts_ime_events(
            true, false, false, true, false, false, false
        ));
        // 근거가 하나도 없으면 종전대로 받지 않는다.
        assert!(!terminal_accepts_ime_events(
            true, false, false, false, false, false, false
        ));
        // TextEdit·팝업·모달 배제는 이번 프레임 preedit이 있어도 그대로 우선한다 —
        // 다른 입력창의 조합을 터미널이 가로채면 안 된다.
        assert!(!terminal_accepts_ime_events(
            true, false, false, true, true, false, false
        ));
        assert!(!terminal_accepts_ime_events(
            true, false, false, true, false, true, false
        ));
        assert!(!terminal_accepts_ime_events(
            true, false, false, true, false, false, true
        ));
        // 터미널이 키보드 소유자가 아니면 무조건 거절한다.
        assert!(!terminal_accepts_ime_events(
            false, false, false, true, false, false, false
        ));
    }

    #[test]
    fn 네이티브_command_c는_copy_event가_없거나_늦어도_터미널_선택을_복사한다() {
        assert!(terminal_should_copy_selection(true, &[], true, false, true,));
        assert!(terminal_should_copy_selection(
            false,
            &[egui::Event::Copy],
            true,
            false,
            true,
        ));
        assert!(!terminal_should_copy_selection(true, &[], true, true, true,));
        assert!(!terminal_should_copy_selection(
            true,
            &[],
            false,
            false,
            true,
        ));
    }

    #[test]
    fn 터미널_enter_합성클릭은_hover_path를_활성화하지_않는다() {
        let ctx = egui::Context::default();
        let mut response_id = None;
        ctx.run_ui(egui::RawInput::default(), |ui| {
            let (_, response) =
                ui.allocate_exact_size(egui::vec2(120.0, 40.0), egui::Sense::click_and_drag());
            response.request_focus();
            response_id = Some(response.id);
        })
        .drop_without_applying_deltas();
        assert!(ctx.memory(|memory| memory.has_focus(response_id.unwrap())));

        let mut activation = None;
        let input = egui::RawInput {
            events: vec![egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
            ..egui::RawInput::default()
        };
        ctx.run_ui(input, |ui| {
            let (_, response) =
                ui.allocate_exact_size(egui::vec2(120.0, 40.0), egui::Sense::click_and_drag());
            activation = Some((
                response.clicked(),
                terminal_primary_pointer_clicked(&response),
            ));
        })
        .drop_without_applying_deltas();

        assert_eq!(activation, Some((true, false)));
    }

    #[test]
    fn agents_window는_terminal_refocus를_막는_modal_layer가_아니다() {
        let ctx = egui::Context::default();
        ctx.run_ui(egui::RawInput::default(), |ui| {
            egui::Window::new("Agents")
                .id(crate::ui::agent_sessions::agents_window_id())
                .show(ui.ctx(), |ui| {
                    ui.label("agent content");
                });
        })
        .drop_without_applying_deltas();
        let agents = egui::LayerId::new(
            egui::Order::Middle,
            crate::ui::agent_sessions::agents_window_id(),
        );
        let confirmation = egui::LayerId::new(egui::Order::Middle, egui::Id::new("confirmation"));
        let background = egui::LayerId::background();

        assert!(ctx.memory(|memory| memory.areas().visible_layer_ids().contains(&agents)));
        assert!(!is_blocking_terminal_window(&agents));
        assert!(is_blocking_terminal_window(&confirmation));
        assert!(!is_blocking_terminal_window(&background));
        // diff 리뷰 패널도 같은 규약의 비모달 창이다 (PR-D).
        let diff = egui::LayerId::new(egui::Order::Middle, crate::ui::diff_panel::diff_window_id());
        assert!(!is_blocking_terminal_window(&diff));
    }

    #[test]
    fn pending_focus는_runtime_이전_pane보다_먼저_입력_대상이_된다() {
        let old = pane_id("old");
        let next = pane_id("next");
        assert!(terminal_input_owner(&old, true, None));
        assert!(!terminal_input_owner(&old, true, Some(&next)));
        assert!(terminal_input_owner(&next, false, Some(&next)));
        assert!(terminal_input_owner(&next, true, Some(&next)));
    }

    #[test]
    fn async_restore_runtime_snapshot_does_not_override_the_explicit_input_fence() {
        let old = pane_id("pane-old");
        let requested = pane_id("pane-requested");
        let mut last_runtime_focus = None;
        let mut pending_focus = Some(requested.clone());

        assert!(sync_runtime_focus_intent(
            &mut last_runtime_focus,
            &mut pending_focus,
            Some(&requested),
            Some(old.clone()),
        ));
        assert_eq!(pending_focus, Some(requested.clone()));
        assert!(!terminal_input_owner(&old, true, pending_focus.as_ref()));

        pending_focus = None;
        assert!(!sync_runtime_focus_intent(
            &mut last_runtime_focus,
            &mut pending_focus,
            None,
            Some(old.clone()),
        ));
        assert!(terminal_input_owner(&old, true, pending_focus.as_ref()));
    }

    #[test]
    fn newer_runtime_focus_replaces_an_older_runtime_derived_pending_focus() {
        let old = pane_id("pane-old");
        let next = pane_id("pane-next");
        let mut last_runtime_focus = Some(old.clone());
        let mut pending_focus = Some(old);

        assert!(sync_runtime_focus_intent(
            &mut last_runtime_focus,
            &mut pending_focus,
            None,
            Some(next.clone()),
        ));
        assert_eq!(pending_focus, Some(next));
    }

    #[test]
    fn app_armed_terminal_focus_waits_for_exact_pane_surface() {
        let mut workspace = WorkspaceUi::new();
        let pane = pane_id("pane-exact");

        workspace.arm_terminal_focus(pane.clone());

        assert_eq!(workspace.pending_focus, Some(pane));
        assert_eq!(workspace.explicit_pending_focus, workspace.pending_focus);
        assert!(!workspace.take_terminal_focus_claimed());
    }

    #[test]
    fn app_can_cancel_an_unmaterialized_terminal_focus_intent() {
        let mut workspace = WorkspaceUi::new();
        workspace.arm_terminal_focus(pane_id("pane-stale"));

        workspace.cancel_terminal_focus();

        assert!(workspace.pending_focus.is_none());
        assert!(workspace.explicit_pending_focus.is_none());
    }

    #[test]
    fn explicit_focus_fence_is_released_when_its_observed_pane_disappears() {
        let target = pane_id("pane-target");
        let mut workspace = WorkspaceUi::new();
        workspace.mux = Some(mux(
            "primary",
            vec![tab(
                "primary",
                vec![pane("pane-target", SessionId(7))],
                LayoutNode::Pane(target.clone()),
            )],
            "pane-target",
        ));
        workspace.arm_terminal_focus(target);
        workspace.mux = Some(mux(
            "primary",
            vec![tab(
                "primary",
                vec![pane("survivor", SessionId(8))],
                LayoutNode::Pane(pane_id("survivor")),
            )],
            "survivor",
        ));

        workspace.reconcile_explicit_terminal_focus();

        assert!(workspace.pending_focus.is_none());
        assert!(workspace.explicit_pending_focus.is_none());
    }

    #[test]
    fn unobserved_explicit_focus_fence_stays_fail_closed_until_app_cancels_it() {
        let mut workspace = WorkspaceUi::new();
        workspace.arm_terminal_focus(pane_id("pane-never-acknowledged"));

        workspace.reconcile_explicit_terminal_focus();

        assert_eq!(
            workspace.pending_focus,
            Some(pane_id("pane-never-acknowledged"))
        );
        assert_eq!(workspace.explicit_pending_focus, workspace.pending_focus);
    }

    #[test]
    fn focused_terminal_click_reports_its_exact_local_focus_claim() {
        let catalog = catalog();
        let config = TerminalConfig::default();
        let session = SessionId(7);
        let clicked = pane_id("clicked");
        let mut workspace = WorkspaceUi::new();
        workspace.mux = Some(mux(
            "primary",
            vec![tab(
                "primary",
                vec![pane("clicked", session)],
                LayoutNode::Pane(clicked.clone()),
            )],
            "clicked",
        ));
        workspace.last_focused_pane = Some(clicked.clone());
        workspace.arm_terminal_focus(clicked.clone());
        workspace.sessions.entry(session).or_default().snapshot = Some(snapshot("ready"));
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, state: &mut (WorkspaceUi, Option<runtime::MuxPaneId>)| {
                let frame = state.0.show_with_input(ui, &config, &[], &catalog, true);
                if frame.local_focus_claimed.is_some() {
                    state.1 = frame.local_focus_claimed;
                }
            },
            (workspace, None),
        );
        harness.run();
        let terminal_point = egui::pos2(80.0, TERMINAL_PANE_HEADER_HEIGHT + 40.0);
        harness.input_mut().events.extend([
            egui::Event::PointerMoved(terminal_point),
            egui::Event::PointerButton {
                pos: terminal_point,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
            egui::Event::PointerButton {
                pos: terminal_point,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            },
        ]);
        harness.run();

        assert_eq!(harness.state().1, Some(clicked));
        assert!(drain_protocol(&mut harness.state_mut().0).iter().any(
            |command| matches!(command, RuntimeCommand::FocusPane { pane } if pane == &pane_id("clicked"))
        ));
    }

    #[test]
    fn app_armed_focus_claims_terminal_ownership_when_exact_surface_appears() {
        let session = SessionId(7);
        let mut harness = setup_focused_local_pane_harness(session);
        let target = pane_id("pane");
        harness.state_mut().arm_terminal_focus(target);

        harness.run();

        assert!(harness.state_mut().take_terminal_focus_claimed());
    }

    #[test]
    fn terminal_focus_claim은_한_frame에서_한번만_소비된다() {
        let mut workspace = WorkspaceUi::new();
        workspace.terminal_focus_claimed = true;

        assert!(workspace.take_terminal_focus_claimed());
        assert!(!workspace.take_terminal_focus_claimed());
    }

    /// OS 파일 드롭은 확장자를 분류하거나 PTY에 경로를 쓰지 않고 App에 그대로 넘긴다.
    #[test]
    fn kittest_터미널_pane_위_os_드롭은_모든_경로를_문서_intent로_보낸다() {
        let session = SessionId(7);
        let mut harness = setup_focused_local_pane_drop_harness(session);
        let dropped = [
            PathBuf::from("/x/main.rs"),
            PathBuf::from("/x/config.json"),
            PathBuf::from("/x/settings.toml"),
            PathBuf::from("/x/deploy.yaml"),
        ];
        let pane_point = egui::pos2(80.0, TERMINAL_PANE_HEADER_HEIGHT + 40.0);
        harness
            .input_mut()
            .events
            .push(egui::Event::PointerMoved(pane_point));
        harness.input_mut().dropped_files.extend(
            dropped
                .iter()
                .cloned()
                .map(crate::test_dropped_file::handle),
        );
        harness.run();

        assert_eq!(harness.state().1.document_drop_paths, dropped.to_vec());
        assert!(written_bytes(drain_protocol(&mut harness.state_mut().0)).is_empty());
    }

    /// pane 헤더처럼 터미널 표면 밖에 놓인 OS 드롭은 무시한다 — dropped_files가
    /// 있다는 사실만으로 삽입하면 안 되고, 실제 포인터 위치가 이 pane의 rect 안에
    /// 있을 때만 받아야 한다(2026-08-14).
    #[test]
    fn kittest_pane_밖_os_드롭은_무시된다() {
        let session = SessionId(7);
        let mut harness = setup_focused_local_pane_drop_harness(session);
        // pane 헤더 영역(표면 rect 위) — TERMINAL_PANE_HEADER_HEIGHT보다 작은 y는
        // pane_layout.surface(=pane_rect) 밖이다.
        let header_point = egui::pos2(80.0, 4.0);
        harness
            .input_mut()
            .events
            .push(egui::Event::PointerMoved(header_point));
        harness
            .input_mut()
            .dropped_files
            .push(crate::test_dropped_file::handle(PathBuf::from(
                "/x/dropped.txt",
            )));
        harness.run();

        assert!(harness.state().1.document_drop_paths.is_empty());
        assert!(written_bytes(drain_protocol(&mut harness.state_mut().0)).is_empty());
    }

    /// 분할된 두 pane 중 포인터 밑 pane에만 들어간다 — 한 번의 OS 드롭이 두 목적지로
    /// 새면 안 된다는 요구사항의 워크스페이스 쪽 절반(컴포저/사이드바와의 배타성은
    /// egui Panel 레이아웃이 화면을 서로 겹치지 않게 나누는 데서 나온다).
    #[test]
    fn kittest_분할된_pane_중_포인터_아래_pane에만_os_드롭이_들어간다() {
        let catalog = catalog();
        let config = TerminalConfig::default();
        let left = SessionId(7);
        let right = SessionId(8);
        let mut workspace = WorkspaceUi::new();
        let layout = LayoutNode::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(LayoutNode::Pane(pane_id("left"))),
            second: Box::new(LayoutNode::Pane(pane_id("right"))),
        };
        workspace.mux = Some(mux(
            "primary",
            vec![tab(
                "primary",
                vec![pane("left", left), pane("right", right)],
                layout,
            )],
            "left",
        ));
        workspace.last_focused_pane = Some(pane_id("left"));
        workspace.pending_focus = Some(pane_id("left"));
        workspace.sessions.entry(left).or_default().snapshot = Some(snapshot("left"));
        workspace.sessions.entry(right).or_default().snapshot = Some(snapshot("right"));
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(600.0, 400.0))
            .build_ui_state(
                move |ui, state: &mut (WorkspaceUi, WorkspaceSurfaceOutput)| {
                    let frame = state.0.show_with_input(ui, &config, &[], &catalog, true);
                    state
                        .1
                        .document_drop_paths
                        .extend(frame.document_drop_paths);
                    if frame.local_focus_claimed.is_some() {
                        state.1.local_focus_claimed = frame.local_focus_claimed;
                    }
                },
                (workspace, WorkspaceSurfaceOutput::default()),
            );
        harness.run();
        drain_protocol(&mut harness.state_mut().0);

        // 초기 포커스는 왼쪽이지만 오른쪽 pane 위에 드롭한다.
        let right_point = egui::pos2(500.0, TERMINAL_PANE_HEADER_HEIGHT + 40.0);
        harness
            .input_mut()
            .events
            .push(egui::Event::PointerMoved(right_point));
        harness
            .input_mut()
            .dropped_files
            .push(crate::test_dropped_file::handle(PathBuf::from(
                "/x/right-pane.rs",
            )));
        harness.run();

        assert_eq!(
            harness.state().1.document_drop_paths,
            vec![PathBuf::from("/x/right-pane.rs")]
        );
        assert_eq!(
            harness.state().1.local_focus_claimed,
            Some(pane_id("right"))
        );
        assert!(written_bytes(drain_protocol(&mut harness.state_mut().0)).is_empty());
    }

    #[test]
    #[allow(non_snake_case)]
    fn kittest_파일트리_PathBuf_드롭은_문서_intent이고_pty_write가_아니다() {
        let session = SessionId(7);
        let mut harness = setup_focused_local_pane_drop_harness(session);
        let point = egui::pos2(80.0, TERMINAL_PANE_HEADER_HEIGHT + 40.0);
        harness.hover_at(point);
        harness.drag_at(point);
        harness.run();
        egui::DragAndDrop::set_payload(&harness.ctx, PathBuf::from("/x/lib.rs"));
        harness.event(egui::Event::PointerMoved(point));
        harness.event(egui::Event::PointerButton {
            pos: point,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        });
        harness.run();

        assert_eq!(
            harness.state().1.document_drop_paths,
            vec![PathBuf::from("/x/lib.rs")]
        );
        assert!(written_bytes(drain_protocol(&mut harness.state_mut().0)).is_empty());
    }

    fn printable_key(key: egui::Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    fn reconciled_text_bytes(
        native: &[crate::native_key_monitor::NativePrintableKeyDown],
        events: &[egui::Event],
        preedit_active: bool,
    ) -> Vec<u8> {
        let reconciliation = reconcile_ime_text_events(native, events, preedit_active, false);
        let mut bytes = Vec::new();
        for text in reconciliation.event_text.into_iter().flatten() {
            bytes.extend(text.as_bytes());
        }
        bytes.extend(reconciliation.fallback_bytes);
        bytes.extend(reconciliation.fallback_after_submit_bytes);
        bytes
    }

    #[test]
    fn 한글_조합직후_text없는_문장부호_key는_pty로_보완된다() {
        let events = [
            egui::Event::Ime(egui::ImeEvent::Preedit {
                text: "ㄱ".into(),
                active_range_chars: None,
            }),
            printable_key(egui::Key::Period),
            egui::Event::Ime(egui::ImeEvent::Commit("ㄱ".into())),
        ];
        assert_eq!(reconciled_text_bytes(&[], &events, false), "ㄱ.".as_bytes());
    }

    #[test]
    fn 한글_조합직후_정상_text는_특수문자_대체입력을_취소한다() {
        let events = [
            egui::Event::Ime(egui::ImeEvent::Preedit {
                text: "ㄱ".into(),
                active_range_chars: None,
            }),
            printable_key(egui::Key::Period),
            egui::Event::Text(".".into()),
        ];
        assert_eq!(reconciled_text_bytes(&[], &events, false), b".");
    }

    #[test]
    fn 한글_조합직후_같은_특수문자_연타도_text_수만큼만_중복제거한다() {
        let events = [
            egui::Event::Ime(egui::ImeEvent::Preedit {
                text: "ㄱ".into(),
                active_range_chars: None,
            }),
            printable_key(egui::Key::Period),
            printable_key(egui::Key::Period),
            egui::Event::Text(".".into()),
        ];
        assert_eq!(reconciled_text_bytes(&[], &events, false), b"..");
    }

    #[test]
    fn 빠른_연타에서_누락된_첫_기호는_뒤_text보다_앞에_복구된다() {
        let native = [
            crate::native_key_monitor::NativePrintableKeyDown::for_test('.'),
            crate::native_key_monitor::NativePrintableKeyDown::for_test(','),
        ];
        let events = [
            egui::Event::Ime(egui::ImeEvent::Commit("ㅁ".into())),
            egui::Event::Text(",".into()),
        ];
        assert_eq!(
            reconciled_text_bytes(&native, &events, true),
            "ㅁ.,".as_bytes()
        );
    }

    #[test]
    fn appkit이_보존한_keydown은_winit이_숨긴_마침표를_commit_frame에서_복구한다() {
        let native = [crate::native_key_monitor::NativePrintableKeyDown::for_test(
            '.',
        )];
        let events = [
            egui::Event::Ime(egui::ImeEvent::Preedit {
                text: "ㅁ".into(),
                active_range_chars: None,
            }),
            egui::Event::Ime(egui::ImeEvent::Preedit {
                text: String::new(),
                active_range_chars: None,
            }),
            egui::Event::Ime(egui::ImeEvent::Commit("ㅁ".into())),
        ];
        assert_eq!(
            reconciled_text_bytes(&native, &events, false),
            "ㅁ.".as_bytes()
        );
    }

    #[test]
    fn appkit_keydown은_commit_text와_egui_fallback을_각각_중복하지_않는다() {
        let native_period = [crate::native_key_monitor::NativePrintableKeyDown::for_test(
            '.',
        )];
        let commit_includes_period = [egui::Event::Ime(egui::ImeEvent::Commit("ㅁ.".into()))];
        assert_eq!(
            reconciled_text_bytes(&native_period, &commit_includes_period, true),
            "ㅁ.".as_bytes()
        );

        let commit_omits_period = [egui::Event::Ime(egui::ImeEvent::Commit("ㅁ".into()))];
        assert_eq!(
            reconciled_text_bytes(&native_period, &commit_omits_period, true),
            "ㅁ.".as_bytes()
        );
    }

    #[test]
    fn alacritty_8079의_commit과_text_이중_space는_한칸만_전달된다() {
        let native_space = [crate::native_key_monitor::NativePrintableKeyDown::for_test(
            ' ',
        )];
        let events = [
            egui::Event::Ime(egui::ImeEvent::Preedit {
                text: " ".into(),
                active_range_chars: Some(1..1),
            }),
            egui::Event::Ime(egui::ImeEvent::Preedit {
                text: String::new(),
                active_range_chars: None,
            }),
            egui::Event::Ime(egui::ImeEvent::Commit(" ".into())),
            printable_key(egui::Key::Space),
            egui::Event::Text(" ".into()),
        ];
        assert_eq!(reconciled_text_bytes(&native_space, &events, false), b" ");
    }

    #[test]
    fn space를_두번_누르면_commit_text_중복후에도_정확히_두칸이다() {
        let native_spaces = [
            crate::native_key_monitor::NativePrintableKeyDown::for_test(' '),
            crate::native_key_monitor::NativePrintableKeyDown::for_test(' '),
        ];
        let events = [
            egui::Event::Ime(egui::ImeEvent::Commit(" ".into())),
            printable_key(egui::Key::Space),
            egui::Event::Text(" ".into()),
            printable_key(egui::Key::Space),
            egui::Event::Text(" ".into()),
        ];
        assert_eq!(reconciled_text_bytes(&native_spaces, &events, true), b"  ");
    }

    #[test]
    fn comma_commit과_text가_함께_와도_물리키_한번만_전달된다() {
        let native_comma = [crate::native_key_monitor::NativePrintableKeyDown::for_test(
            ',',
        )];
        let events = [
            egui::Event::Ime(egui::ImeEvent::Commit(",".into())),
            printable_key(egui::Key::Comma),
            egui::Event::Text(",".into()),
        ];
        assert_eq!(reconciled_text_bytes(&native_comma, &events, true), b",");
    }

    #[test]
    fn 한글_commit에_포함된_comma는_뒤따른_text와_중복되지_않는다() {
        let native_comma = [crate::native_key_monitor::NativePrintableKeyDown::for_test(
            ',',
        )];
        let events = [
            egui::Event::Ime(egui::ImeEvent::Commit("한,".into())),
            printable_key(egui::Key::Comma),
            egui::Event::Text(",".into()),
        ];
        assert_eq!(
            reconciled_text_bytes(&native_comma, &events, true),
            "한,".as_bytes()
        );
    }

    #[test]
    fn 일반_text도_appkit_물리키보다_많은_comma는_중복제거한다() {
        let native_comma = [crate::native_key_monitor::NativePrintableKeyDown::for_test(
            ',',
        )];
        let events = [egui::Event::Text(",".into()), egui::Event::Text(",".into())];
        assert_eq!(reconciled_text_bytes(&native_comma, &events, false), b",");
    }

    #[test]
    fn ime가_아닌_batch에서는_누락된_네이티브키를_추측삽입하지_않는다() {
        let native = [
            crate::native_key_monitor::NativePrintableKeyDown::for_test('.'),
            crate::native_key_monitor::NativePrintableKeyDown::for_test(','),
        ];
        let events = [egui::Event::Text(",".into())];
        assert_eq!(reconciled_text_bytes(&native, &events, false), b",");
    }

    #[test]
    fn ime가_관여하지_않은_네이티브_keydown은_누락문자를_별도_주입하지_않는다() {
        let native = [crate::native_key_monitor::NativePrintableKeyDown::for_test(
            '.',
        )];
        assert!(reconciled_text_bytes(&native, &[], false).is_empty());
    }

    // 아래 두 테스트는 `reconcile_ime_text_events` 단독이 아니라 `show_with_input`
    // 전체 경로(self.preedit 갱신 + terminal_accepts_ime_events 게이트 + 원장)를
    // egui_kittest Harness로 실제 프레임을 돌려 검증한다. 기존 IME 테스트는 전부
    // 순수 함수만 호출해 이 배선(wiring) 자체는 한 번도 실행된 적이 없었다 —
    // "빠르게 칠 때만 자모로 분리된다"는 신고를 조사하며 이 격차를 확인하고
    // 메꾼다. 두 테스트 모두 특수문자 없는 순수 한글(가/나)만 써서 문장부호
    // 중복제거 원장(native_key_monitor 기반)이 애초에 관여하지 않는 경로를
    // 검증한다.
    #[test]
    fn popup_behavior_probe_workspace_rename_does_not_paste_into_terminal() {
        use crate::ui::file_tree::{
            FileTreeUi, SidebarAction, SidebarSnapshot, SidebarWorkspaceEntry,
            SidebarWorkspaceState,
        };
        use egui_kittest::kittest::Queryable;
        let text = catalog();
        let config = TerminalConfig::default();
        let workspaces = vec![SidebarWorkspaceEntry {
            id: "ws-inline".into(),
            name: "Serenity".into(),
            state: SidebarWorkspaceState::Active,
            summary: Default::default(),
        }];
        let mut ws = WorkspaceUi::new();
        ws.mux = Some(mux(
            "primary",
            vec![tab(
                "primary",
                vec![pane("pane", SessionId(7))],
                LayoutNode::Pane(pane_id("pane")),
            )],
            "pane",
        ));
        ws.last_focused_pane = Some(pane_id("pane"));
        ws.pending_focus = Some(pane_id("pane"));
        ws.sessions.entry(SessionId(7)).or_default().snapshot = Some(snapshot("ready"));
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1100.0, 720.0))
            .build_ui_state(
                move |ui, state: &mut (FileTreeUi, WorkspaceUi, Vec<SidebarAction>, bool, bool)| {
                    if !state.3 {
                        return;
                    }
                    if std::mem::take(&mut state.4) {
                        crate::native_key_monitor::tests::record_paste();
                    }
                    let snapshot = SidebarSnapshot {
                        active_workspace_id: "ws-inline",
                        workspaces: &workspaces,
                        view: crate::ui::agent_terminal::AgentTerminalView::Terminal,
                        home_notice_count: 0,
                        fleet_summary: Default::default(),
                        history_tab_active: false,
                        git_tab_active: false,
                        agents_open: false,
                        workspace_note: None,
                    };
                    if let Some(action) =
                        state
                            .0
                            .panel(ui, &std::collections::HashMap::new(), &snapshot, &text)
                    {
                        state.2.push(action);
                    }
                    let (paste, copy) = state.0.take_clipboard_shortcut_consumption();
                    state.1.suppress_clipboard_shortcuts_this_frame(paste, copy);
                    state.1.show_with_input(ui, &config, &[], &text, true);
                },
                (
                    FileTreeUi::new(egui::Context::default()),
                    ws,
                    Vec::new(),
                    false,
                    false,
                ),
            );
        let fonts = crate::config::Config::default();
        crate::fonts::install_cjk_fallback(
            &harness.ctx,
            None,
            &fonts.terminal.mono_font,
            &fonts.terminal.mono_weight,
        );
        harness.state_mut().3 = true;
        harness.run();
        drain_protocol(&mut harness.state_mut().1);
        while harness.state_mut().1.take_io_intent().is_some() {}
        harness
            .state_mut()
            .0
            .begin_workspace_rename("ws-inline".into(), "Serenity".into());
        harness.run();
        assert!(harness.ctx.text_edit_focused());
        assert!(
            harness
                .query_by_role(egui::accesskit::Role::TextInput)
                .is_some()
        );
        harness.input_mut().events.extend([
            egui::Event::Paste("-alias".into()),
            egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            },
        ]);
        harness.state_mut().4 = true;
        harness.run_steps(1);
        assert!(harness.state().2.iter().any(|action| matches!(action, SidebarAction::CommitWorkspaceName { workspace_id, name } if workspace_id == "ws-inline" && name == "Serenity-alias")));
        let mut clipboard_request = false;
        while let Some(intent) = harness.state_mut().1.take_io_intent() {
            eprintln!("after inline rename: {intent:?}");
            clipboard_request |= matches!(intent, WorkspaceIoIntent::ReadTerminalClipboard { .. });
        }
        assert!(
            !clipboard_request,
            "inline rename's native paste was sent to the terminal"
        );
        // The discarded native batch must not replay after the one-pass fence expires.
        harness.run_steps(1);
        assert!(harness.state_mut().1.take_io_intent().is_none());
        drain_protocol(&mut harness.state_mut().1);
        harness
            .input_mut()
            .events
            .push(egui::Event::Text("x".into()));
        harness.run_steps(1);
        assert_eq!(
            written_bytes(drain_protocol(&mut harness.state_mut().1)),
            b"x"
        );
    }

    fn setup_focused_local_pane_harness(
        session: SessionId,
    ) -> egui_kittest::Harness<'static, WorkspaceUi> {
        let catalog = catalog();
        let config = TerminalConfig::default();
        let target_pane = pane_id("pane");
        let mut workspace = WorkspaceUi::new();
        workspace.mux = Some(mux(
            "primary",
            vec![tab(
                "primary",
                vec![pane("pane", session)],
                LayoutNode::Pane(target_pane.clone()),
            )],
            "pane",
        ));
        workspace.last_focused_pane = Some(target_pane.clone());
        workspace.pending_focus = Some(target_pane.clone());
        workspace.sessions.entry(session).or_default().snapshot = Some(snapshot("ready"));
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, workspace: &mut WorkspaceUi| {
                workspace.show_with_input(ui, &config, &[], &catalog, true);
            },
            workspace,
        );
        // pending_focus 1회 소비 + egui 공식 IME 소유권 확보를 끝낸 "이미 타이핑
        // 중인" 정상 상태로 만든다 — 포커스 전환 첫 프레임의 특수 경로가 아니라
        // 연속 타이핑 중의 정상 경로를 테스트하기 위함이다.
        harness.run();
        drain_protocol(harness.state_mut());
        harness
    }

    fn setup_textedit_and_local_pane_harness(
        session: SessionId,
    ) -> egui_kittest::Harness<'static, (WorkspaceUi, String)> {
        let catalog = catalog();
        let config = TerminalConfig::default();
        let target = pane_id("pane");
        let mut workspace = WorkspaceUi::new();
        workspace.mux = Some(mux(
            "primary",
            vec![tab(
                "primary",
                vec![pane("pane", session)],
                LayoutNode::Pane(target.clone()),
            )],
            "pane",
        ));
        workspace.last_focused_pane = Some(target.clone());
        workspace.pending_focus = Some(target);
        workspace.sessions.entry(session).or_default().snapshot = Some(snapshot("ready"));
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, state: &mut (WorkspaceUi, String)| {
                ui.add(
                    egui::TextEdit::singleline(&mut state.1).id(egui::Id::new("actual-textedit")),
                );
                state.0.show_with_input(ui, &config, &[], &catalog, true);
            },
            (workspace, String::new()),
        );
        harness.run();
        drain_protocol(&mut harness.state_mut().0);
        harness
    }

    fn setup_focused_local_pane_drop_harness(
        session: SessionId,
    ) -> egui_kittest::Harness<'static, (WorkspaceUi, WorkspaceSurfaceOutput)> {
        let catalog = catalog();
        let config = TerminalConfig::default();
        let target_pane = pane_id("pane");
        let mut workspace = WorkspaceUi::new();
        workspace.mux = Some(mux(
            "primary",
            vec![tab(
                "primary",
                vec![pane("pane", session)],
                LayoutNode::Pane(target_pane.clone()),
            )],
            "pane",
        ));
        workspace.last_focused_pane = Some(target_pane.clone());
        workspace.pending_focus = Some(target_pane);
        workspace.sessions.entry(session).or_default().snapshot = Some(snapshot("ready"));
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, state: &mut (WorkspaceUi, WorkspaceSurfaceOutput)| {
                let frame = state.0.show_with_input(ui, &config, &[], &catalog, true);
                state
                    .1
                    .document_drop_paths
                    .extend(frame.document_drop_paths);
                if frame.local_focus_claimed.is_some() {
                    state.1.local_focus_claimed = frame.local_focus_claimed;
                }
            },
            (workspace, WorkspaceSurfaceOutput::default()),
        );
        harness.run();
        drain_protocol(&mut harness.state_mut().0);
        harness
    }

    fn preedit_event(text: &str) -> egui::Event {
        egui::Event::Ime(egui::ImeEvent::Preedit {
            text: text.to_owned(),
            active_range_chars: None,
        })
    }

    fn commit_event(text: &str) -> egui::Event {
        egui::Event::Ime(egui::ImeEvent::Commit(text.to_owned()))
    }

    fn written_bytes(commands: Vec<RuntimeCommand>) -> Vec<u8> {
        commands
            .into_iter()
            .filter_map(|command| match command {
                RuntimeCommand::WriteInput { bytes, .. } => Some(bytes),
                _ => None,
            })
            .flatten()
            .collect()
    }

    #[test]
    fn pr10_direct_korean_keeps_owned_bytes_when_protocol_queue_is_busy() {
        let mut harness = setup_focused_local_pane_harness(SessionId(7));
        for index in 0..WORKSPACE_PROTOCOL_CAP {
            harness
                .state_mut()
                .queue_protocol_intent(RuntimeCommand::Scroll {
                    session: SessionId(index as u64 + 100),
                    delta: 1,
                })
                .unwrap();
        }
        harness.input_mut().events.extend([
            egui::Event::Text("빠른한글".into()),
            egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            },
        ]);
        harness.run_steps(1);
        let commands = drain_protocol(harness.state_mut());
        assert_eq!(
            written_bytes(commands),
            "빠른한글\r".as_bytes(),
            "known-unsent direct input must survive protocol slot pressure"
        );
    }

    #[test]
    fn pr10_host_busy_completion_keeps_exact_original_input() {
        let mut harness = setup_focused_local_pane_harness(SessionId(7));
        harness
            .input_mut()
            .events
            .push(egui::Event::Text("원래세션".into()));
        harness.run_steps(1);
        let intent = harness
            .state_mut()
            .take_protocol_intent()
            .expect("direct input");
        let operation = intent.operation();
        let generation = intent.generation();
        assert!(matches!(
            intent.command,
            RuntimeCommand::WriteInput {
                session: SessionId(7),
                ..
            }
        ));
        harness.state_mut().return_unsent_terminal_protocol(
            operation,
            generation,
            intent.into_command(),
        );
        assert!(harness.state_mut().take_protocol_intent().is_none());
        harness.state_mut().protocol_retry_at = Some(std::time::Instant::now());
        assert!(
            matches!(
                drain_protocol(harness.state_mut()).as_slice(),
                [RuntimeCommand::WriteInput { session: SessionId(7), bytes }] if bytes == "원래세션".as_bytes()
            ),
            "known-unsent host refusal must retain original bytes and target"
        );
    }

    #[test]
    fn pr10_raw_input_pipeline_before_measurement() {
        let catalog = catalog();
        let config = TerminalConfig::default();
        let mut focused = setup_focused_local_pane_harness(SessionId(7));
        let workspace = std::mem::replace(focused.state_mut(), WorkspaceUi::new());
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, state: &mut (WorkspaceUi, Vec<RuntimeCommand>)| {
                // Actual eframe order and current App boundary: logic drains before UI.
                state.1.extend(drain_protocol(&mut state.0));
                state.0.show_with_input(ui, &config, &[], &catalog, true);
            },
            (workspace, Vec::new()),
        );
        harness.run();
        harness.state_mut().1.clear();
        harness.state_mut().0.pending_focus = Some(pane_id("pane"));
        harness.run();
        harness.state_mut().1.clear();
        let started = std::time::Instant::now();
        harness
            .input_mut()
            .events
            .push(egui::Event::Text("direct".into()));
        harness.run_steps(1);
        let first_pass = written_bytes(std::mem::take(&mut harness.state_mut().1));
        let first_elapsed = started.elapsed();
        harness.run_steps(1);
        let second_pass = written_bytes(std::mem::take(&mut harness.state_mut().1));
        eprintln!(
            "PR10 before RawInput->host first_logic_input_bytes={} second_logic_input_bytes={} first_pass_us={} second_pass_us={} scope=private-egui-pipeline",
            first_pass.len(),
            second_pass.len(),
            first_elapsed.as_micros(),
            started.elapsed().as_micros()
        );
        assert!(first_pass.is_empty());
        assert_eq!(second_pass, b"direct");
    }

    #[test]
    fn pr10_discarded_pass_does_not_duplicate_terminal_input() {
        let catalog = catalog();
        let config = TerminalConfig::default();
        let mut focused = setup_focused_local_pane_harness(SessionId(7));
        let workspace = std::mem::replace(focused.state_mut(), WorkspaceUi::new());
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, state: &mut (WorkspaceUi, Vec<RuntimeCommand>, bool)| {
                // eframe invokes logic before UI on every correction pass. The actual App
                // full-drain fence must leave this frame's discarded-pass bytes staged.
                if crate::app::workspace_protocol_logic_pass_ready(ui.ctx()) {
                    state.1.extend(drain_protocol(&mut state.0));
                }
                state.0.show_with_input(ui, &config, &[], &catalog, true);
                if state.2 && ui.ctx().current_pass_index() == 0 {
                    ui.ctx().request_discard("PR10 private correction pass");
                }
                state.0.flush_render_side_effects(ui.ctx());
                crate::app::dispatch_terminal_protocol_tail(&mut state.0, ui.ctx(), |command| {
                    assert!(!ui.ctx().will_discard());
                    state.1.push(command);
                    Ok(())
                });
            },
            (workspace, Vec::new(), false),
        );
        harness.run();
        harness.state_mut().0.pending_focus = Some(pane_id("pane"));
        harness.run();
        harness.state_mut().1.clear();
        harness.state_mut().2 = true;
        harness
            .input_mut()
            .events
            .push(egui::Event::Text("한번".into()));
        harness.run_steps(1);
        assert_eq!(
            written_bytes(std::mem::take(&mut harness.state_mut().1)),
            "한번".as_bytes()
        );
    }

    fn pr10_dispatch(workspace: &mut WorkspaceUi, ctx: &egui::Context) -> Vec<RuntimeCommand> {
        let mut commands = Vec::new();
        crate::app::dispatch_terminal_protocol_tail(workspace, ctx, |command| {
            commands.push(command);
            Ok(())
        });
        commands
    }

    #[test]
    fn pr10_host_retry_keeps_fifo_generation_and_does_not_copy_on_idle_frames() {
        let mut workspace = WorkspaceUi::new();
        let bytes = vec![b'x'; WORKSPACE_PROTOCOL_INPUT_MAX_BYTES];
        let pointer = bytes.as_ptr();
        workspace.send(RuntimeCommand::WriteInput {
            session: SessionId(7),
            bytes,
        });
        let key = (
            workspace.protocol_intents[0].operation,
            workspace.protocol_intents[0].generation,
        );
        workspace.send(RuntimeCommand::FocusPane {
            pane: pane_id("next"),
        });
        workspace.send(RuntimeCommand::Resize {
            session: SessionId(8),
            cols: 80,
            rows: 24,
        });
        workspace.send(RuntimeCommand::WriteInput {
            session: SessionId(8),
            bytes: b"next\r".to_vec(),
        });
        let ctx = egui::Context::default();
        let mut attempts = 0;
        crate::app::dispatch_terminal_protocol_tail(&mut workspace, &ctx, |command| {
            attempts += 1;
            Err((
                runtime::RuntimeCommandSendError::Backpressure.into(),
                Box::new(command),
            ))
        });
        assert_eq!(attempts, 1);
        assert!(workspace.protocol_inflight.is_empty());
        assert_eq!(
            (
                workspace.protocol_intents[0].operation,
                workspace.protocol_intents[0].generation
            ),
            key
        );
        workspace.protocol_retry_at =
            Some(std::time::Instant::now() + std::time::Duration::from_secs(3600));
        for _ in 0..120 {
            crate::app::dispatch_terminal_protocol_tail(&mut workspace, &ctx, |_| {
                panic!("idle frame rebuilt or retried the retained command")
            });
        }
        assert!(
            matches!(&workspace.protocol_intents[0].command, RuntimeCommand::WriteInput { session: SessionId(7), bytes }
            if bytes.as_ptr() == pointer && bytes.capacity() == WORKSPACE_PROTOCOL_INPUT_MAX_BYTES)
        );
        workspace.protocol_retry_at = Some(std::time::Instant::now());
        let commands = pr10_dispatch(&mut workspace, &ctx);
        assert!(matches!(commands.as_slice(), [
            RuntimeCommand::WriteInput { session: SessionId(7), bytes },
            RuntimeCommand::FocusPane { pane },
            RuntimeCommand::Resize { session: SessionId(8), .. },
            RuntimeCommand::WriteInput { session: SessionId(8), bytes: next },
        ] if bytes.as_ptr() == pointer && pane == &pane_id("next") && next == b"next\r"));
        assert_eq!(workspace.retained_protocol_input_bytes(), 0);
    }

    #[test]
    fn pr10_exact_one_mib_input_then_tiny_key_uses_next_fifo_entry() {
        let mut workspace = WorkspaceUi::new();
        workspace.send(RuntimeCommand::WriteInput {
            session: SessionId(7),
            bytes: vec![b'x'; WORKSPACE_PROTOCOL_INPUT_MAX_BYTES],
        });
        workspace.send(RuntimeCommand::WriteInput {
            session: SessionId(7),
            bytes: "글".as_bytes().to_vec(),
        });
        assert_eq!(workspace.protocol_intents.len(), 2);
        assert!(!workspace.protocol_request_lost);
        assert_eq!(
            written_bytes(drain_protocol(&mut workspace)).len(),
            WORKSPACE_PROTOCOL_INPUT_MAX_BYTES + "글".len()
        );
    }

    #[test]
    fn pr10_coalesced_blocked_gestures_grow_capacity_geometrically_within_budget() {
        let mut workspace = WorkspaceUi::new();
        let mut growths = 0;
        let mut previous = 0;
        for _ in 0..256 {
            workspace.send(RuntimeCommand::WriteInput {
                session: SessionId(7),
                bytes: vec![b'x'; 4096],
            });
            let retained = workspace.retained_protocol_input_bytes();
            growths += usize::from(retained != previous);
            previous = retained;
            assert!(retained <= WORKSPACE_PROTOCOL_INPUT_MAX_BYTES);
        }
        eprintln!(
            "PR10 blocked adjacent gestures=256 body_bytes={} capacity_growths={growths}",
            previous
        );
        assert!(
            growths <= 9,
            "reallocating the growing retained body on every new gesture is quadratic"
        );
        assert_eq!(workspace.protocol_intents.len(), 1);
    }

    #[test]
    fn pr10_pressure_reserve_and_aggregate_capacity_reject_whole_gesture_visibly() {
        let mut workspace = WorkspaceUi::new();
        for index in 0..TERMINAL_PROTOCOL_PRESSURE_CAP {
            assert!(workspace.send_keep_selection(RuntimeCommand::Scroll {
                session: SessionId(index as u64),
                delta: 1
            }));
        }
        assert!(!workspace.send_keep_selection(RuntimeCommand::WriteInput {
            session: SessionId(77),
            bytes: b"whole gesture".to_vec()
        }));
        assert!(workspace.protocol_request_lost);
        assert_eq!(
            workspace.protocol_intents.len(),
            TERMINAL_PROTOCOL_PRESSURE_CAP
        );
        assert!(written_bytes(drain_protocol(&mut workspace)).is_empty());
        workspace.protocol_request_lost = false;
        let mut held = Vec::new();
        for index in 0..WORKSPACE_PROTOCOL_CAP {
            workspace.send(RuntimeCommand::WriteInput {
                session: SessionId(index as u64),
                bytes: vec![0; WORKSPACE_PROTOCOL_INPUT_MAX_BYTES],
            });
            held.push(workspace.take_protocol_intent().unwrap());
        }
        assert_eq!(
            workspace.retained_protocol_input_bytes(),
            TERMINAL_PROTOCOL_RETAINED_INPUT_MAX_BYTES
        );
        assert!(!workspace.send_keep_selection(RuntimeCommand::WriteInput {
            session: SessionId(90),
            bytes: b"whole".to_vec()
        }));
        assert!(workspace.protocol_request_lost);
        assert!(workspace.protocol_intents.is_empty());
        for intent in held {
            workspace.complete_protocol(WorkspaceProtocolCompletion {
                operation: intent.operation(),
                generation: intent.generation(),
                result: Ok(()),
            });
        }
        assert_eq!(workspace.retained_protocol_input_bytes(), 0);
        let mut oversized = Vec::with_capacity(4 * WORKSPACE_PROTOCOL_INPUT_MAX_BYTES);
        oversized.extend_from_slice(b"small");
        workspace.send(RuntimeCommand::WriteInput {
            session: SessionId(90),
            bytes: oversized,
        });
        assert_eq!(workspace.retained_protocol_input_bytes(), 5);
    }

    #[test]
    fn pr10_tail_stops_at_lifecycle_and_disconnect_is_visible_without_retry() {
        let mut workspace = WorkspaceUi::new();
        workspace.send(RuntimeCommand::WriteInput {
            session: SessionId(7),
            bytes: b"first".to_vec(),
        });
        workspace
            .queue_protocol_intent(RuntimeCommand::ClosePane {
                pane: pane_id("closing"),
            })
            .unwrap();
        workspace.send(RuntimeCommand::WriteInput {
            session: SessionId(7),
            bytes: b"later".to_vec(),
        });
        assert_eq!(
            written_bytes(pr10_dispatch(&mut workspace, &egui::Context::default())),
            b"first"
        );
        assert_eq!(workspace.protocol_intents.len(), 2);
        drain_protocol(&mut workspace);
        workspace.send(RuntimeCommand::WriteInput {
            session: SessionId(7),
            bytes: b"unsent".to_vec(),
        });
        crate::app::dispatch_terminal_protocol_tail(
            &mut workspace,
            &egui::Context::default(),
            |command| {
                Err((
                    runtime::RuntimeCommandSendError::Disconnected.into(),
                    Box::new(command),
                ))
            },
        );
        assert!(workspace.protocol_intents.is_empty());
        assert!(workspace.protocol_inflight.is_empty());
        assert!(workspace.protocol_retry_at.is_none());
        assert!(workspace.protocol_request_lost);
    }

    #[test]
    fn pr10_raw_input_pipeline_after_measurement_and_ime_control_paste() {
        let catalog = catalog();
        let config = TerminalConfig::default();
        let mut focused = setup_focused_local_pane_harness(SessionId(7));
        let workspace = std::mem::replace(focused.state_mut(), WorkspaceUi::new());
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, state: &mut (WorkspaceUi, Vec<RuntimeCommand>)| {
                // Real final-pass composition adapter, after the Workspace mapper/resize flush.
                state.0.show_with_input(ui, &config, &[], &catalog, true);
                state.0.flush_render_side_effects(ui.ctx());
                state.1.extend(pr10_dispatch(&mut state.0, ui.ctx()));
            },
            (workspace, Vec::new()),
        );
        harness.run();
        harness.state_mut().0.pending_focus = Some(pane_id("pane"));
        harness.run();
        harness.state_mut().1.clear();
        let started = std::time::Instant::now();
        harness
            .input_mut()
            .events
            .push(egui::Event::Text("direct".into()));
        harness.run_steps(1);
        let bytes = written_bytes(std::mem::take(&mut harness.state_mut().1));
        eprintln!(
            "PR10 after RawInput->host first_pass_input_bytes={} first_pass_us={} scope=private-egui-pipeline",
            bytes.len(),
            started.elapsed().as_micros()
        );
        assert_eq!(bytes, b"direct");
        harness.input_mut().events.push(preedit_event("글"));
        harness.run_steps(1);
        assert!(written_bytes(std::mem::take(&mut harness.state_mut().1)).is_empty());
        harness.input_mut().events.extend([
            egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            },
            commit_event("글"),
        ]);
        harness.run_steps(1);
        assert_eq!(
            written_bytes(std::mem::take(&mut harness.state_mut().1)),
            "글\r".as_bytes()
        );
        harness.input_mut().events.extend([
            egui::Event::Paste("붙여넣기".into()),
            egui::Event::Key {
                key: egui::Key::ArrowLeft,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            },
        ]);
        harness.run_steps(1);
        let bytes = written_bytes(std::mem::take(&mut harness.state_mut().1));
        assert!(
            bytes
                .windows("붙여넣기".len())
                .any(|part| part == "붙여넣기".as_bytes())
        );
        assert!(bytes.windows(3).any(|part| part == b"\x1b[D"));
    }

    #[test]
    fn pr10_raw_input_owner_switch_and_modal_fence_preserve_original_queued_bytes() {
        let catalog = catalog();
        let config = TerminalConfig::default();
        let mut focused = setup_focused_local_pane_harness(SessionId(7));
        let workspace = std::mem::replace(focused.state_mut(), WorkspaceUi::new());
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, state: &mut (WorkspaceUi, Vec<RuntimeCommand>, bool, bool)| {
                super::super::popup::set_pending_modal(ui.ctx(), state.3);
                state.0.show_with_input(ui, &config, &[], &catalog, true);
                state.0.flush_render_side_effects(ui.ctx());
                crate::app::dispatch_terminal_protocol_tail(&mut state.0, ui.ctx(), |command| {
                    if state.2 {
                        Err((
                            runtime::RuntimeCommandSendError::Backpressure.into(),
                            Box::new(command),
                        ))
                    } else {
                        state.1.push(command);
                        Ok(())
                    }
                });
            },
            (workspace, Vec::new(), false, false),
        );
        harness.run();
        harness.state_mut().0.pending_focus = Some(pane_id("pane"));
        harness.run();
        harness.state_mut().1.clear();
        harness.state_mut().2 = true;
        harness
            .input_mut()
            .events
            .push(egui::Event::Text("old".into()));
        harness.run_steps(1);
        harness.state_mut().0.protocol_retry_at =
            Some(std::time::Instant::now() + std::time::Duration::from_secs(3600));
        let old_key = (
            harness.state().0.protocol_intents[0].operation,
            harness.state().0.protocol_intents[0].generation,
        );
        harness.state_mut().0.mux = Some(mux(
            "primary",
            vec![tab(
                "primary",
                vec![pane("pane", SessionId(8))],
                LayoutNode::Pane(pane_id("pane")),
            )],
            "pane",
        ));
        harness
            .state_mut()
            .0
            .sessions
            .entry(SessionId(8))
            .or_default()
            .snapshot = Some(snapshot("new"));
        harness.state_mut().0.pending_focus = Some(pane_id("pane"));
        harness
            .input_mut()
            .events
            .push(egui::Event::Text("new".into()));
        harness.run_steps(1);
        harness.state_mut().3 = true;
        harness
            .input_mut()
            .events
            .push(egui::Event::Text("blocked".into()));
        harness.run_steps(1);
        assert_eq!(
            (
                harness.state().0.protocol_intents[0].operation,
                harness.state().0.protocol_intents[0].generation
            ),
            old_key
        );
        harness.state_mut().0.protocol_retry_at = Some(std::time::Instant::now());
        harness.state_mut().2 = false;
        harness.run_steps(1);
        let inputs: Vec<_> = std::mem::take(&mut harness.state_mut().1)
            .into_iter()
            .filter_map(|command| {
                if let RuntimeCommand::WriteInput { session, bytes } = command {
                    Some((session, bytes))
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            inputs,
            vec![
                (SessionId(7), b"old".to_vec()),
                (SessionId(8), b"new".to_vec())
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn pr10_actual_raw_ime_commit_reaches_private_pty_echo_in_same_pass() {
        use runtime::{RuntimeCommandSink, RuntimeEventStream};
        struct NoSecrets;
        impl runtime::RuntimeSecretResolver for NoSecrets {
            fn resolve(&self, _: &str) -> anyhow::Result<runtime::RuntimeSecret> {
                anyhow::bail!("private PR10 fixture has no secrets")
            }
        }
        struct PrivateRoot(PathBuf);
        impl Drop for PrivateRoot {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let root =
            PrivateRoot(std::env::temp_dir().join(format!("deppy-pr10-{}", uuid::Uuid::new_v4())));
        std::fs::create_dir_all(&root.0).unwrap();
        let mut client = runtime::InProcessRuntimeClient::try_new_with_resolver(
            5,
            Arc::new(NoSecrets),
            root.0.clone(),
            secret::RedactionService::new(),
            None,
            Some(root.0.clone()),
            vec![],
        )
        .unwrap();
        let rx = client.subscribe();
        client
            .send_command(RuntimeCommand::SpawnAgent {
                agent_config_id: None,
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
                command: "/bin/sh".into(),
                args: vec![
                    "-c".into(),
                    "stty -echo; printf 'PR10_READY\\r\\n'; exec /bin/cat".into(),
                ],
                env_plain: vec![],
                env_secrets: vec![],
                waiting_regex: None,
                approval_regex: None,
                error_regex: None,
                done_regex: None,
            })
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut session = None;
        let mut ready = false;
        while session.is_none() || !ready {
            for event in rx.drain() {
                if let RuntimeEvent::AgentSpawned { session: spawned } = event {
                    session = Some(spawned);
                }
                if let Some((_, screen, _, _)) = event.viewport() {
                    ready |= screen
                        .visible_cells
                        .iter()
                        .map(|cell| cell.c)
                        .collect::<String>()
                        .contains("PR10_READY");
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "private cat startup timeout"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let session = session.unwrap();
        let mut focused = setup_focused_local_pane_harness(session);
        let mut workspace = std::mem::replace(focused.state_mut(), WorkspaceUi::new());
        let config = TerminalConfig::default();
        let catalog = catalog();
        let mut sent = 0;
        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui,
             state: &mut (
                &mut WorkspaceUi,
                &runtime::InProcessRuntimeClient,
                &mut usize,
            )| {
                state.0.show_with_input(ui, &config, &[], &catalog, true);
                state.0.flush_render_side_effects(ui.ctx());
                crate::app::dispatch_terminal_protocol_tail(state.0, ui.ctx(), |command| {
                    if let RuntimeCommand::WriteInput {
                        session: target,
                        bytes,
                    } = &command
                    {
                        assert_eq!(*target, session);
                        assert_eq!(bytes, "한글빠른입력\r".as_bytes());
                        *state.2 += 1;
                    }
                    state.1.send_command_owned(command)
                });
            },
            (&mut workspace, &client, &mut sent),
        );
        harness.run();
        harness.state_mut().0.pending_focus = Some(pane_id("pane"));
        harness.run();
        harness
            .input_mut()
            .events
            .push(preedit_event("한글빠른입력"));
        harness.run_steps(1);
        let started = std::time::Instant::now();
        harness.input_mut().events.extend([
            egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            },
            commit_event("한글빠른입력"),
        ]);
        harness.run_steps(1);
        assert_eq!(
            *harness.state().2,
            1,
            "exactly one input admitted before the next frame"
        );
        let host_us = started.elapsed().as_micros();
        drop(harness);
        let deadline = started + std::time::Duration::from_secs(5);
        let mut seen = VecDeque::new();
        loop {
            let mut echoed = false;
            for event in rx.drain() {
                if let Some((id, screen, _, _)) = event.viewport() {
                    let text: String = screen
                        .visible_cells
                        .iter()
                        .filter(|cell| !cell.wide_spacer())
                        .map(|cell| cell.c)
                        .collect();
                    echoed |= id == session && text.contains("한글빠른입력");
                    if seen.len() == 8 {
                        seen.pop_front();
                    }
                    seen.push_back(text);
                } else if matches!(
                    &event,
                    RuntimeEvent::PtyInputPressure { .. } | RuntimeEvent::SessionExited { .. }
                ) {
                    panic!("private echo input unexpectedly rejected/exited");
                }
            }
            if echoed {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "private cat echo timeout; private decoded viewport history={seen:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        eprintln!(
            "PR10 private RawInput IME->channel host_us={host_us} ->cat viewport echo_us={} scope=private-pty-not-native-Grok",
            started.elapsed().as_micros()
        );
        client.shutdown();
    }

    #[test]
    fn 한글_조합중_enter가_commit보다_먼저_와도_완성된_문장부터_제출한다() {
        let mut harness = setup_focused_local_pane_harness(SessionId(7));
        harness.input_mut().events.push(preedit_event("요"));
        harness.run_steps(1);
        assert!(written_bytes(drain_protocol(harness.state_mut())).is_empty());

        harness.input_mut().events.extend([
            egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            },
            preedit_event(""),
            commit_event("요"),
        ]);
        harness.run_steps(1);
        assert_eq!(
            written_bytes(drain_protocol(harness.state_mut())),
            "요\r".as_bytes()
        );
    }

    #[test]
    fn 한글_조합중_shift_enter도_commit_뒤에_줄바꿈을_보낸다() {
        let mut harness = setup_focused_local_pane_harness(SessionId(7));
        harness.input_mut().events.push(preedit_event("글"));
        harness.run_steps(1);
        drain_protocol(harness.state_mut());

        harness.input_mut().events.extend([
            egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::SHIFT,
            },
            commit_event("글"),
        ]);
        harness.run_steps(1);
        assert_eq!(
            written_bytes(drain_protocol(harness.state_mut())),
            "글\n".as_bytes()
        );
    }

    #[test]
    fn 조합이_없는_enter는_평소대로_전달한다() {
        let mut harness = setup_focused_local_pane_harness(SessionId(7));
        harness.input_mut().events.push(egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });
        harness.run_steps(1);
        assert_eq!(written_bytes(drain_protocol(harness.state_mut())), b"\r");
    }

    #[test]
    fn 한글_enter와_commit이_서로_다른_프레임이어도_완성한_뒤_제출한다() {
        let mut harness = setup_focused_local_pane_harness(SessionId(7));
        harness.input_mut().events.push(preedit_event("요"));
        harness.run_steps(1);
        drain_protocol(harness.state_mut());
        harness.input_mut().events.extend([
            egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            },
            preedit_event(""),
        ]);
        harness.run_steps(1);
        assert!(written_bytes(drain_protocol(harness.state_mut())).is_empty());
        harness.input_mut().events.push(commit_event("요"));
        harness.run_steps(1);
        assert_eq!(
            written_bytes(drain_protocol(harness.state_mut())),
            "요\r".as_bytes()
        );
    }

    #[test]
    fn 한글_enter와_commit_사이에_다음_키가_와도_제출_뒤에_도착한다() {
        let mut harness = setup_focused_local_pane_harness(SessionId(7));
        harness.input_mut().events.push(preedit_event("한"));
        harness.run_steps(1);
        drain_protocol(harness.state_mut());
        harness.input_mut().events.extend([
            egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            },
            egui::Event::Key {
                key: egui::Key::Tab,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            },
            commit_event("한"),
        ]);
        harness.run_steps(1);
        assert_eq!(
            written_bytes(drain_protocol(harness.state_mut())),
            "한\r\t".as_bytes()
        );
    }

    #[test]
    fn 누락된_문장부호는_enter_전후의_물리키_순서를_유지한다() {
        let native = [
            crate::native_key_monitor::NativePrintableKeyDown::for_test('.'),
            crate::native_key_monitor::NativePrintableKeyDown::for_test_after_submit(','),
        ];
        let events = [
            preedit_event("한"),
            egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            },
            commit_event("한"),
        ];
        let reconciled = reconcile_ime_text_events(&native, &events, false, false);
        assert_eq!(reconciled.fallback_bytes, b".");
        assert_eq!(reconciled.fallback_after_submit_bytes, b",");
        let mut output = "한\r\t".as_bytes().to_vec();
        settle_ime_fallback(
            &mut output,
            &mut None,
            false,
            Some("한".len()),
            0,
            Some("한".len() + 1),
            &reconciled,
        );
        assert_eq!(output, "한.\r,\t".as_bytes());
    }

    #[test]
    fn enter_뒤_text는_enter_전_문장부호를_끌고_가지_않는다() {
        let native = [
            crate::native_key_monitor::NativePrintableKeyDown::for_test('.'),
            crate::native_key_monitor::NativePrintableKeyDown::for_test_after_submit(','),
        ];
        let events = [
            preedit_event("한"),
            egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            },
            commit_event("한"),
            egui::Event::Text(",".into()),
        ];
        let reconciled = reconcile_ime_text_events(&native, &events, false, false);
        assert_eq!(reconciled.event_text[3].as_deref(), Some(","));
        assert_eq!(reconciled.fallback_bytes, b".");
    }

    #[test]
    fn 다음_프레임_대기중에도_text가_사라진_문장부호를_복구한다() {
        let native = [crate::native_key_monitor::NativePrintableKeyDown::for_test(
            '.',
        )];
        let events = [preedit_event("")];
        let reconciled = reconcile_ime_text_events(&native, &events, false, true);
        assert_eq!(reconciled.fallback_after_submit_bytes, b".");
        let mut deferred = Some(PendingImeSubmit {
            owner: SessionId(7),
            started: std::time::Instant::now(),
            preedit_at_submit: String::new(),
            before_submit: Vec::new(),
            independent_before_submit: Vec::new(),
            after_submit: b"\r\t".to_vec(),
        });
        settle_ime_fallback(
            &mut Vec::new(),
            &mut deferred,
            true,
            None,
            1,
            None,
            &reconciled,
        );
        assert_eq!(deferred.unwrap().after_submit, b"\r.\t");
    }

    #[test]
    fn 조합_확정이_늦어도_보류한_enter를_버리지_않는다() {
        let mut harness = setup_focused_local_pane_harness(SessionId(7));
        harness.state_mut().pending_ime_submit = Some(PendingImeSubmit {
            owner: SessionId(7),
            started: std::time::Instant::now() - std::time::Duration::from_secs(3),
            preedit_at_submit: String::new(),
            before_submit: Vec::new(),
            independent_before_submit: Vec::new(),
            after_submit: vec![b'\r'],
        });
        harness.input_mut().events.push(commit_event("요"));
        harness.run_steps(1);
        assert_eq!(
            written_bytes(drain_protocol(harness.state_mut())),
            "요\r".as_bytes()
        );
    }

    #[test]
    fn commit이_오지_않는_보류입력은_기한뒤_잃지_않고_전달한다() {
        let mut harness = setup_focused_local_pane_harness(SessionId(7));
        harness.state_mut().pending_ime_submit = Some(PendingImeSubmit {
            owner: SessionId(7),
            started: std::time::Instant::now() - std::time::Duration::from_secs(3),
            preedit_at_submit: String::new(),
            before_submit: b".".to_vec(),
            independent_before_submit: Vec::new(),
            after_submit: b"\r\t".to_vec(),
        });
        harness.run_steps(1);
        assert_eq!(written_bytes(drain_protocol(harness.state_mut())), b".\r\t");
    }

    #[test]
    fn sidebar_line_summary_preserves_sparse_graphemes_with_a_bounded_cluster_prefix() {
        use terminal::TerminalBackend;
        for text in ["가ᇹ", "a\u{0301}\u{0308}", "a"] {
            let mut backend = terminal::AlacrittyBackend::new(80, 1, 0);
            backend.feed(text.as_bytes()).unwrap();
            assert_eq!(
                last_line_summary(&backend.viewport_snapshot().unwrap()),
                text
            );
        }
        let text = "x".repeat(47) + "a\u{0301}\u{0308}";
        let mut backend = terminal::AlacrittyBackend::new(80, 1, 0);
        backend.feed(text.as_bytes()).unwrap();
        assert_eq!(
            last_line_summary(&backend.viewport_snapshot().unwrap()),
            "x".repeat(47),
            "48-char limit must not cut a sparse cluster in the middle"
        );
    }

    #[test]
    fn enter_대기중_도착한_클립보드_결과는_commit_뒤에_보낸다() {
        let mut harness = setup_focused_local_pane_harness(SessionId(7));
        harness.input_mut().events.push(preedit_event("요"));
        harness.run_steps(1);
        drain_protocol(harness.state_mut());
        harness.input_mut().events.push(egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });
        harness.run_steps(1);
        drain_protocol(harness.state_mut());
        harness.state_mut().request_terminal_clipboard(
            SessionId(7),
            false,
            crate::ui::file_tree::ShellKind::Posix,
            None,
        );
        let Some(WorkspaceIoIntent::ReadTerminalClipboard {
            operation,
            generation,
        }) = harness.state_mut().take_io_intent()
        else {
            panic!("clipboard request missing");
        };
        harness
            .state_mut()
            .complete_io(WorkspaceIoCompletion::TerminalClipboardRead {
                operation,
                generation,
                result: TerminalClipboardPayload::try_new(Vec::new(), Some("paste".into())),
            });
        assert!(written_bytes(drain_protocol(harness.state_mut())).is_empty());
        harness.input_mut().events.push(commit_event("요"));
        harness.run_steps(1);
        assert_eq!(
            written_bytes(drain_protocol(harness.state_mut())),
            "요\rpaste".as_bytes()
        );
    }

    #[test]
    fn pane_전환은_보류한_키를_원래_세션으로_보낸다() {
        let mut workspace = WorkspaceUi::new();
        workspace.pending_ime_submit = Some(PendingImeSubmit {
            owner: SessionId(7),
            started: std::time::Instant::now(),
            preedit_at_submit: String::new(),
            before_submit: b".".to_vec(),
            independent_before_submit: Vec::new(),
            after_submit: b"\r\t".to_vec(),
        });
        workspace.begin_terminal_refocus(pane_id("next"));
        assert!(workspace.pending_ime_submit.is_none());
        assert!(matches!(
            drain_protocol(&mut workspace).as_slice(),
            [RuntimeCommand::WriteInput { session: SessionId(7), bytes }] if bytes == b".\r\t"
        ));
    }

    #[test]
    fn pane_전환은_미확정_한글을_enter보다_먼저_원래_세션에_보낸다() {
        let mut workspace = WorkspaceUi::new();
        workspace.preedit =
            TerminalPreeditState::default().for_frame(SessionId(7), &[preedit_event("요")]);
        workspace.pending_ime_submit = Some(PendingImeSubmit {
            owner: SessionId(7),
            started: std::time::Instant::now(),
            preedit_at_submit: "요".to_owned(),
            before_submit: Vec::new(),
            independent_before_submit: Vec::new(),
            after_submit: vec![b'\r'],
        });
        workspace.begin_terminal_refocus(pane_id("next"));
        assert!(workspace.detached_ime_submit.is_some());
        assert_eq!(
            workspace
                .resolve_detached_ime_submit(&[commit_event("요")], &egui::Context::default())
                .map(|(index, text, _)| (index, text)),
            Some((0, "요".to_owned()))
        );
        assert!(matches!(
            drain_protocol(&mut workspace).as_slice(),
            [RuntimeCommand::WriteInput { session: SessionId(7), bytes }] if bytes == "요\r".as_bytes()
        ));
    }

    #[test]
    fn pending_terminal_refocus_cannot_project_a_focused_textedit_preedit() {
        for event in [preedit_event("외부조합"), commit_event("외부조합")] {
            let mut harness = setup_focused_local_pane_harness(SessionId(7));
            let foreign = egui::Id::new("focused-textedit");
            egui::text_edit::TextEditState::default().store(&harness.ctx, foreign);
            harness
                .ctx
                .memory_mut(|memory| memory.request_focus(foreign));
            harness.state_mut().pending_focus = Some(pane_id("pane"));
            assert!(harness.ctx.text_edit_focused());
            harness.input_mut().events.push(event);
            harness.run_steps(1);
            assert!(
                !harness
                    .output()
                    .shapes
                    .iter()
                    .any(|shape| matches!(&shape.shape,
            egui::Shape::Text(text) if text.galley.text() == "외부조합")),
                "a TextEdit-owned preedit must not be projected into the terminal"
            );
            assert!(
                harness.output().platform_output.ime.is_none(),
                "the terminal must not replace the focused TextEdit candidate output"
            );
            assert!(harness.state().preedit.is_empty());
            assert!(written_bytes(drain_protocol(harness.state_mut())).is_empty());
        }
    }
    #[test]
    fn pending_refocus_keeps_actual_textedit_focus_for_the_complete_ime_batch() {
        for event in [preedit_event("외부조합"), commit_event("외부조합")] {
            let mut harness = setup_textedit_and_local_pane_harness(SessionId(7));
            let foreign = egui::Id::new("actual-textedit");
            harness
                .ctx
                .memory_mut(|memory| memory.request_focus(foreign));
            harness.state_mut().0.pending_focus = Some(pane_id("pane"));
            harness.input_mut().events.push(event);
            harness.run_steps(1);
            assert_eq!(
                harness.ctx.memory(|memory| memory.focused()),
                Some(foreign),
                "a deferred pane refocus must not steal the TextEdit's IME focus"
            );
            assert_eq!(harness.state().0.pending_focus, Some(pane_id("pane")));
            assert!(harness.state().0.preedit.is_empty());
            assert!(written_bytes(drain_protocol(&mut harness.state_mut().0)).is_empty());
            let ime = harness
                .output()
                .platform_output
                .ime
                .as_ref()
                .expect("focused TextEdit candidate output");
            assert_eq!(ime.purpose, egui::IMEPurpose::Normal);
            assert!(!ime.should_interrupt_composition);
        }
    }

    #[test]
    fn explicit_pane_click_can_claim_focus_during_a_foreign_textedit_ime_batch() {
        let mut harness = setup_textedit_and_local_pane_harness(SessionId(7));
        let foreign = egui::Id::new("actual-textedit");
        harness
            .ctx
            .memory_mut(|memory| memory.request_focus(foreign));
        harness.state_mut().0.pending_focus = Some(pane_id("pane"));
        let point = harness
            .output()
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Rect(rect) if rect.fill == renderer_egui::TERMINAL_SURFACE_BG => {
                    Some(rect.rect.center())
                }
                _ => None,
            })
            .expect("terminal pane rectangle");
        harness.input_mut().events.extend([
            preedit_event("외부조합"),
            egui::Event::PointerMoved(point),
            egui::Event::PointerButton {
                pos: point,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
            egui::Event::PointerButton {
                pos: point,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            },
        ]);
        harness.run_steps(1);
        assert_ne!(
            harness.ctx.memory(|memory| memory.focused()),
            Some(foreign),
            "an explicit pane click must still select the terminal"
        );
        assert!(harness.state().0.terminal_focus_claimed);
        assert!(written_bytes(drain_protocol(&mut harness.state_mut().0)).is_empty());
    }

    #[test]
    fn pending_refocus_is_consumed_after_a_textedit_commit_batch_has_finished() {
        let mut harness = setup_textedit_and_local_pane_harness(SessionId(7));
        let foreign = egui::Id::new("actual-textedit");
        harness
            .ctx
            .memory_mut(|memory| memory.request_focus(foreign));
        harness.state_mut().0.pending_focus = Some(pane_id("pane"));
        harness.input_mut().events.push(commit_event("외부조합"));
        harness.run_steps(1);
        assert_eq!(harness.ctx.memory(|memory| memory.focused()), Some(foreign));
        harness.run_steps(1); // Commit is finished; apply the queued pane intent once.
        assert_ne!(harness.ctx.memory(|memory| memory.focused()), Some(foreign));
        assert!(harness.state().0.pending_focus.is_none());
        assert!(written_bytes(drain_protocol(&mut harness.state_mut().0)).is_empty());
    }

    #[test]
    fn pending_terminal_refocus_still_recovers_the_first_ascii_key_from_stale_textedit_focus() {
        let mut harness = setup_focused_local_pane_harness(SessionId(7));
        let stale = egui::Id::new("stale-textedit");
        egui::text_edit::TextEditState::default().store(&harness.ctx, stale);
        harness.ctx.memory_mut(|memory| memory.request_focus(stale));
        harness.state_mut().pending_focus = Some(pane_id("pane"));
        harness
            .input_mut()
            .events
            .push(egui::Event::Text(".".into()));
        harness.run_steps(1);
        assert_eq!(written_bytes(drain_protocol(harness.state_mut())), b".");
    }

    #[test]
    fn preedit_from_a_replaced_pane_session_is_not_painted_or_forwarded() {
        let mut harness = setup_focused_local_pane_harness(SessionId(7));
        harness.input_mut().events.push(preedit_event("조합중"));
        harness.run_steps(1);
        assert_eq!(harness.state().preedit.owner, Some(SessionId(7)));
        drain_protocol(harness.state_mut());
        // Same pane id, different terminal session: the runtime-focus pane id
        // did not change, so the composition's explicit session owner matters.
        let workspace = harness.state_mut();
        workspace.mux = Some(mux(
            "primary",
            vec![tab(
                "primary",
                vec![pane("pane", SessionId(8))],
                LayoutNode::Pane(pane_id("pane")),
            )],
            "pane",
        ));
        workspace.sessions.entry(SessionId(8)).or_default().snapshot = Some(snapshot("ready"));
        harness.run_steps(1);
        assert!(
            !harness
                .output()
                .shapes
                .iter()
                .any(|shape| matches!(&shape.shape,
            egui::Shape::Text(text) if text.galley.text() == "조합중"))
        );
        assert!(harness.state().preedit.is_empty());
        assert!(written_bytes(drain_protocol(harness.state_mut())).is_empty());
    }

    #[test]
    fn disabled_terminal_does_not_publish_projected_preedit_or_send_commit() {
        let mut harness = setup_focused_local_pane_harness(SessionId(7));
        harness.input_mut().events.push(preedit_event("이전조합"));
        harness.run_steps(1);
        drain_protocol(harness.state_mut());
        let ctx = egui::Context::default();
        let config = TerminalConfig::default();
        let catalog = catalog();
        let raw = egui::RawInput {
            events: vec![preedit_event("다른입력"), commit_event("다른입력")],
            ..Default::default()
        };
        let full = ctx.run_ui(raw, |ui| {
            harness
                .state_mut()
                .show_with_input(ui, &config, &[], &catalog, false);
        });
        assert_eq!(harness.state().preedit.text, "이전조합");
        assert_eq!(harness.state().preedit.owner, Some(SessionId(7)));
        assert!(!full.shapes.iter().any(|shape| matches!(&shape.shape,
            egui::Shape::Text(text) if text.galley.text() == "다른입력" || text.galley.text() == "이전조합")));
        assert!(written_bytes(drain_protocol(harness.state_mut())).is_empty());
        full.drop_without_applying_deltas();
    }

    #[test]
    fn preedit_projection_preserves_char_ranges_and_clears_owner_on_cancel() {
        let owner = SessionId(7);
        let state = TerminalPreeditState::default().for_frame(
            owner,
            &[egui::Event::Ime(egui::ImeEvent::Preedit {
                text: "가🙂나다".into(),
                active_range_chars: Some(2..3),
            })],
        );
        assert_eq!(state.active_range_chars, Some(2..3));
        assert_eq!(state.owner, Some(owner));
        assert!(state.is_active_for(owner));
        assert!(!state.is_active_for(SessionId(8)));
        assert!(state.for_frame(SessionId(8), &[]).is_empty());
        let clamped = state.for_frame(
            owner,
            &[egui::Event::Ime(egui::ImeEvent::Preedit {
                text: "한글".into(),
                active_range_chars: Some(8..20),
            })],
        );
        assert_eq!(clamped.active_range_chars, Some(2..2));
        let reversed_start = "한글".chars().count();
        let reversed = state.for_frame(
            owner,
            &[egui::Event::Ime(egui::ImeEvent::Preedit {
                text: "한글".into(),
                active_range_chars: Some(reversed_start..reversed_start - 1),
            })],
        );
        assert_eq!(reversed.active_range_chars, Some(2..2));
        let none = state.for_frame(owner, &[preedit_event("한글")]);
        assert_eq!(none.active_range_chars, None);
        for event in [preedit_event(""), commit_event("한글")] {
            let cancelled = state.for_frame(owner, &[event]);
            assert!(cancelled.is_empty());
            assert_eq!(cancelled.owner, None);
            assert_eq!(cancelled.active_range_chars, None);
        }
        // Projection cannot mutate the prior frame used by physical-key reconciliation.
        assert_eq!(state.text, "가🙂나다");
    }

    #[test]
    fn 한글_preedit_후보창은_char_range_내부_커서를_따른다() {
        let mut harness = setup_focused_local_pane_harness(SessionId(7));
        harness
            .input_mut()
            .events
            .push(egui::Event::Ime(egui::ImeEvent::Preedit {
                text: "한글가나".into(),
                active_range_chars: Some(0..0),
            }));
        harness.run_steps(1);
        let start = harness
            .output()
            .platform_output
            .ime
            .as_ref()
            .unwrap()
            .cursor_rect;
        harness
            .input_mut()
            .events
            .push(egui::Event::Ime(egui::ImeEvent::Preedit {
                text: "한글가나".into(),
                active_range_chars: Some(2..3),
            }));
        harness.run_steps(1);
        let internal = harness
            .output()
            .platform_output
            .ime
            .as_ref()
            .unwrap()
            .cursor_rect;
        assert!(
            internal.left() > start.left() + 1.0,
            "selected char range must move candidate anchor"
        );
        assert!(written_bytes(drain_protocol(harness.state_mut())).is_empty());
    }

    #[test]
    fn 한글_preedit는_도착한_같은_pass에_표시된다() {
        let mut harness = setup_focused_local_pane_harness(SessionId(7));
        harness.input_mut().events.push(preedit_event("조합중"));
        harness.run_steps(1);
        assert!(
            harness
                .output()
                .shapes
                .iter()
                .any(|shape| matches!(&shape.shape,
            egui::Shape::Text(text) if text.galley.text() == "조합중")),
            "preedit text must be painted in its arrival pass"
        );
        assert!(written_bytes(drain_protocol(harness.state_mut())).is_empty());
    }

    #[test]
    fn 빠른_한글_연타로_한_프레임에_섞인_preedit_commit도_음절대로_pty에_들어간다() {
        let session = SessionId(7);
        let mut harness = setup_focused_local_pane_harness(session);

        // 실제 빠른 타이핑 재현: "가"→"나" 두 음절의 조합 이벤트 전부가 렌더
        // 프레임 하나에 몰려 도착한다(키 입력이 프레임 주기보다 빠를 때 실제로
        // 벌어지는 배치).
        harness.input_mut().events.extend([
            preedit_event("ㄱ"),
            preedit_event("가"),
            commit_event("가"),
            preedit_event("ㄴ"),
            preedit_event("나"),
            commit_event("나"),
        ]);
        harness.run();

        assert_eq!(
            written_bytes(drain_protocol(harness.state_mut())),
            "가나".as_bytes()
        );
    }

    #[test]
    fn 프레임_경계에서_끊긴_한글_조합도_다음_프레임에서_음절대로_이어붙는다() {
        let session = SessionId(7);
        let mut harness = setup_focused_local_pane_harness(session);

        // 프레임 1: 첫 자모의 preedit만 도착하고 렌더 프레임이 끼어든 경우 — 아직
        // 조합 중이므로 PTY로는 아무것도 나가면 안 된다.
        harness.input_mut().events.extend([preedit_event("ㄱ")]);
        harness.run();
        assert!(written_bytes(drain_protocol(harness.state_mut())).is_empty());
        assert_eq!(harness.state().preedit.text, "ㄱ");

        // 프레임 2: 나머지 조합과 다음 음절까지 이어서 도착 — self.preedit가
        // 프레임을 넘어 올바르게 이어지는지 확인한다.
        harness.input_mut().events.extend([
            preedit_event("가"),
            commit_event("가"),
            preedit_event("ㄴ"),
            preedit_event("나"),
            commit_event("나"),
        ]);
        harness.run();

        assert_eq!(
            written_bytes(drain_protocol(harness.state_mut())),
            "가나".as_bytes()
        );
    }

    #[test]
    fn terminal_focus_lock_filter는_app_request_focus와_같은_filter를_쓴다() {
        let filter = renderer_egui::terminal_focus_lock_filter();
        assert!(filter.tab);
        assert!(filter.horizontal_arrows);
        assert!(filter.vertical_arrows);
        assert!(filter.escape);
    }

    // PR-3: SessionRestored(앱 재시작 후 열람 전용 복원)와 SessionExited(실제 종료)가
    // exit_code 부기는 같아도 restored_readonly만 갈라야 pane 하단 배너/재실행 버튼이
    // 올바른 세션에만 뜬다.

    #[test]
    fn session_restored는_restored_readonly를_켜고_session_exited는_켜지_않는다() {
        let mut ui = WorkspaceUi::new();
        let catalog = catalog();
        let m = mux(
            "a",
            vec![tab(
                "a",
                vec![pane("pa", SessionId(1)), pane("pb", SessionId(2))],
                LayoutNode::Pane(pane_id("pa")),
            )],
            "pa",
        );
        ui.handle_events(&[RuntimeEvent::MuxUpdated { snapshot: m }], &catalog);
        ui.handle_events(
            &[
                RuntimeEvent::SessionRestored {
                    session: SessionId(1),
                    exit_code: Some(0),
                },
                RuntimeEvent::SessionExited {
                    session: SessionId(2),
                    exit_code: Some(1),
                },
            ],
            &catalog,
        );
        assert!(ui.sessions.get(&SessionId(1)).unwrap().restored_readonly);
        assert_eq!(
            ui.sessions.get(&SessionId(1)).unwrap().exit_code,
            Some(Some(0))
        );
        assert!(!ui.sessions.get(&SessionId(2)).unwrap().restored_readonly);
        assert_eq!(
            ui.sessions.get(&SessionId(2)).unwrap().exit_code,
            Some(Some(1))
        );
    }

    /// warm(비활성) 워크스페이스 경로(apply_warm_events)도 같은 규칙을 지킨다 — 재활성
    /// 전까지는 이 경로로만 부기되므로, 여기서 안 갈리면 재활성 뒤에도 배너가 틀린다.
    #[test]
    fn apply_warm_events도_restored_readonly를_같은_규칙으로_갈라_채운다() {
        let mut ui = WorkspaceUi::new();
        let catalog = catalog();
        let m = mux(
            "a",
            vec![tab(
                "a",
                vec![pane("pa", SessionId(1)), pane("pb", SessionId(2))],
                LayoutNode::Pane(pane_id("pa")),
            )],
            "pa",
        );
        ui.apply_warm_events(&[RuntimeEvent::MuxUpdated { snapshot: m }], &catalog);
        ui.apply_warm_events(
            &[
                RuntimeEvent::SessionRestored {
                    session: SessionId(1),
                    exit_code: None,
                },
                RuntimeEvent::SessionExited {
                    session: SessionId(2),
                    exit_code: None,
                },
            ],
            &catalog,
        );
        assert!(ui.sessions.get(&SessionId(1)).unwrap().restored_readonly);
        assert!(!ui.sessions.get(&SessionId(2)).unwrap().restored_readonly);
    }

    fn restored_archived_resume_harness(
        presentation: crate::agent_resume::ArchivedResumePresentation,
    ) -> egui_kittest::Harness<'static, WorkspaceUi> {
        let catalog = catalog();
        let config = TerminalConfig::default();
        let session = SessionId(7);
        let mut workspace = WorkspaceUi::new();
        workspace.mux = Some(mux(
            "primary",
            vec![tab(
                "primary",
                vec![pane("pane", session)],
                LayoutNode::Pane(pane_id("pane")),
            )],
            "pane",
        ));
        let view = workspace.sessions.entry(session).or_default();
        view.snapshot = Some(snapshot("archived"));
        view.snapshot_gen = 1;
        view.exit_code = Some(Some(0));
        view.restored_readonly = true;
        workspace.set_archived_resume_presentation(std::collections::HashMap::from([(
            session,
            presentation,
        )]));
        egui_kittest::Harness::new_ui_state(
            move |ui, workspace: &mut WorkspaceUi| {
                workspace.show_with_input(ui, &config, &[], &catalog, true);
            },
            workspace,
        )
    }

    #[test]
    fn restored_archived_resume_exact와_recent는_서로_다른_설명으로_이어실행한다() {
        use egui_kittest::kittest::Queryable;

        let catalog = catalog();
        let continue_label = catalog.t("workspace.exited.respawn_continue", &[]);
        let exact_message = catalog.t("workspace.exited.app_restart", &[]);
        let recent_message = catalog.t("workspace.exited.resume_recent", &[]);

        let mut exact = restored_archived_resume_harness(
            crate::agent_resume::ArchivedResumePresentation::Exact,
        );
        exact.step();
        assert!(exact.query_by_label(&exact_message).is_some());
        exact.get_by_label(&continue_label).click();
        exact.step();
        assert_eq!(
            exact.state_mut().take_respawn_archived_request(),
            Some(SessionId(7))
        );

        let mut recent = restored_archived_resume_harness(
            crate::agent_resume::ArchivedResumePresentation::RecentInCwd,
        );
        recent.step();
        assert!(recent.query_by_label(&recent_message).is_some());
        recent.get_by_label(&continue_label).click();
        recent.step();
        assert_eq!(
            recent.state_mut().take_respawn_archived_request(),
            Some(SessionId(7))
        );
    }

    #[test]
    fn restored_archived_resume_미지원만_새실행하고_확인중과미설치는_차단한다() {
        use egui_kittest::kittest::Queryable;

        let catalog = catalog();
        let new_label = catalog.t("workspace.exited.respawn_new", &[]);
        let cases = [
            (
                crate::agent_resume::ArchivedResumePresentation::Unsupported,
                "workspace.exited.resume_unsupported",
                true,
            ),
            (
                crate::agent_resume::ArchivedResumePresentation::Unavailable,
                "workspace.exited.resume_unavailable",
                false,
            ),
            (
                crate::agent_resume::ArchivedResumePresentation::Checking,
                "workspace.exited.resume_checking",
                false,
            ),
        ];

        for (presentation, message_key, dispatches) in cases {
            let message = catalog.t(message_key, &[]);
            let mut harness = restored_archived_resume_harness(presentation);
            harness.step();
            assert!(
                harness.query_by_label(&message).is_some(),
                "missing {message_key}"
            );
            let button_label =
                if presentation == crate::agent_resume::ArchivedResumePresentation::Checking {
                    catalog.t("status.detecting", &[])
                } else {
                    new_label.clone()
                };
            harness.get_by_label(&button_label).click();
            harness.step();
            assert_eq!(
                harness.state_mut().take_respawn_archived_request(),
                dispatches.then_some(SessionId(7)),
                "{presentation:?}"
            );
        }
    }
}
