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
const WORKSPACE_IO_QUEUE_CAP: usize = 1;
const WORKSPACE_PATH_MAX_BYTES: usize = 32 * 1024;
const WORKSPACE_URL_MAX_BYTES: usize = 32 * 1024;
const TERMINAL_CLIPBOARD_PATH_MAX_ITEMS: usize = 16;
const TERMINAL_CLIPBOARD_PATH_MAX_BYTES: usize = 256 * 1024;
const TERMINAL_CLIPBOARD_TEXT_MAX_BYTES: usize = 1024 * 1024;
const WORKSPACE_NOTICE_SUMMARY_MAX_BYTES: usize = 4 * 1024;
const WORKSPACE_NOTICE_BODY_MAX_BYTES: usize = 2 * 1024;
const WORKSPACE_NOTICE_TOTAL_MAX_BYTES: usize = 5 * 1024;
const WORKSPACE_PROTOCOL_CAP: usize = 8;
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
}

enum PendingShellSpawn {
    Awaiting { cwd: Option<String> },
    CwdWritePending { session: SessionId, cwd: String },
}

fn workspace_protocol_kind(command: &RuntimeCommand) -> &'static str {
    match command {
        RuntimeCommand::SpawnShell { .. } => "spawn_shell",
        RuntimeCommand::WriteInput { .. } => "write_input",
        RuntimeCommand::Resize { .. } => "resize",
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
                && !path.is_empty()
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
const TERMINAL_PANE_HEADER_HEIGHT: f32 = 32.0;
const TERMINAL_STREAM_LEFT_PADDING: f32 = 3.0;
const TERMINAL_STREAM_RIGHT_PADDING: f32 = 3.0;
const TERMINAL_STREAM_VERTICAL_PADDING: f32 = 6.0;

#[derive(Clone, Copy, Debug, PartialEq)]
struct PaneHeaderStyle {
    background: egui::Color32,
    selection_fill: Option<egui::Color32>,
    active_stroke: Option<egui::Stroke>,
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
    label: &str,
    style: PaneDropFeedbackStyle,
) -> Option<PaneDropFeedbackLabelLayout> {
    let margin = 8.0;
    let padding = egui::vec2(6.0, 3.0);
    let max_text_width = pane_rect.width() - margin * 2.0 - padding.x * 2.0;
    if max_text_width <= 0.0 {
        return None;
    }

    let mut job = egui::text::LayoutJob::single_section(
        label.to_owned(),
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
}

#[cfg(test)]
fn terminal_pane_layout(rect: egui::Rect) -> TerminalPaneLayout {
    terminal_pane_layout_with_embedded_header(rect, true)
}

fn terminal_pane_layout_with_embedded_header(
    rect: egui::Rect,
    embedded_header: bool,
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
    let pad_left = TERMINAL_STREAM_LEFT_PADDING.min(surface.width().max(0.0) * 0.25);
    let pad_right = TERMINAL_STREAM_RIGHT_PADDING.min(surface.width().max(0.0) * 0.25);
    let pad_y = TERMINAL_STREAM_VERTICAL_PADDING.min(surface.height().max(0.0) * 0.25);
    let content = egui::Rect::from_min_max(
        egui::pos2(surface.left() + pad_left, surface.top() + pad_y),
        egui::pos2(surface.right() - pad_right, surface.bottom() - pad_y),
    );
    TerminalPaneLayout {
        header,
        surface,
        content,
    }
}

fn keeps_embedded_pane_header(_layout: &LayoutNode) -> bool {
    true
}

/// pane 헤더 우측 도구 버튼 한 변(정사각)과 간격 — pane_header_buttons와
/// render_pane_header의 제목 폭 계산이 공유하는 단일 원천.
const PANE_HEADER_TOOLBAR_BUTTON: f32 = 20.0;
const PANE_HEADER_TOOLBAR_GAP: f32 = 2.0;

/// pane 헤더 버튼 기하 — 닫기(×)는 마지막까지 남는 버튼이다.
struct PaneHeaderButtons {
    /// 닫기(×) 히트박스. 항상 존재한다.
    close: egui::Rect,
    /// 표시할 우측 도구 히트박스(왼쪽→오른쪽). 아이콘은 전체 목록의 뒤에서부터
    /// `toolbar.len()`개를 대응시킨다 (왼쪽 도구부터 숨김).
    toolbar: Vec<egui::Rect>,
}

/// 헤더 폭·제목 폭으로 닫기(×)와 우측 도구의 히트박스를 계산한다.
///
/// codex 리뷰 P2 회귀 가드: compact 헤더(3e3e909)는 visible_toolbar를
/// clamp(1,·)로 최소 1개 강제해 589pt 픽스처의 10% pane(≈59px)에서 Split
/// 버튼이 닫기 히트박스 22px 중 17px를 덮었고, 도구 interact가 나중에
/// 등록되므로 겹침 클릭이 닫기 대신 분할을 실행했다. 지금은 도구 0개를
/// 허용하고, 만에 하나 기하가 어긋나 도구가 닫기를 덮으면 왼쪽 도구를 더
/// 숨겨 닫기가 항상 우선하도록 보장한다.
fn pane_header_buttons(
    header: egui::Rect,
    title_width: f32,
    icon_count: usize,
) -> PaneHeaderButtons {
    let title_left = header.left() + 17.0;
    let center_y = header.center().y;
    // 제목과 닫기 버튼을 먼저 온전히 확보한다. 분할 pane이 좁아지면 우측 도구를
    // 왼쪽부터 단계적으로 숨겨(0개 허용) 제목 글자가 중간에서 잘리는 일을 막는다.
    let toolbar_available = (header.width() - title_width - 61.0).max(0.0);
    let mut visible_toolbar = (((toolbar_available + PANE_HEADER_TOOLBAR_GAP)
        / (PANE_HEADER_TOOLBAR_BUTTON + PANE_HEADER_TOOLBAR_GAP))
        .floor() as usize)
        .min(icon_count);
    loop {
        let toolbar_width = PANE_HEADER_TOOLBAR_BUTTON * visible_toolbar as f32
            + PANE_HEADER_TOOLBAR_GAP * visible_toolbar.saturating_sub(1) as f32;
        let toolbar_left = header.right() - 4.0 - toolbar_width;
        let close_center_x = (title_left + title_width + 14.0)
            .min(toolbar_left - 11.0)
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
        return PaneHeaderButtons { close, toolbar };
    }
}

#[derive(Clone, Copy)]
enum TerminalToolbarIcon {
    Search,
    NewTerminal,
    SplitColumns,
    SplitRows,
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WorkspaceSurfaceOutput {
    pub focus_requested: bool,
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PaneRenderOutput {
    focus_requested: bool,
}

impl PaneRenderOutput {
    fn merge(&mut self, other: Self) {
        self.focus_requested |= other.focus_requested;
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
    let stroke = egui::Stroke::new(1.25, color);
    let center = rect.center();
    match icon {
        TerminalToolbarIcon::Search => {
            let lens = center + egui::vec2(-0.9, -0.9);
            painter.circle_stroke(lens, 3.2, stroke);
            painter.line_segment(
                [lens + egui::vec2(2.3, 2.3), lens + egui::vec2(4.5, 4.5)],
                stroke,
            );
        }
        TerminalToolbarIcon::NewTerminal => {
            let body = egui::Rect::from_center_size(center, egui::vec2(9.0, 7.0));
            painter.rect_stroke(body, 0.75, stroke, egui::StrokeKind::Inside);
            painter.line_segment(
                [
                    center + egui::vec2(-2.8, -1.5),
                    center + egui::vec2(-1.2, 0.0),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    center + egui::vec2(-1.2, 0.0),
                    center + egui::vec2(-2.8, 1.5),
                ],
                stroke,
            );
            painter.line_segment(
                [center + egui::vec2(0.0, 2.0), center + egui::vec2(2.7, 2.0)],
                stroke,
            );
        }
        TerminalToolbarIcon::SplitColumns | TerminalToolbarIcon::SplitRows => {
            let body = egui::Rect::from_center_size(center, egui::vec2(9.0, 9.0));
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

pub struct WorkspaceUi {
    mux: Option<Arc<MuxSnapshot>>,
    sessions: HashMap<SessionId, SessionView>,
    /// Runtime이 per-session shell metadata를 제공하기 전까지 path insert quoting에 쓰는
    /// workspace 기본 shell kind.
    shell_kind: crate::ui::file_tree::ShellKind,
    /// IME 조합 중 텍스트 (focused pane 전용)
    preedit: String,
    /// 이번 UI 프레임 직전에 AppKit local monitor가 본 ASCII 문장부호/숫자/공백
    /// key-down. IME Commit/Text와 대조한 뒤 누락된 문자만 복구하고 프레임 끝에 버린다.
    native_printable_key_downs: Vec<crate::native_key_monitor::NativePrintableKeyDown>,
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
    sent_sizes: HashMap<SessionId, (u16, u16)>,
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
    /// 이 프레임에 사용자가 터미널을 직접 클릭해 키보드 소유권을 요청했다. App이
    /// 뒤이어 렌더하는 Agents TextEdit의 지연 autofocus를 취소하는 one-shot 신호다.
    terminal_focus_claimed: bool,
    /// 성공 전달된 셸 spawn 순서와 프로토콜 입장을 기다리는 cd 후속 명령.
    /// spawn·후속 cd를 합쳐 최대 WORKSPACE_PROTOCOL_CAP개만 유지한다.
    pending_spawn_cwds: VecDeque<PendingShellSpawn>,
    /// split 경계 드래그 중 로컬 미리보기 (path, ratio). 드래그 동안은 명령을 보내지
    /// 않고(매 프레임 DB 저장 방지) 릴리즈 시 1회 ResizeSplit을 보낸다.
    split_drag: Option<(Vec<u8>, f32)>,
    /// 닫기 확인 대기 중인 pane — 실행 중 세션이 있는 pane 닫기는 확인을 거친다
    /// (2026-07-05 사용자 보고: 닫기 실수로 셸 전체 즉사 방지).
    confirm_close: Option<runtime::MuxPaneId>,
    /// 「에이전트로 보내기」 프리셋 프롬프트 (설정 미러 — App이 매 프레임 갱신).
    agent_send_presets: Vec<String>,
    /// pane 우클릭 → "환경변수·API 설정" 요청 (E4 ⑥). App이 프레임에서 take해
    /// 설정 창을 Environment 카테고리로 연다.
    open_environment_requested: bool,
    new_session_requested: bool,
    /// pane 우클릭 → 세션 폴더 요청(파일 트리 이동/Finder 열기, 2026-07-18). cwd
    /// 해석(lsof 폴백 포함)과 트리·Finder 라우팅은 App 몫이라 요청만 쌓는다 — E4 ⑥
    /// take_open_environment와 같은 프레임 소비 패턴.
    session_folder_request: Option<SessionFolderRequest>,
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
    /// 세션별 현재 작업 폴더(App이 매 프레임 set) — 1행 제목 폴더명/프로젝트명 원천.
    session_cwds: std::collections::HashMap<SessionId, String>,
    /// App host가 filesystem 밖에서 미리 계산한 세션별 프로젝트 표시명. cwd를 함께
    /// 검증하므로 늦은 결과가 이동한 셸의 제목을 덮지 못하고, immutable Arc snapshot이라
    /// 동일 revision setter는 전체 맵을 복제하지 않는다.
    session_project_names: SessionProjectNameSnapshot,
    /// 세션별 에이전트 표시정보(model/effort/context — App이 병합해 set) — 3줄 행 2/3행.
    agent_info: std::collections::HashMap<SessionId, crate::agent_detect::AgentDisplay>,
    /// App host가 수행 중인 terminal clipboard 요청. completion은 operation/generation을
    /// 모두 맞춘 뒤 정확히 한 번만 적용한다. 새 요청은 이전 요청을 stale로 만든다.
    pending_paste: Option<PendingPaste>,
    error: Option<String>,
    /// 현재 error 배너가 input backpressure 경고인지 — 해소 이벤트(queued=0)가
    /// 무관한 오류(spawn 실패 등)를 지우지 않게 구분한다(codex 2026-07-09).
    error_is_pressure: bool,
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

/// 터미널 텍스트 검색 세션 상태 (T3).
struct TerminalSearch {
    /// 검색 대상 세션 — 이 세션의 pane에만 검색 바를 그린다.
    session: SessionId,
    /// 현재 입력된 쿼리.
    query: String,
    /// worker에 마지막으로 요청한 쿼리 — 바뀔 때만 재검색한다(매 프레임 재검색 금지).
    requested: Option<String>,
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
        let display = path.to_string_lossy();
        if display.as_bytes().contains(&0) {
            return Err(WorkspaceIoErrorCode::InvalidPath);
        }
        let bytes = display.len();
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
    pub fn try_new(
        paths: Vec<PathBuf>,
        text: Option<String>,
    ) -> Result<Self, WorkspaceIoErrorCode> {
        if paths.len() > TERMINAL_CLIPBOARD_PATH_MAX_ITEMS {
            return Err(WorkspaceIoErrorCode::ClipboardTooLarge);
        }
        let mut path_bytes = 0usize;
        for path in &paths {
            let display = path.to_string_lossy();
            if display.as_bytes().contains(&0) || display.len() > WORKSPACE_PATH_MAX_BYTES {
                return Err(WorkspaceIoErrorCode::InvalidPath);
            }
            path_bytes = path_bytes
                .checked_add(display.len())
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

/// App host paste 결과의 수명 — 이보다 오래된 완료는 버린다.
const PASTE_TASK_TTL: std::time::Duration = std::time::Duration::from_secs(10);

/// 세션별 화면 캐시. hidden tab 세션의 스냅샷은 `MuxUpdated`에서 버린다.
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
    render_cache: renderer_egui::TerminalRenderCache,
    bracketed_paste: bool,
    /// 사이드바 세션 목록에 보여줄 최신 화면 요약 (마지막 비어있지 않은 행, ≤48자)
    summary: String,
    exit_code: Option<Option<u32>>,
    /// status detector 감지 상태 (agent만, PR-12)
    status: Option<SessionStatus>,
    status_view: Option<runtime::SessionStatusView>,
    input_pressure: Option<runtime::PtyInputPressure>,
}

impl WorkspaceUi {
    pub fn new() -> Self {
        Self {
            mux: None,
            sessions: HashMap::new(),
            shell_kind: crate::ui::file_tree::default_shell_kind(),
            preedit: String::new(),
            native_printable_key_downs: Vec::new(),
            native_clipboard_paste_requested: false,
            native_clipboard_copy_requested: false,
            prepared_attached_input_owner: None,
            prepared_attached_input_pass: None,
            prepared_attached_input_consumed: false,
            suppress_paste_request: false,
            suppress_copy_request: false,
            paste_suppressed: false,
            copy_suppressed: false,
            sent_sizes: HashMap::new(),
            scroll_residual: 0.0,
            drag_autoscroll_residual: 0.0,
            command_sent: false,
            last_focused_pane: None,
            session_flash: HashMap::new(),
            pending_focus: None,
            terminal_focus_claimed: false,
            pending_spawn_cwds: VecDeque::with_capacity(WORKSPACE_PROTOCOL_CAP),
            split_drag: None,
            confirm_close: None,
            agent_send_presets: Vec::new(),
            open_environment_requested: false,
            new_session_requested: false,
            session_folder_request: None,
            selection: None,
            project_name: None,
            ui_scale: 1.0,
            workspace_accent: egui::Color32::TRANSPARENT,
            session_pids: HashMap::new(),
            path_click_cache: None,
            io_generation: 1,
            next_io_operation: 1,
            io_intents: VecDeque::with_capacity(WORKSPACE_IO_QUEUE_CAP),
            protocol_generation: 1,
            next_protocol_operation: 1,
            protocol_intents: VecDeque::with_capacity(WORKSPACE_PROTOCOL_CAP),
            protocol_inflight: HashMap::with_capacity(WORKSPACE_PROTOCOL_CAP),
            pending_path_resolution: None,
            last_dir_click: None,
            last_url_click: None,
            last_text_paste: None,
            last_native_paste: None,
            session_cwds: std::collections::HashMap::new(),
            session_project_names: SessionProjectNameSnapshot::default(),
            agent_info: std::collections::HashMap::new(),
            pending_paste: None,
            error: None,
            error_is_pressure: false,
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
        if self.io_intents.len() >= WORKSPACE_IO_QUEUE_CAP {
            return Err(WorkspaceIoErrorCode::Busy);
        }
        self.io_intents.push_back(intent);
        Ok(())
    }

    /// App host가 실행할 다음 native I/O intent. 큐는 최대 8개이며 render는 이 API로
    /// 실행 권한만 넘긴다. 큐가 비면 idle thread/network/polling이 생기지 않는다.
    pub fn take_io_intent(&mut self) -> Option<WorkspaceIoIntent> {
        self.io_intents.pop_front()
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
        self.queue_protocol_intent_with_spawn_cwd(command, None)
    }

    fn queue_terminal_resize(&mut self, session: SessionId, cols: u16, rows: u16) {
        if self.sent_sizes.get(&session) == Some(&(cols, rows)) {
            return;
        }
        if self
            .queue_protocol_intent(RuntimeCommand::Resize {
                session,
                cols,
                rows,
            })
            .is_ok()
        {
            self.sent_sizes.insert(session, (cols, rows));
        }
    }

    fn queue_protocol_intent_with_spawn_cwd(
        &mut self,
        command: RuntimeCommand,
        spawn_cwd: Option<String>,
    ) -> Result<(), WorkspaceProtocolErrorCode> {
        workspace_protocol_command_is_valid(&command)?;

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
            return Err(WorkspaceProtocolErrorCode::Busy);
        }

        if let RuntimeCommand::Resize {
            session: next_session,
            cols: next_cols,
            rows: next_rows,
        } = &command
            && let Some(WorkspaceProtocolIntent {
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
            return Ok(());
        }

        if let RuntimeCommand::WriteInput {
            session: next_session,
            bytes: next_bytes,
        } = &command
            && let Some(WorkspaceProtocolIntent {
                command:
                    RuntimeCommand::WriteInput {
                        session: queued_session,
                        bytes: queued_bytes,
                    },
                ..
            }) = self.protocol_intents.back_mut()
            && queued_session == next_session
        {
            let combined = queued_bytes
                .len()
                .checked_add(next_bytes.len())
                .ok_or(WorkspaceProtocolErrorCode::PayloadTooLarge)?;
            if combined > WORKSPACE_PROTOCOL_INPUT_MAX_BYTES {
                return Err(WorkspaceProtocolErrorCode::PayloadTooLarge);
            }
            queued_bytes.extend_from_slice(next_bytes);
            self.command_sent = true;
            return Ok(());
        }

        if self
            .protocol_intents
            .len()
            .saturating_add(self.protocol_inflight.len())
            >= WORKSPACE_PROTOCOL_CAP
        {
            return Err(WorkspaceProtocolErrorCode::Busy);
        }
        let (operation, generation) = self.next_protocol_operation();
        self.protocol_intents.push_back(WorkspaceProtocolIntent {
            operation,
            generation,
            command,
            spawn_cwd,
        });
        self.command_sent = true;
        Ok(())
    }

    /// Drains one validated protocol request for the composition root. A taken request occupies
    /// one of the same eight slots until an exact completion is applied, preventing a hidden
    /// in-flight backlog when a host adapter stalls.
    pub fn take_protocol_intent(&mut self) -> Option<WorkspaceProtocolIntent> {
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
            },
        );
        Some(intent)
    }

    /// Applies only the exact operation/generation currently in flight. Unknown, duplicate, or
    /// pre-wrap completions are discarded without mutating UI lifecycle state.
    pub fn complete_protocol(&mut self, completion: WorkspaceProtocolCompletion) {
        let Some(pending) = self
            .protocol_inflight
            .remove(&(completion.operation, completion.generation))
        else {
            return;
        };
        match completion.result {
            Ok(()) => {
                if pending.spawn {
                    debug_assert!(self.pending_spawn_cwds.len() < WORKSPACE_PROTOCOL_CAP);
                    self.pending_spawn_cwds
                        .push_back(PendingShellSpawn::Awaiting {
                            cwd: pending.spawn_cwd,
                        });
                }
            }
            Err(_) => {
                self.error_is_pressure = false;
                self.error = Some("terminal protocol delivery failed".to_owned());
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
                let Some(pending) = self.pending_paste.as_ref() else {
                    return;
                };
                if pending.operation != operation
                    || pending.generation != generation
                    || generation != self.io_generation
                    || pending.requested_at.elapsed() > PASTE_TASK_TTL
                {
                    return;
                }
                let pending = self.pending_paste.take().expect("exact pending checked");
                let payload = match result {
                    Ok(payload) => payload,
                    Err(code) => {
                        self.error_is_pressure = false;
                        self.error = Some(format!("terminal clipboard operation failed: {code:?}"));
                        return;
                    }
                };
                let (paths, text) = payload.into_parts();
                let text = text
                    .map(|value| terminal_text_paste_bytes(&value, pending.bracketed))
                    .or(pending.text_fallback);
                let bytes = clipboard_terminal_paste_bytes(
                    (!paths.is_empty()).then_some(paths.as_slice()),
                    text,
                    pending.shell_kind,
                    pending.bracketed,
                );
                if let Some(bytes) = bytes {
                    self.send(RuntimeCommand::WriteInput {
                        session: pending.session,
                        bytes,
                    });
                }
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
            self.error_is_pressure = false;
            self.error = Some("terminal clipboard input exceeded limit".to_owned());
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
            self.error_is_pressure = false;
            self.error = Some("native operation queue is busy".to_owned());
            return;
        }
        self.pending_paste = Some(PendingPaste {
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
        if intent
            .and_then(|intent| self.queue_io_intent(intent))
            .is_err()
        {
            self.error_is_pressure = false;
            self.error = Some("native path operation rejected".to_owned());
        }
    }

    fn request_open_url(&mut self, url: &str) {
        let intent = WorkspaceUrlPayload::try_new(url.to_owned()).map(WorkspaceIoIntent::OpenUrl);
        if intent
            .and_then(|intent| self.queue_io_intent(intent))
            .is_err()
        {
            self.error_is_pressure = false;
            self.error = Some("native URL operation rejected".to_owned());
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
                    matches: Vec::new(),
                    total_lines: 0,
                    capped: false,
                    current: 0,
                    focus_input: true,
                    scroll_to_current: false,
                });
            }
        }
    }

    /// 검색 바를 닫고 원래 터미널로 포커스를 되돌린다 (T3).
    fn close_search(&mut self) {
        self.search = None;
        // 실제 refocus는 다음 프레임 render_pane에서 pending_focus로 소비된다.
        if let Some(pane) = self.mux.as_ref().and_then(|mux| mux.focused_pane.clone()) {
            self.begin_terminal_refocus(pane);
        }
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
    ) {
        // 이 pane의 세션에 대한 검색만 그린다.
        if self.search.as_ref().map(|s| s.session) != Some(session) {
            return;
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
                return;
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
            self.send(RuntimeCommand::Scroll { session, delta });
        }

        // 3) 우상단 검색 바.
        let (mut query, focus_input, match_count, current, capped) = {
            let Some(search) = self.search.as_ref() else {
                return;
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

        let bar_width = 280.0;
        let pos = egui::pos2(
            (term_rect.right() - bar_width - 8.0).max(term_rect.left() + 4.0),
            term_rect.top() + 8.0,
        );
        egui::Area::new(egui::Id::new(("terminal_search", session)))
            .order(egui::Order::Foreground)
            .fixed_pos(pos)
            .constrain_to(term_rect)
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_width(bar_width);
                    ui.horizontal(|ui| {
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut query)
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
                        // 키 입력은 입력창이 포커스일 때만 소비(터미널로 안 흘러감).
                        // Enter=다음, Shift+Enter=이전, Esc=닫기.
                        if resp.has_focus() {
                            let (enter, esc, shift) = ui.input(|i| {
                                (
                                    i.key_pressed(egui::Key::Enter),
                                    i.key_pressed(egui::Key::Escape),
                                    i.modifiers.shift,
                                )
                            });
                            if esc {
                                do_close = true;
                            } else if enter {
                                if shift {
                                    do_prev = true;
                                } else {
                                    do_next = true;
                                }
                            }
                        }
                    });
                });
            });

        // 4) 검색 바 조작 반영.
        {
            let Some(search) = self.search.as_mut() else {
                return;
            };
            search.focus_input = false;
            if query != search.query {
                search.query = query;
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
            return;
        }

        // 5) 쿼리가 바뀌었을 때만 재검색을 요청한다(매 프레임 금지).
        let pending = self.search.as_ref().and_then(|s| {
            (s.requested.as_deref() != Some(s.query.as_str())).then(|| (s.session, s.query.clone()))
        });
        if let Some((sess, q)) = pending {
            let empty = q.trim().is_empty();
            if let Some(s) = self.search.as_mut() {
                s.requested = Some(q.clone());
                if empty {
                    s.matches.clear();
                    s.total_lines = 0;
                    s.capped = false;
                    s.current = 0;
                }
            }
            if !empty {
                self.send(RuntimeCommand::SearchScrollback {
                    session: sess,
                    query: q,
                    max_matches: SEARCH_MAX_MATCHES,
                });
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

    /// 세션의 에이전트 요약 줄("Codex · gpt-5.6-sol · max")을 돌려준다 — 없으면 셸/미감지.
    /// 워크스페이스가 대기(warm)로 내려가도 이 맵은 마지막 감지값을 유지하므로(전환 시
    /// 안 지움), 활동 패널·PWA가 비활성 워크스페이스의 에이전트 정보를 보여줄 수 있다
    /// (2026-07-13 방안①). 절전되면 workspace_ui째로 사라져 자연히 표시 안 된다.
    pub fn agent_line_for(&self, session: SessionId) -> Option<String> {
        self.agent_info.get(&session).map(agent_info_line)
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
            return n.to_owned();
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
                    // 사라진 세션의 캐시 정리
                    let alive = mux_sessions(snapshot);
                    self.sessions.retain(|id, _| alive.contains(id));
                    self.sent_sizes.retain(|id, _| alive.contains(id));
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
                            view.render_cache.clear();
                        }
                    }
                    // 드래그 중 tab 전환/분할 구조 변경이면 미리보기가 다른 split에
                    // 잘못 적용될 수 있다 — 구조가 바뀌는 지점에서 정리 (codex 리뷰).
                    // 리사이즈 자신의 MuxUpdated는 drag_stopped 이후라 잃을 상태가 없다.
                    if self.mux.as_ref().map(|m| (&m.active_tab, &m.tabs))
                        != Some((&snapshot.active_tab, &snapshot.tabs))
                    {
                        self.split_drag = None;
                    }
                    self.mux = Some(Arc::clone(snapshot));
                }
                RuntimeEvent::Viewport {
                    session,
                    snapshot,
                    bracketed_paste,
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
                    if self.session_visible(*session) || attached_visible {
                        // 이 세션에 선택이 걸려 있으면 화면(snapshot)을 얼린다 — claude/codex
                        // 작업 중엔 화면이 매 프레임 갱신돼 예전엔 선택이 즉시 무효화됐다(#3).
                        // 좌표 기준 선택이라 그냥 유지만 하면 갱신된 화면의 '다른 텍스트'를
                        // 복사할 수 있어(codex), 선택 중엔 뷰를 정지시켜 선택·복사·표시가 항상
                        // 일치하게 한다(표준 터미널 동작). 클릭으로 선택 해제하면 최신으로 갱신.
                        let frozen = self.selection.is_some_and(|(s, _, _)| s == *session);
                        let view = self.sessions.entry(*session).or_default();
                        view.bracketed_paste = *bracketed_paste;
                        if frozen {
                            // 선택 중엔 표시 snapshot을 얼리되, 최신본은 pending에 보관해
                            // 해제 시 catch-up한다(codex — 안 그러면 화면이 선택 당시에 멈춤).
                            view.pending_snapshot = Some(Arc::clone(snapshot));
                        } else {
                            view.snapshot = Some(Arc::clone(snapshot));
                            view.snapshot_gen = view.snapshot_gen.wrapping_add(1);
                            view.pending_snapshot = None;
                            // 사이드바 세션 요약 — 마지막 비어있지 않은 행 (2026-07-05)
                            view.summary = last_line_summary(snapshot);
                        }
                    }
                }
                // SessionExited(런타임 종료) / SessionRestored(재시작 시 아카이브 복원,
                // PR-A2)는 UI 부기가 동일하다 — exit_code + 결과 상태 배지를 채운다.
                // 완료 알림 차이(복원은 재발화 안 함)는 process_ws_notifications 몫.
                RuntimeEvent::SessionExited { session, exit_code }
                | RuntimeEvent::SessionRestored { session, exit_code } => {
                    if self.session_alive(*session) {
                        let view = self.sessions.entry(*session).or_default();
                        view.exit_code = Some(*exit_code);
                        // 진행형 상태(⏳/✋)는 종료와 함께 무효. 결과 상태(✅/❌)는
                        // 유지하고, 없으면 exit code로 채운다 — 알림(on_exit)과
                        // tab 아이콘이 같은 결과를 보여주도록 (codex 리뷰 반영).
                        if !matches!(
                            view.status,
                            Some(SessionStatus::Done) | Some(SessionStatus::Error)
                        ) {
                            view.status = Some(if *exit_code == Some(0) {
                                SessionStatus::Done
                            } else {
                                SessionStatus::Error
                            });
                        }
                    }
                }
                RuntimeEvent::SpawnFailed { kind, message } => {
                    if *kind == SpawnKind::Shell {
                        self.resolve_pending_shell_spawn(None);
                    }
                    self.error_is_pressure = false;
                    self.error = Some(crate::ui::render_message(catalog, message));
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
                        // 압력 경고일 때만 배너를 걷는다 — spawn 실패 등 무관 오류 보존.
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
                    self.error_is_pressure = true;
                    self.error = Some(catalog.t(
                        "workspace.input_pressure",
                        &[
                            ("queued", &format_bytes(pressure.queued_bytes as u64)),
                            ("max", &format_bytes(pressure.max_bytes as u64)),
                        ],
                    ));
                }
                RuntimeEvent::ShellSpawned { session } => {
                    self.resolve_pending_shell_spawn(Some(*session));
                }
                // Launch correlation is app-owned lifecycle state (approval listener/runtime host),
                // not terminal rendering state. The app consumes this event before forwarding the
                // same batch here, so the workspace leaf intentionally performs no action.
                RuntimeEvent::AgentSpawned { .. } | RuntimeEvent::AgentSpawnResolved { .. } => {}
                RuntimeEvent::ResourceUsage { .. } => {}
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
            self.native_clipboard_paste_requested = native_key_downs.clipboard_paste;
            self.native_clipboard_copy_requested = native_key_downs.clipboard_copy;
        } else {
            self.native_printable_key_downs.clear();
            self.native_clipboard_paste_requested = false;
            self.native_clipboard_copy_requested = false;
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
                RuntimeEvent::MuxUpdated { snapshot } => {
                    let alive = mux_sessions(snapshot);
                    self.sessions.retain(|id, _| alive.contains(id));
                    self.mux = Some(Arc::clone(snapshot));
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
                RuntimeEvent::SessionExited { session, exit_code }
                | RuntimeEvent::SessionRestored { session, exit_code }
                    if self.session_alive(*session) =>
                {
                    let view = self.sessions.entry(*session).or_default();
                    view.exit_code = Some(*exit_code);
                    // handle_events와 같은 규칙 — 결과 상태(✅/❌)는 유지하고
                    // 없을 때만 exit code로 채운다.
                    if !matches!(
                        view.status,
                        Some(SessionStatus::Done) | Some(SessionStatus::Error)
                    ) {
                        view.status = Some(if *exit_code == Some(0) {
                            SessionStatus::Done
                        } else {
                            SessionStatus::Error
                        });
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
                Err(_) => {
                    self.pending_spawn_cwds.remove(index);
                    self.error_is_pressure = false;
                    self.error = Some("terminal spawn path delivery failed".to_owned());
                }
            }
        }
    }

    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        config: &TerminalConfig,
        events: &[RuntimeEvent],
        catalog: &i18n::Catalog,
    ) {
        let _ = self.show_with_input(ui, config, events, catalog, true);
    }

    pub fn show_with_input(
        &mut self,
        ui: &mut egui::Ui,
        config: &TerminalConfig,
        events: &[RuntimeEvent],
        catalog: &i18n::Catalog,
        input_enabled: bool,
    ) -> WorkspaceSurfaceOutput {
        self.prepare_frame(ui.ctx(), events, catalog, input_enabled);

        // 탭바 제거 (2026-07-05): 셸 전환은 좌측 사이드바 세션 목록이 담당하고,
        // 새 셸/분할/닫기는 각 pane 헤더가 담당한다 — 셸 수만큼 탭이 늘어나
        // 상단이 넘치던 문제 해소.
        // 에러 바가 있을 때만 pane과 분리하는 헤어라인을 둔다 — 평소엔 top_bar 하단
        // 헤어라인이 이미 구분선이라 여기 무조건 그리면 라인이 두 줄로 겹쳤다(#64 사용자).
        if let Some(error) = self.error.clone() {
            ui.horizontal(|ui| {
                ui.colored_label(ui.visuals().error_fg_color, error);
                if ui.small_button("×").clicked() {
                    self.error = None;
                }
            });
            crate::ui::hairline(ui);
        }

        let Some(mux) = self.mux.clone() else {
            let output = if input_enabled {
                self.show_new_session_prompt(ui, catalog);
                WorkspaceSurfaceOutput::default()
            } else {
                self.show_disabled_empty_surface(ui)
            };
            self.flush_command_repaint(ui.ctx());
            return output;
        };
        // mux 포커스가 바뀐 프레임: stale 조합/스크롤 잔여분 리셋 (세션 간 이월 방지)
        if self.last_focused_pane != mux.focused_pane {
            self.last_focused_pane = mux.focused_pane.clone();
            self.pending_focus = mux.focused_pane.clone();
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
            let output = if input_enabled {
                self.show_new_session_prompt(ui, catalog);
                WorkspaceSurfaceOutput::default()
            } else {
                self.show_disabled_empty_surface(ui)
            };
            self.flush_command_repaint(ui.ctx());
            return output;
        };

        // (출력/상태 폴링 제거 — 2026-07-04 상시 리페인트 원인 조사)
        // 예전엔 "가시+실행 세션 = 50ms 폴링"으로 출력을 끌어왔다(wake가 Viewport를
        // 깨우지 않던 시절의 안전망) → 가시 idle에서 20fps 리페인트로 CPU ~10%를 상시
        // 소모했다. 이제 worker의 wake가 Viewport(dirty 게이트)·상태 이벤트 모두를
        // 깨우므로 폴링이 불필요하다: 출력/상태가 있을 때만 프레임이 돈다.

        if input_enabled {
            self.close_confirm_dialog(ui.ctx(), catalog);
        }

        let rect = ui.available_rect_before_wrap();
        let layout = active_tab.layout.clone();
        let embedded_headers = keeps_embedded_pane_header(&layout);
        let tab_id = active_tab.id.clone();
        let mut split_path = Vec::new();
        let pane_output = self.render_node(
            ui,
            rect,
            &layout,
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
        ui.painter().hline(
            header.x_range(),
            crate::ui::snap_line_to_pixel(
                header.bottom(),
                crate::ui::designall::SEPARATOR_WIDTH,
                ui.ctx().pixels_per_point(),
            ),
            crate::ui::designall::separator_stroke(ui.visuals()),
        );
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
            response
                .dnd_release_payload::<crate::ui::cross_workspace::AttachmentId>()
                .map(|attachment_id| {
                    AttachedPaneReorder::new(*attachment_id, header_context.destination_index)
                })
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
        let mut title_job = egui::text::LayoutJob::single_section(
            attached_display_title.to_owned(),
            egui::TextFormat {
                font_id: egui::FontId::proportional(12.0),
                color: tokens.muted_text,
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
            tokens.muted_text,
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
                self.new_session_requested = true;
            }
        });
    }

    fn show_disabled_empty_surface(&self, ui: &mut egui::Ui) -> WorkspaceSurfaceOutput {
        let response = ui.interact(
            ui.available_rect_before_wrap(),
            ui.id().with("disabled_empty_workspace_surface"),
            egui::Sense::click(),
        );
        WorkspaceSurfaceOutput {
            focus_requested: response.clicked(),
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
    ) -> bool {
        let mut focus_requested = false;
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
        let title_left = header.left() + 17.0;

        // 우측 도구 4개를 모두 표시하던 기존 제목 폭을 기준으로 실제 글자 수를 구한 뒤
        // 10자를 더 허용한다. 추가 폭이 필요하면 기존 규칙대로 왼쪽 도구부터 숨긴다.
        let full_toolbar_width = PANE_HEADER_TOOLBAR_BUTTON * toolbar_icons.len() as f32
            + PANE_HEADER_TOOLBAR_GAP * toolbar_icons.len().saturating_sub(1) as f32;
        let original_title_width =
            (header.right() - 4.0 - full_toolbar_width - 24.0 - title_left).max(0.0);
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

        let buttons = pane_header_buttons(header, title_width, toolbar_icons.len());
        let center_y = header.center().y;
        let close = buttons.close;
        let tokens = crate::ui::designall::tokens(ui.visuals());
        let style = pane_header_style(self.workspace_accent, focused);
        ui.painter().rect_filled(header, 0.0, style.background);
        if let Some(selection_fill) = style.selection_fill {
            ui.painter().rect_filled(header, 0.0, selection_fill);
        }
        if let Some(active_stroke) = style.active_stroke {
            let ppp = ui.ctx().pixels_per_point();
            let painter = ui.painter();
            let boundary_x =
                painter.round_to_pixel_center(pane_header_active_boundary(header, close));
            // round_to_pixel_center는 문서가 밝히듯 **홀수 물리픽셀 폭**용이다. 이 선은
            // 1.0 **포인트**라 Retina에서 2 물리픽셀(짝수)이므로, 픽셀 중심에 맞추면
            // 양끝이 반 픽셀씩 걸쳐 뭉개지고 header.top()이 소수일 땐 헤더 첫 행이 아예
            // 비어 1px 여백으로 보인다(2026-08-07 사용자).
            //
            // 짝수 폭은 **경계**에 맞춰야 한다 — 헤더 상단을 픽셀 격자에 스냅한 뒤
            // half-width를 더하면 선이 첫 행부터 정확히 덮는다.
            let top_y = pane_header_top_line_y(header.top(), active_stroke.width, ppp);
            painter.hline(
                egui::Rangef::new(header.left(), boundary_x),
                top_y,
                active_stroke,
            );
        }
        ui.painter().hline(
            header.x_range(),
            crate::ui::snap_line_to_pixel(
                header.bottom(),
                crate::ui::designall::SEPARATOR_WIDTH,
                ui.ctx().pixels_per_point(),
            ),
            crate::ui::designall::separator_stroke(ui.visuals()),
        );

        let header_response = ui.interact(
            header,
            egui::Id::new(("terminal_pane_header", &pane.id)),
            egui::Sense::click(),
        );
        if header_response.clicked() {
            if input_enabled && !focused {
                self.request_pane_focus(pane.id.clone());
            } else if !input_enabled {
                focus_requested = true;
            }
        }
        if input_enabled {
            self.pane_context_menu(&header_response, &pane.id, config, catalog);
        }

        // 이 점이 뜻하는 건 "성공"이 아니라 **포커스됨**이다. 원래 tokens.success를 쓰고
        // 있었는데 토큰 이름과 의미가 어긋나 있어서, 나중에 누가 success 색을 조정하면
        // 엉뚱하게 이 점이 따라 바뀐다. 액센트가 앱 전체에서 "선택됨/포커스"를 뜻하므로
        // 그쪽으로 옮긴다(2026-08-07).
        let status_color = if focused {
            tokens.accent
        } else {
            tokens.muted_text
        };
        ui.painter()
            .circle_filled(egui::pos2(header.left() + 8.0, center_y), 4.0, status_color);

        let title_right = (close.left() - 3.0).max(title_left);
        let title_clip = egui::Rect::from_min_max(
            egui::pos2(title_left, header.top()),
            egui::pos2(title_right, header.bottom()),
        );
        let title_color = if focused {
            tokens.text
        } else {
            tokens.muted_text
        };
        let mut title_job = egui::text::LayoutJob::single_section(
            title,
            egui::TextFormat {
                font_id: font,
                color: title_color,
                ..Default::default()
            },
        );
        title_job.wrap = egui::text::TextWrapping {
            max_width: title_clip.width().max(0.0),
            max_rows: 1,
            break_anywhere: true,
            overflow_character: Some('…'),
        };
        let title_galley = ui.painter().layout_job(title_job);
        ui.painter().with_clip_rect(title_clip).galley(
            egui::pos2(title_clip.left(), center_y - title_galley.size().y / 2.0),
            title_galley,
            title_color,
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
        let d = 4.0;
        ui.painter().line_segment(
            [
                close.center() + egui::vec2(-d, -d),
                close.center() + egui::vec2(d, d),
            ],
            egui::Stroke::new(1.5, close_color),
        );
        ui.painter().line_segment(
            [
                close.center() + egui::vec2(-d, d),
                close.center() + egui::vec2(d, -d),
            ],
            egui::Stroke::new(1.5, close_color),
        );
        let close_clicked = close_response
            .on_hover_text(catalog.t("workspace.close_pane", &[]))
            .clicked();
        if close_clicked {
            if input_enabled {
                self.request_close_pane(pane.id.clone());
            } else {
                focus_requested = true;
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
                if input_enabled {
                    if !focused {
                        self.request_pane_focus(pane.id.clone());
                    }
                    self.activate_terminal_toolbar(icon, &pane.id, config);
                } else {
                    focus_requested = true;
                }
            }
        }
        focus_requested
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
            TerminalToolbarIcon::NewTerminal => self.new_session_requested = true,
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
        node: &LayoutNode,
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
                // 목업처럼 pane을 붙이고 1px 구분선만 둔다 (기존 4px 투명 gap 제거).
                // 리사이즈 잡기는 split_handle이 히트영역을 ±2px 확장해 보장한다.
                let gap = 1.0;
                // 드래그 중이면 로컬 미리보기 ratio 사용 (릴리즈 시에만 명령 전송)
                let ratio = match &self.split_drag {
                    Some((drag_path, preview)) if drag_path == path => *preview,
                    _ => *ratio,
                };
                let (first_rect, second_rect, gap_rect) = match direction {
                    SplitDirection::Horizontal => {
                        // 좌/우 분할
                        let split_x = rect.min.x + (rect.width() - gap) * ratio;
                        (
                            egui::Rect::from_min_max(rect.min, egui::pos2(split_x, rect.max.y)),
                            egui::Rect::from_min_max(
                                egui::pos2(split_x + gap, rect.min.y),
                                rect.max,
                            ),
                            egui::Rect::from_min_max(
                                egui::pos2(split_x, rect.min.y),
                                egui::pos2(split_x + gap, rect.max.y),
                            ),
                        )
                    }
                    SplitDirection::Vertical => {
                        // 상/하 분할
                        let split_y = rect.min.y + (rect.height() - gap) * ratio;
                        (
                            egui::Rect::from_min_max(rect.min, egui::pos2(rect.max.x, split_y)),
                            egui::Rect::from_min_max(
                                egui::pos2(rect.min.x, split_y + gap),
                                rect.max,
                            ),
                            egui::Rect::from_min_max(
                                egui::pos2(rect.min.x, split_y),
                                egui::pos2(rect.max.x, split_y + gap),
                            ),
                        )
                    }
                };
                path.push(0);
                let mut output = self.render_node(
                    ui,
                    first_rect,
                    first,
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
                    second,
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
                    self.split_handle(ui, rect, gap_rect, *direction, gap, tab_id, path);
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
        gap: f32,
        tab_id: &runtime::MuxTabId,
        path: &[u8],
    ) {
        // 4px 경계는 잡기 어려우니 히트 영역만 양쪽 2px씩 확장 (시각 폭은 그대로)
        let hit_rect = gap_rect.expand2(match direction {
            SplitDirection::Horizontal => egui::vec2(2.0, 0.0),
            SplitDirection::Vertical => egui::vec2(0.0, 2.0),
        });
        let id = egui::Id::new(("split_handle", tab_id, path));
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
        if resp.dragged()
            && let Some(pointer) = resp.interact_pointer_pos()
        {
            let ratio = match direction {
                SplitDirection::Horizontal => (pointer.x - rect.min.x) / (rect.width() - gap),
                SplitDirection::Vertical => (pointer.y - rect.min.y) / (rect.height() - gap),
            }
            .clamp(0.1, 0.9);
            self.split_drag = Some((path.to_vec(), ratio));
        }
        if resp.drag_stopped()
            && let Some((drag_path, ratio)) = self.split_drag.take()
            && drag_path == path
        {
            self.send(RuntimeCommand::ResizeSplit {
                tab: tab_id.clone(),
                path: drag_path,
                ratio,
            });
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render_pane(
        &mut self,
        ui: &mut egui::Ui,
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
        let pane_layout = terminal_pane_layout_with_embedded_header(ui.max_rect(), embedded_header);
        let pane_rect = pane_layout.surface;
        let tokens = crate::ui::designall::tokens(ui.visuals());
        ui.painter()
            .rect_filled(pane_rect, 0.0, tokens.app_background);
        if embedded_header && mode.is_local() {
            render_output.focus_requested |= self.render_pane_header(
                ui,
                pane_layout.header,
                pane,
                focused,
                config,
                catalog,
                input_enabled,
            );
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
            if mode.is_local() && input_enabled && !focused {
                self.request_pane_focus(pane_id.clone());
            } else if !input_enabled || !mode.is_local() {
                render_output.focus_requested = true;
            }
        }
        if mode.is_local() && input_enabled {
            self.pane_context_menu(&pane_resp, pane_id, config, catalog);
        }

        // 제목/닫기/검색/새 셸/분할은 각 leaf의 얇은 헤더에 있고, 본문은 그 아래를
        // 카드 외곽 여백 없이 채운다.
        if mode.is_local() && input_enabled && pane.session_id.is_some() {
            if pane_resp
                .dnd_hover_payload::<std::path::PathBuf>()
                .is_some()
                || pane_resp
                    .dnd_hover_payload::<TerminalTextDragPayload>()
                    .is_some()
            {
                ui.painter().rect_stroke(
                    pane_rect,
                    2.0,
                    egui::Stroke::new(1.5, ui.visuals().selection.stroke.color),
                    egui::StrokeKind::Inside,
                );
            }
            if let Some(session) = pane.session_id {
                if let Some(path) = release_typed_dnd_payload::<std::path::PathBuf>(&pane_resp) {
                    let bytes = path_insert_paste_bytes(
                        &path,
                        self.session_shell_kind(session),
                        self.session_bracketed_paste(session),
                    );
                    self.send(RuntimeCommand::WriteInput { session, bytes });
                }
                if let Some(text) = release_typed_dnd_payload::<TerminalTextDragPayload>(&pane_resp)
                {
                    let bytes = terminal_text_paste_bytes(
                        &text.text,
                        self.session_bracketed_paste(session),
                    );
                    self.send(RuntimeCommand::WriteInput { session, bytes });
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

        // 터미널 폰트는 UI 배율(zoom_factor)로 같이 커지므로 font_size를 배율로 역보정해
        // 물리 크기를 유지한다(UI만 스케일, 터미널 독립 — 2026-07-13). cell_size·draw가
        // 같은 값을 써야 격자/선택이 일치한다.
        let metrics = renderer_egui::CellMetrics {
            font_size: config.font_size / self.ui_scale,
            line_height: config.line_height,
        };
        // pane 크기 → cols/rows. visible pane 전부 대상 — split 직후 기존 pane의
        // PTY 크기가 틀어지는 문제 방지 (runtime도 visible 세션을 모두 push한다)
        let cell = renderer_egui::cell_size(ui.ctx(), metrics);
        let avail = ui.available_size();
        let cols =
            ((renderer_egui::grid_width_for_available(avail.x) / cell.x) as u16).clamp(10, 500);
        let rows = renderer_egui::grid_rows_for_available(avail.y, cell.y);
        self.queue_terminal_resize(session, cols, rows);

        let selected = self.selection.is_some_and(|(s, _, _)| s == session);
        let (exit_code, bracketed, snapshot) = {
            let view = self.sessions.entry(session).or_default();
            // 선택이 없으면(freeze 해제) freeze 중 보관한 최신본으로 catch-up한다 — 새
            // Viewport가 안 와도 화면이 선택 당시에 멈추지 않게(codex).
            if !selected && let Some(pending) = view.pending_snapshot.take() {
                view.summary = last_line_summary(&pending);
                view.snapshot = Some(pending);
                view.snapshot_gen = view.snapshot_gen.wrapping_add(1);
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
            (view.exit_code, view.bracketed_paste, snapshot)
        };

        // 런타임의 focused pane과 현재 native UI의 논리적 키보드 소유 상태를 draw 전에
        // 확정한다. renderer가 이 값을 바탕으로 같은 프레임에 egui 공식 IME 소유권까지
        // 동기화하므로, 기존 egui owner를 “요청할지”의 선행 조건으로 쓰지 않는다.
        let terminal_refocus_pending =
            mode.is_local() && self.pending_focus.as_ref() == Some(pane_id);
        let terminal_input_owner = input_enabled
            && if mode.is_local() {
                terminal_input_owner(pane_id, focused, self.pending_focus.as_ref())
            } else {
                true
            };
        let any_blocking_window_visible = ui.ctx().memory(|mem| {
            mem.areas()
                .visible_layer_ids()
                .iter()
                .any(is_blocking_terminal_window)
        });
        let terminal_keyboard_active = terminal_input_owner
            && terminal_keyboard_input_allowed(
                ui.ctx().text_edit_focused(),
                ui.ctx().any_popup_open(),
                any_blocking_window_visible,
                terminal_refocus_pending,
            );
        let preedit =
            (terminal_keyboard_active && !self.preedit.is_empty()).then_some(self.preedit.as_str());
        // 이 세션의 선택 영역 (정규화)
        let selection_range = self
            .selection
            .and_then(|(s, a, b)| (s == session).then_some((a.min(b), a.max(b))));
        let output = {
            let view = self.sessions.entry(session).or_default();
            renderer_egui::draw(
                ui,
                &snapshot,
                metrics,
                &mut view.render_cache,
                preedit,
                terminal_keyboard_active,
                selection_range,
                view.snapshot_gen,
            )
        };
        // B1 실측: 이 프레임에 그린 pane들의 렌더 비용을 합산한다 (visible pane 전부).
        self.frame_counters += output.counters;

        // 선택된 텍스트 위에서 시작한 드래그는 terminal-internal DnD payload가 된다.
        // 그 외의 마우스 드래그는 기존 셀 선택 동작을 유지한다.
        if input_enabled && !egui::DragAndDrop::has_any_payload(ui.ctx()) {
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
            if output.response.double_clicked()
                && let Some(pos) = output.response.interact_pointer_pos()
            {
                // 더블클릭 → 커서 아래 단어(공백 구분) 선택 (복사용). URL 열기는 단일
                // 클릭(위 hover/click 블록)으로 이동 — 여기서도 열면 이중 발화된다
                // (2026-07-17). 파일 열기는 우클릭 메뉴, 폴더 진입은 단일 클릭 담당.
                if let Some((s, e)) = word_range_at(&snapshot, cell_at(pos)) {
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
                let rate = drag_autoscroll_rate(pos.y, rect.top(), rect.bottom(), cell.y);
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
            && self.pending_focus.as_ref() == Some(pane_id)
        {
            self.pending_focus = None;
            request_terminal_focus(&output.response);
        }
        if terminal_primary_pointer_clicked(&output.response) {
            if !input_enabled || !mode.is_local() {
                render_output.focus_requested = true;
            }
            if input_enabled {
                self.terminal_focus_claimed = true;
                request_terminal_focus(&output.response);
                if mode.is_local() {
                    // 이미 runtime focus인 pane을 다시 클릭해도 stale TextEdit focus를 누르고
                    // 다음 keydown부터 터미널로 받도록 refocus를 예약한다.
                    if !terminal_refocus_pending {
                        self.begin_terminal_refocus(pane_id.clone());
                    }
                    // pane 배경이 같은 클릭을 먼저 받았다면 이미 FocusPane을 보냈다.
                    if !focused && !terminal_refocus_pending {
                        self.request_pane_focus(pane_id.clone());
                    }
                }
            }
        }

        // 파일 트리에서 드래그한 경로를 터미널 위에 드롭 → 입력으로 삽입 (2026-07-05).
        // hover 테두리는 위 pane 배경 경로가 pane_rect에 그린다.
        if mode.is_local()
            && input_enabled
            && let Some(path) = release_typed_dnd_payload::<std::path::PathBuf>(&output.response)
        {
            let bytes = path_insert_paste_bytes(&path, self.session_shell_kind(session), bracketed);
            self.send(RuntimeCommand::WriteInput { session, bytes });
            if mode.is_local() && !focused {
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
        if mode.is_local() && input_enabled {
            self.pane_context_menu(&output.response, pane_id, config, catalog);
        }

        // 터미널 텍스트 검색 (T3): 매치 하이라이트 + 우상단 검색 바 + 스크롤 이동.
        if mode.is_local() && input_enabled {
            self.render_terminal_search(
                ui,
                session,
                output.response.rect,
                output.origin,
                output.cell_size,
                &snapshot,
                catalog,
            );
        }

        // 검색 TextEdit/팝업 같은 overlay가 renderer 뒤에서 포커스를 가져갈 수도 있으므로
        // 이벤트를 소비하는 바로 이 시점에 egui의 공식 IME 소유권을 다시 확인한다.
        let terminal_owns_ime_events = ui
            .ctx()
            .memory(|memory| memory.owns_ime_events(output.response.id));
        let any_blocking_window_visible = ui.ctx().memory(|mem| {
            mem.areas()
                .visible_layer_ids()
                .iter()
                .any(is_blocking_terminal_window)
        });
        let terminal_accepts_ime_events = terminal_accepts_ime_events(
            terminal_keyboard_active,
            terminal_owns_ime_events,
            !self.preedit.is_empty(),
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
            let preedit_active_before_input = !self.preedit.is_empty();
            let native_key_downs = std::mem::take(&mut self.native_printable_key_downs);
            let native_clipboard_paste_requested =
                std::mem::take(&mut self.native_clipboard_paste_requested);
            let ime_reconciliation = ui.input(|input| {
                reconcile_ime_text_events(
                    &native_key_downs,
                    &input.raw.events,
                    preedit_active_before_input,
                )
            });
            let mut image_paste_trigger = (native_clipboard_paste_requested
                && !self.paste_suppressed)
                .then_some(ClipboardPasteTrigger::NativeKeyDown);
            let mut text_paste_bytes: Option<Vec<u8>> = None;
            ui.input(|input| {
                let modifiers = input.modifiers;
                for (event_index, event) in input.raw.events.iter().enumerate() {
                    if let egui::Event::Ime(egui::ImeEvent::Preedit { text, .. }) = event {
                        self.preedit = text.clone();
                        continue;
                    }
                    if let egui::Event::Ime(egui::ImeEvent::Commit(_)) = event {
                        self.preedit.clear();
                    }
                    if let Some(text) = ime_reconciliation.event_text[event_index].as_ref() {
                        pending.extend(text.as_bytes());
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
                    // Shift+Enter → 줄바꿈(LF). claude/codex는 \n을 입력 줄바꿈으로, \r을
                    // 제출로 구분한다(claude /terminal-setup 관례). Enter(\r)는 그대로 제출.
                    if let egui::Event::Key {
                        key: egui::Key::Enter,
                        pressed: true,
                        modifiers: m,
                        ..
                    } = event
                        && m.shift
                    {
                        pending.push(b'\n');
                        continue;
                    }
                    if let Some(bytes) = input_mapper::map_event(event, bracketed, &modifiers) {
                        pending.extend(bytes);
                    }
                }
            });
            pending.extend(&ime_reconciliation.fallback_bytes);
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
                pending.extend(bytes);
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
        if input_enabled && surface_focused && output.response.hovered() {
            let scroll_y = ui.input(|i| i.smooth_scroll_delta.y);
            self.scroll_residual += scroll_y / cell.y;
            let whole_rows = self.scroll_residual.trunc() as i32;
            if whole_rows != 0 {
                self.scroll_residual -= whole_rows as f32;
                self.send(RuntimeCommand::Scroll {
                    session,
                    delta: whole_rows,
                });
            }
        }

        if let Some(code) = exit_code {
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
    fn agent_send_targets(&self) -> Vec<(SessionId, String)> {
        self.mux
            .iter()
            .flat_map(|mux| mux.tabs.iter().flat_map(|tab| &tab.panes))
            .filter_map(|pane| {
                let session = pane.session_id?;
                let line = self.agent_line_for(session)?;
                Some((session, line))
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
        targets: &[(SessionId, String)],
        catalog: &i18n::Catalog,
    ) -> Option<(Vec<SessionId>, Option<String>)> {
        let mut choice: Option<(Vec<SessionId>, Option<String>)> = None;
        ui.menu_button(title, |ui| {
            for (session, agent_line) in targets {
                ui.label(egui::RichText::new(agent_line).small().weak());
                if ui
                    .button(catalog.t("workspace.menu.send_agent.raw", &[]))
                    .clicked()
                {
                    choice = Some((vec![*session], None));
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
                        choice = Some((vec![*session], Some(preset.clone())));
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
                choice = Some((targets.iter().map(|(session, _)| *session).collect(), None));
                ui.close();
            }
        });
        choice
    }

    /// 선택 본문을 대상 에이전트들에 주입하고 단일 대상이면 pane 포커스까지 옮긴다.
    fn dispatch_agent_prompt(&mut self, mut send_to: Vec<SessionId>, body: &str) {
        // 주입 시점에 대상을 재확인한다 — 메뉴가 열린(또는 추출 응답을 기다린) 사이 감지
        // tick(2.5s)이 에이전트를 제거했을 수 있다(stale 대상 오주입 방지, 2026-07-17
        // 리뷰 P3).
        send_to.retain(|session| self.agent_info.contains_key(session));
        for session in &send_to {
            self.send_agent_prompt(*session, body);
        }
        // 단일 대상이면 그 pane으로 포커스를 옮겨 Enter만 치면 되게 한다. **자동 전송은
        // 하지 않는다** — 보내기 전에 프롬프트를 다듬을 수 있어야 한다(확정 사항).
        // 여러 대상(브로드캐스트)은 포커스를 옮기지 않는다 — 어디로 갈지 정할 수 없고,
        // 사용자가 각 pane에서 직접 출발시키는 것이 비교 실행의 의도다.
        if let Some(session) = send_to.first().filter(|_| send_to.len() == 1)
            && let Some(pane) = self.pane_of_session(*session)
        {
            // request_pane_focus로 pending_focus까지 세팅한다 — FocusPane 직접 전송은
            // 스냅샷이 돌아올 때까지 terminal_input_owner가 이전 pane을 보므로 첫
            // 타이핑/Enter가 소스 pane에 들어갈 수 있다(리뷰 P2).
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

    /// 에이전트 pane 입력창에 텍스트를 주입한다(전송은 사용자 Enter). 여러 줄이 안전하게
    /// 한 덩어리로 들어가도록 붙여넣기 경로(bracketed paste)를 그대로 쓴다 — 개행이
    /// 즉시 전송으로 해석되지 않는다.
    ///
    /// bracketed paste가 **꺼진** 세션(감지가 ^Z 중단·백그라운드 에이전트를 아직 대상으로
    /// 보는 사이 셸 프롬프트로 돌아온 pane, bash 3.2 등)에는 개행을 공백으로 접어 한 줄로
    /// 보낸다 — raw 개행은 줄마다 즉시 명령으로 실행돼 선택문 안의 문장이 셸 명령이 될 수
    /// 있다(2026-07-17 리뷰 P1). claude/codex는 실행 중 bracketed paste를 켜므로 정상
    /// 대상에는 영향이 없다.
    fn send_agent_prompt(&mut self, session: SessionId, body: &str) {
        let bracketed = self.session_bracketed_paste(session);
        let folded;
        let body = if bracketed {
            body
        } else {
            folded = body.replace(['\r', '\n'], " ");
            folded.as_str()
        };
        let bytes = terminal_text_paste_bytes(body, bracketed);
        self.send(RuntimeCommand::WriteInput { session, bytes });
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

    /// 닫기 확인 다이얼로그 (request_close_pane이 세팅) — 실행 중 세션 종료 경고.
    fn close_confirm_dialog(&mut self, ctx: &egui::Context, catalog: &i18n::Catalog) {
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
        let mut open = true;
        egui::Window::new(catalog.t("workspace.close_confirm.title", &[]))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(catalog.t("workspace.close_confirm.body", &[]));
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button(catalog.t("action.close", &[])).clicked() {
                        self.send(RuntimeCommand::ClosePane { pane: pane.clone() });
                        self.confirm_close = None;
                    }
                    if ui.button(catalog.t("action.cancel", &[])).clicked() {
                        self.confirm_close = None;
                    }
                });
            });
        if !open {
            self.confirm_close = None;
        }
    }

    /// pane 우클릭 메뉴 — 분할/닫기 (2026-07-05, 선택한 pane 단위 제어).
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
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.to_string_lossy().into_owned());
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
                    // 선택 → 에이전트로 보내기 (2026-07-17 시나리오 ①): 에러 출력을
                    // 복사→pane 전환→붙여넣기→타이핑하던 흐름을 우클릭 두 번으로 줄인다.
                    // 대상은 **실행 중으로 감지된 에이전트 pane**(등록 목록이 아니라
                    // agent_info) — 없으면 이 메뉴 자체가 안 보인다.
                    self.send_to_agent_menu(ui, &text, catalog);
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
                self.send(RuntimeCommand::ScrollToBottom { session });
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
                self.open_environment_requested = true;
                ui.close();
            }
        });
    }

    /// pane 우클릭의 환경설정 진입 요청을 소비한다 (E4 ⑥ — App이 프레임마다 확인).
    pub fn take_open_environment(&mut self) -> bool {
        std::mem::take(&mut self.open_environment_requested)
    }

    pub fn take_new_session_requested(&mut self) -> bool {
        std::mem::take(&mut self.new_session_requested)
    }

    /// pane 우클릭의 세션 폴더 요청(트리 이동/Finder)을 소비한다 — App이 프레임마다
    /// 확인해 cwd 해석 후 라우팅한다(2026-07-18).
    pub fn take_session_folder_request(&mut self) -> Option<SessionFolderRequest> {
        self.session_folder_request.take()
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
                let merged = merge_agent_status(regex_status, activity, waiting, done, working);
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
                let osc = self.session_osc_title(pane.session_id);
                // 에이전트 정보(2/3행) — 있으면 3줄 렌더. codex/claude 병합본(App).
                let info = pane.session_id.and_then(|s| self.agent_info.get(&s));
                let (agent_line, status_label, status_line) = match info {
                    Some(d) => (
                        Some(agent_info_line(d)),
                        Some(session_status_label(status, catalog)),
                        Some(agent_activity_line(d, &summary, status, catalog)),
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
                    status,
                    summary,
                    focused: mux.focused_pane.as_ref() == Some(&pane.id),
                    attention: false, // App의 alert 추적이 채운다 (update_session_alerts)
                    pulse: None,
                    agent_line,
                    status_label,
                    status_line,
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
            self.error_is_pressure = false;
            self.error = Some("terminal spawn path rejected".to_owned());
            return;
        }
        if self
            .queue_protocol_intent_with_spawn_cwd(
                RuntimeCommand::SpawnShell {
                    cols: 80,
                    rows: 24,
                    scrollback_lines,
                },
                cwd,
            )
            .is_err()
        {
            self.error_is_pressure = false;
            self.error = Some("terminal protocol request rejected".to_owned());
        }
    }

    /// 단축키용 현재 pane 닫기. 실행 중인 세션은 마우스 ×와 동일하게 확인창을 거친다.
    pub fn close_focused_pane(&mut self) {
        if let Some(pane) = self.mux.as_ref().and_then(|mux| mux.focused_pane.clone()) {
            self.request_close_pane(pane);
        }
    }

    /// 단축키(⌘↓)용 포커스된 pane을 스크롤백 맨 아래로 되돌린다. close_focused_pane과
    /// 동일 구조 — 호출부(app crate의 단축키 처리부) 배선은 workspace.rs 밖이라 이 PR
    /// 범위 밖이다 (호출부가 없어 현재는 미사용).
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
            self.send(RuntimeCommand::ScrollToBottom { session });
        }
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
    fn begin_terminal_refocus(&mut self, pane: runtime::MuxPaneId) {
        self.pending_focus = Some(pane);
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
        if self.queue_protocol_intent(command).is_err() {
            self.error_is_pressure = false;
            self.error = Some("terminal protocol request rejected".to_owned());
            false
        } else {
            true
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

#[derive(Clone, Debug, PartialEq, Eq)]
struct TerminalTextDragPayload {
    text: String,
}

fn release_typed_dnd_payload<Payload>(response: &egui::Response) -> Option<Arc<Payload>>
where
    Payload: std::any::Any + Send + Sync,
{
    egui::DragAndDrop::has_payload_of_type::<Payload>(&response.ctx)
        .then(|| response.dnd_release_payload::<Payload>())
        .flatten()
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

/// 세션 행 2행: "[PTY] Codex · gpt-5.5 · xhigh · ctx 69%" (빈 부분은 생략).
fn agent_info_line(d: &crate::agent_detect::AgentDisplay) -> String {
    use crate::agent_surface::{AgentProvider, AgentTransport};

    let provider = AgentProvider::from(d.kind);
    let mut parts = vec![format!(
        "[{}] {}",
        AgentTransport::Pty.badge(),
        provider.label()
    )];
    if let Some(m) = d.model.as_deref().filter(|s| !s.is_empty()) {
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
            S::Waiting | S::NeedsApproval => "status.needs_approval",
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
    terminal_summary: &str,
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
    if !terminal_summary.trim().is_empty() {
        return terminal_summary.to_owned();
    }
    use runtime::SessionStatus as S;
    let key = match status {
        Some(S::Running) => "session.activity.running",
        Some(S::Waiting | S::NeedsApproval) => "session.activity.waiting",
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

fn terminal_primary_pointer_clicked(response: &egui::Response) -> bool {
    response.clicked_by(egui::PointerButton::Primary)
}

fn clipboard_terminal_paste_bytes(
    paths: Option<&[std::path::PathBuf]>,
    text_paste_bytes: Option<Vec<u8>>,
    shell_kind: crate::ui::file_tree::ShellKind,
    bracketed_paste: bool,
) -> Option<Vec<u8>> {
    if let Some(paths) = paths {
        Some(paths_insert_paste_bytes(paths, shell_kind, bracketed_paste))
    } else {
        text_paste_bytes
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
    let is_word = |c: usize| -> bool {
        snapshot.visible_cells.get(base + c).is_some_and(|cell| {
            // wide char(한글 등) 뒤의 자리 채움 셀은 c==' '지만 단어의 일부다 —
            // 공백으로 취급하면 "nant-성과분석.pdf"가 첫 한글에서 끊긴다 (2026-07-14).
            cell.wide_spacer || (!cell.c.is_whitespace() && cell.c != '\0')
        })
    };
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
) -> Option<runtime::SessionStatus> {
    use crate::agent_transcript::AgentActivity;
    use runtime::SessionStatus as S;
    // hook needsInput은 가장 신뢰도 높은 승인 대기 신호 — 단, transcript가 Working이면
    // 에이전트가 재개된 것이라(clear hook 지연 대비) 대기로 보지 않는다.
    if needs_input && activity != Some(AgentActivity::Working) {
        return Some(S::NeedsApproval);
    }
    // 명시적 오류는 완료보다 우선 — Stop은 모든 턴 종료에 오므로 turn_done이 error를
    // 가리면 실패한 턴이 '완료(바이올렛)'로 위장된다(codex 리뷰).
    if matches!(regex, Some(S::Error)) {
        return regex;
    }
    // Stop hook = 턴 완료. UserPromptSubmit/PreToolUse가 clear하므로 재개 시 즉시 해제.
    // transcript activity(Stop 직후 잠깐 Working으로 남음)보다 우선한다.
    if turn_done {
        return Some(S::Done);
    }
    match regex {
        // 대기(Waiting)는 입력대기(주황)로 통합 — 별도 팔레트 없음 (2026-07-07 결정).
        Some(S::Waiting) => return Some(S::NeedsApproval),
        Some(S::NeedsApproval | S::Done) => return regex,
        _ => {}
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

fn terminal_accepts_ime_events(
    terminal_keyboard_active: bool,
    owns_ime_events: bool,
    preedit_active: bool,
    text_edit_focused: bool,
    popup_open: bool,
    blocking_window_open: bool,
) -> bool {
    terminal_keyboard_active
        && !text_edit_focused
        && !popup_open
        && !blocking_window_open
        && (owns_ime_events || preedit_active)
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
fn is_blocking_terminal_window(layer: &egui::LayerId) -> bool {
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

#[derive(Debug, PartialEq, Eq)]
struct ImeTextReconciliation {
    /// raw event와 같은 길이. Text/Commit 위치에는 중복을 제거한 최종 문자열이 있고,
    /// 다른 이벤트 위치에는 None이 있다.
    event_text: Vec<Option<String>>,
    /// IME가 Text/Commit을 생략한 실제 key-down만 event batch 뒤에 보낸다.
    fallback_bytes: Vec<u8>,
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
) -> ImeTextReconciliation {
    let ime_involved = preedit_active
        || events.iter().any(|event| {
            matches!(event, egui::Event::Ime(egui::ImeEvent::Commit(_)))
                || matches!(
                    event,
                    egui::Event::Ime(egui::ImeEvent::Preedit { text, .. }) if !text.is_empty()
                )
        });

    let native_candidates: Vec<char> = key_downs
        .iter()
        .map(|key_down| key_down.character)
        .collect();
    let mut physical_candidates = native_candidates.clone();
    if ime_involved {
        // AppKit과 egui Key는 같은 key-down을 보는 두 경로다. 먼저 native 후보와
        // one-for-one으로 짝지어, native가 놓친 egui 후보만 원장에 추가한다.
        let mut unmatched_native = native_candidates;
        for character in events
            .iter()
            .filter_map(input_mapper::ime_terminator_key_char)
        {
            if !consume_ime_terminator_char(&mut unmatched_native, character) {
                physical_candidates.push(character);
            }
        }
    }

    let constrained_characters: HashSet<char> = physical_candidates.iter().copied().collect();
    let mut unclaimed_physical = physical_candidates;
    let mut event_text = Vec::with_capacity(events.len());

    for event in events {
        let text = match event {
            egui::Event::Text(text) | egui::Event::Ime(egui::ImeEvent::Commit(text)) => text,
            _ => {
                event_text.push(None);
                continue;
            }
        };

        let mut filtered = String::with_capacity(text.len());
        for character in text.chars() {
            if !constrained_characters.contains(&character) {
                filtered.push(character);
                continue;
            }
            let Some(position) = unclaimed_physical
                .iter()
                .position(|candidate| *candidate == character)
            else {
                // 이 물리 키의 허용 개수는 앞선 Commit/Text에서 이미 모두 전송됐다.
                continue;
            };
            if ime_involved {
                // 앞선 물리 키가 Text 없이 사라졌는데 뒤 키의 Text가 먼저 보인 경우,
                // 사라진 키를 여기 삽입해야 빠른 연타에서도 입력 순서가 뒤집히지 않는다.
                filtered.extend(unclaimed_physical.drain(..position));
                unclaimed_physical.remove(0);
            } else {
                unclaimed_physical.remove(position);
            }
            filtered.push(character);
        }
        event_text.push(Some(filtered));
    }

    let fallback_bytes = if ime_involved {
        unclaimed_physical
            .into_iter()
            .map(|character| character as u8)
            .collect()
    } else {
        Vec::new()
    };

    ImeTextReconciliation {
        event_text,
        fallback_bytes,
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
        let line: String = snapshot.visible_cells[row * cols..(row + 1) * cols]
            .iter()
            .filter(|c| !c.wide_spacer)
            .map(|c| c.c)
            .collect();
        let line = line.trim();
        if !line.is_empty() {
            return line.chars().take(48).collect();
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

#[cfg(test)]
mod tests {
    use super::*;
    use runtime::{MuxPaneId, MuxTabId, PaneSnapshot, TabSnapshot};
    use terminal::{CursorShape, CursorSnapshot, TerminalCell};

    /// hook "작업 중"(v32) 병합 우선순위: 확정 상태(승인대기/오류/완료/화면 대기)가
    /// 이기고, 그 외엔 hook working이 transcript(지연·활성 전용)보다 우선한다.
    #[test]
    fn merge_agent_status_hook_working_우선순위() {
        use crate::agent_transcript::AgentActivity;
        use runtime::SessionStatus as S;
        // hook working 단독 → Running (transcript 없음/유휴여도)
        assert_eq!(
            merge_agent_status(None, None, false, false, true),
            Some(S::Running)
        );
        assert_eq!(
            merge_agent_status(None, Some(AgentActivity::Idle), false, false, true),
            Some(S::Running)
        );
        // 확정 상태가 우선: needs_input / regex Error / turn_done / 화면 대기(regex)
        assert_eq!(
            merge_agent_status(None, None, true, false, true),
            Some(S::NeedsApproval)
        );
        assert_eq!(
            merge_agent_status(Some(S::Error), None, false, false, true),
            Some(S::Error)
        );
        assert_eq!(
            merge_agent_status(None, None, false, true, true),
            Some(S::Done)
        );
        assert_eq!(
            merge_agent_status(Some(S::Waiting), None, false, false, true),
            Some(S::NeedsApproval)
        );
        // hook working 없으면 기존과 동일(transcript 폴백)
        assert_eq!(
            merge_agent_status(None, Some(AgentActivity::Idle), false, false, false),
            Some(S::Idle)
        );
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

    #[test]
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

    /// codex 리뷰 P2 재현 가드 — compact 헤더(3e3e909)는 visible_toolbar를
    /// clamp(1,·)로 최소 1개 강제해, 589pt 픽스처의 10% pane(58.9px)에서
    /// SplitRows 버튼([30.9, 54.9])이 닫기(×) 히트박스 22px 중 17.1px([26, 48])를
    /// 덮었다. 도구 interact가 나중에 등록되므로 겹침 클릭은 닫기 대신 분할을
    /// 실행했다. 지금은 도구 0개 허용 + 겹침 시 왼쪽 도구 추가 숨김으로 닫기가
    /// 항상 우선한다.
    #[test]
    fn 지원되는_모든_좁은_split에서_도구가_닫기를_덮지_않는다() {
        // 589pt 픽스처의 지원 최소 split 비율 10%(resize clamp 0.1) — 58.9px pane.
        let narrow = egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(58.9, TERMINAL_PANE_HEADER_HEIGHT),
        );
        for title_width in [0.0_f32, 13.0, 26.0, 70.0, 130.0] {
            let buttons = pane_header_buttons(narrow, title_width, 4);
            assert!(
                buttons.toolbar.is_empty(),
                "59px pane은 도구 0개가 정상 (title_width {title_width})"
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
                let buttons = pane_header_buttons(header, title_width, 4);
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

        let _ = context.run_ui(egui::RawInput::default(), |ui| {
            measured = layout_pane_drop_feedback_label(
                ui.painter(),
                pane,
                "선택한 세션을 이 Pane의 오른쪽에 연결합니다",
                style,
            );
        });

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

        let _ = context.run_ui(egui::RawInput::default(), |ui| {
            measured = layout_pane_drop_feedback_label(
                ui.painter(),
                pane,
                "현재 화면 오른쪽에 열기",
                style,
            );
        });

        let measured = measured.expect("pane has room for the feedback label");
        assert!((measured.rect.center().x - pane.center().x).abs() < f32::EPSILON);
        assert!((measured.rect.center().y - pane.center().y).abs() < f32::EPSILON);
        assert_eq!(measured.galley.job.sections[0].format.font_id.size, 13.0);
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
        let output = context.run_ui(egui::RawInput::default(), |ui| {
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
        let output = context.run_ui(egui::RawInput::default(), |ui| {
            let header = egui::Rect::from_min_size(
                ui.available_rect_before_wrap().min,
                egui::vec2(header_width, TERMINAL_PANE_HEADER_HEIGHT),
            );
            title_right = Some(header.right() - 15.0 - 12.0 - 4.0);
            workspace.render_attached_pane_header(
                ui, header, &target, "Other", long_title, &catalog, None,
            );
        });
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

        let disabled_output = context.run_ui(egui::RawInput::default(), |ui| {
            workspace.prepare_frame_with_native_input(ui.ctx(), &[], &catalog, false, || {
                crate::native_key_monitor::NativeKeyDownBatch::default()
            });
        });

        assert_eq!(workspace.pending_copy.as_deref(), Some("owned copy"));
        assert!(disabled_output.platform_output.commands.is_empty());

        let enabled_output = context.run_ui(egui::RawInput::default(), |ui| {
            workspace.prepare_frame_with_native_input(ui.ctx(), &[], &catalog, true, || {
                crate::native_key_monitor::NativeKeyDownBatch::default()
            });
        });

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
        harness.get_by_label(&close_label).click();
        harness.run();

        assert!(harness.state().0.confirm_close.is_none());
        assert!(drain_protocol(&mut harness.state_mut().0).iter().any(
            |command| matches!(command, RuntimeCommand::ClosePane { pane } if pane == &target)
        ));
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
        assert!(!harness.state_mut().0.take_new_session_requested());
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
        assert!(!harness.state_mut().0.take_new_session_requested());
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
                .all(|command| matches!(command, RuntimeCommand::Resize { .. }))
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
        assert!(
            drain_protocol(harness.state_mut())
                .iter()
                .any(|command| matches!(command, RuntimeCommand::Resize { .. }))
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
        let commands = drain_protocol(&mut harness.state_mut().0);
        assert!(
            commands
                .iter()
                .all(|command| matches!(command, RuntimeCommand::Resize { .. }))
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
            move |ui, state: &mut (WorkspaceUi, WorkspaceSurfaceOutput)| {
                let frame = state.0.show_with_input(ui, &config, &[], &catalog, false);
                state.1.focus_requested |= frame.focus_requested;
            },
            (workspace, WorkspaceSurfaceOutput::default()),
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
        let commands = drain_protocol(&mut harness.state_mut().0);
        assert!(
            commands
                .iter()
                .all(|command| matches!(command, RuntimeCommand::Resize { .. }))
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
                workspace.show(ui, &config, &[], &catalog);
            },
            WorkspaceUi::new(),
        );
        harness.run();
        harness.get_by_label(&button_label).click();
        harness.run();

        assert!(harness.state_mut().take_new_session_requested());
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
        assert!(ui.take_new_session_requested());
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

    #[test]
    fn agent_info_line_distinguishes_pty_transport() {
        let display = crate::agent_detect::AgentDisplay {
            kind: crate::agent_detect::AgentKind::Codex,
            model: Some("gpt-test".to_owned()),
            effort: Some("high".to_owned()),
            context_pct: Some(69),
            last_agent_summary: Some("PR #124 코드 리뷰 완료".to_owned()),
        };

        assert_eq!(
            agent_info_line(&display),
            "[PTY] Codex · gpt-test · high · ctx 69%"
        );
        let catalog = catalog();
        assert_eq!(
            agent_activity_line(
                &display,
                "터미널 폴백",
                Some(SessionStatus::Running),
                &catalog
            ),
            "PR #124 코드 리뷰 완료"
        );
        assert_eq!(
            session_status_label(Some(SessionStatus::Idle), &catalog),
            "Idle"
        );
    }

    #[test]
    fn 한글_파일명은_wide_spacer를_넘어_한_단어로_잡힌다() {
        // "a nant-성과.pdf b" — 한글은 wide+spacer 2셀. 스페이서를 공백 취급하면
        // 단어가 첫 한글에서 끊긴다 (2026-07-14 "nant-성과분석.pdf 안 열림" 원인).
        fn push(cells: &mut Vec<TerminalCell>, c: char, wide: bool, spacer: bool) {
            cells.push(TerminalCell {
                c,
                fg: [255; 3],
                bg: [0; 3],
                wide,
                wide_spacer: spacer,
                attrs: Default::default(),
            });
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
            dirty_ranges: Vec::new(),
            title: None,
            scroll_offset: 0,
            is_alt_screen: false,
        };
        // '성'(idx 7) 위를 더블클릭 — 파일명 전체가 한 단어여야 한다
        let (s, e) = word_range_at(&snap, 7).expect("단어");
        assert_eq!(renderer_egui::selection_text(&snap, s, e), "nant-성과.pdf");
        // 공백(idx 1)은 여전히 단어가 아니다
        assert!(word_range_at(&snap, 1).is_none());
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

    fn snapshot(text: &str) -> Arc<TerminalViewportSnapshot> {
        let cols = 12;
        let rows = 2;
        let mut cells = Vec::with_capacity(cols * rows);
        let chars: Vec<char> = text.chars().collect();
        for idx in 0..cols * rows {
            cells.push(TerminalCell {
                c: chars.get(idx).copied().unwrap_or(' '),
                fg: [255, 255, 255],
                bg: [0, 0, 0],
                wide: false,
                wide_spacer: false,
                attrs: Default::default(),
            });
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
        ui.set_session_cwds(
            HashMap::from([(session, "/workspace/other".to_owned())]),
            crate::config::SessionNameStyle::Repo,
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
                bytes: vec![b'c'],
            }),
            Err(WorkspaceProtocolErrorCode::PayloadTooLarge)
        );
        let commands = drain_protocol(&mut ui);
        assert!(matches!(
            &commands[0],
            RuntimeCommand::WriteInput { bytes, .. }
                if bytes.len() == WORKSPACE_PROTOCOL_INPUT_MAX_BYTES
                    && bytes.last() == Some(&b'b')
        ));
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
            RuntimeCommand::Resize {
                session: queued_session,
                cols: 120,
                rows: 40,
            } if *queued_session == session
        )));
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

    /// kittest 재현 — 10% pane(58.9px) 헤더에서 닫기(×) 자리를 클릭하면 분할이
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
            egui::vec2(58.9, TERMINAL_PANE_HEADER_HEIGHT),
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
            clipboard_terminal_paste_bytes(Some(&paths), text, ShellKind::Posix, false),
            Some(paths_insert_paste_bytes(&paths, ShellKind::Posix, false))
        );
    }

    #[test]
    fn clipboard_terminal_paste는_file_list가_없으면_text_paste로_fallback한다() {
        use crate::ui::file_tree::ShellKind;

        let text = input_mapper::paste_bytes("plain text".as_bytes(), true);

        assert_eq!(
            clipboard_terminal_paste_bytes(None, Some(text.clone()), ShellKind::Posix, true),
            Some(text)
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
            true, false, true, false, false, false
        ));
        assert!(!terminal_accepts_ime_events(
            true, false, true, true, false, false
        ));
        assert!(!terminal_accepts_ime_events(
            true, false, true, false, true, false
        ));
        assert!(!terminal_accepts_ime_events(
            true, false, true, false, false, true
        ));
        assert!(!terminal_accepts_ime_events(
            true, true, true, true, false, false
        ));
        assert!(!terminal_accepts_ime_events(
            true, true, true, false, true, false
        ));
        assert!(!terminal_accepts_ime_events(
            true, true, true, false, false, true
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
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            let (_, response) =
                ui.allocate_exact_size(egui::vec2(120.0, 40.0), egui::Sense::click_and_drag());
            response.request_focus();
            response_id = Some(response.id);
        });
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
        let _ = ctx.run_ui(input, |ui| {
            let (_, response) =
                ui.allocate_exact_size(egui::vec2(120.0, 40.0), egui::Sense::click_and_drag());
            activation = Some((
                response.clicked(),
                terminal_primary_pointer_clicked(&response),
            ));
        });

        assert_eq!(activation, Some((true, false)));
    }

    #[test]
    fn agents_window는_terminal_refocus를_막는_modal_layer가_아니다() {
        let ctx = egui::Context::default();
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            egui::Window::new("Agents")
                .id(crate::ui::agent_sessions::agents_window_id())
                .show(ui.ctx(), |ui| {
                    ui.label("agent content");
                });
        });
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
    fn terminal_focus_claim은_한_frame에서_한번만_소비된다() {
        let mut workspace = WorkspaceUi::new();
        workspace.terminal_focus_claimed = true;

        assert!(workspace.take_terminal_focus_claimed());
        assert!(!workspace.take_terminal_focus_claimed());
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
        let reconciliation = reconcile_ime_text_events(native, events, preedit_active);
        let mut bytes = Vec::new();
        for text in reconciliation.event_text.into_iter().flatten() {
            bytes.extend(text.as_bytes());
        }
        bytes.extend(reconciliation.fallback_bytes);
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

    #[test]
    fn terminal_focus_lock_filter는_app_request_focus와_같은_filter를_쓴다() {
        let filter = renderer_egui::terminal_focus_lock_filter();
        assert!(filter.tab);
        assert!(filter.horizontal_arrows);
        assert!(filter.vertical_arrows);
        assert!(filter.escape);
    }
}
