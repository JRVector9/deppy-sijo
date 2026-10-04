//! 하단 도크 프롬프트 컴포저 (2026-07-17 사용자).
//!
//! 긴 프롬프트를 TUI 입력줄에 직접 칠 때의 마찰(한국어 IME + TUI 재그리기)을 없앤다.
//! ChatGPT Desktop 컴포저처럼 창 하단에 **레이아웃의 일부로 상주**한다 — 팝업/오버레이/
//! 플로팅 금지(사용자 명시). 워크스페이스당 드래프트 1개, 항상 표시.
//!
//! 두 상태: 비활성이면 한 줄 컴팩트 입력, 포커스(클릭) 또는 ⌘J면 여러 줄로 부드럽게
//! 확장(`animate_value_with_time` 높이 트윈). 전송 대상은 활성 워크스페이스의 포커스된
//! 세션이며 실제 주입(WriteInput)은 app.rs가 한다 — 이 모듈은 leaf UI 경계를 지켜
//! 런타임/저장소 구체 타입을 직접 만지지 않는다.

use super::text_input::{BoundedTextBuffer, forget_bounded_text_state, initialize_bounded_undo};
use crate::composer_drafts::{
    DRAFT_KEY_MAX_BYTES, DRAFT_MAX_ITEMS, DRAFT_METADATA_MAX_BYTES, DRAFT_TOTAL_MAX_BYTES,
    DRAFT_WORKSPACE_MAX_BYTES, DraftRecord, DraftSnapshot,
};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use unicode_normalization::UnicodeNormalization;

use crate::agent_surface::AgentProvider;
use crate::config::ComposerSendKey;
use connector_contract::{ConnectorSnapshot, ServerId, ServerSummary, ToolPage};

/// App-owned history persistence에 넘기는 snapshot 상한. Send 이벤트에서만 새 snapshot을
/// 만들고 stable frame에는 clone/allocation이 없다.
pub const COMPOSER_HISTORY_MAX_ITEMS: usize = 100;
pub const COMPOSER_HISTORY_MAX_BYTES: usize = 1024 * 1024;
pub const COMPOSER_PROMPT_MAX_BYTES: usize = 1024 * 1024;
const COMPOSER_DELIVERY_MAX_ITEMS: usize = 256;
const COMPOSER_DELIVERY_MAX_BYTES: usize = 8 * 1024 * 1024;
/// JSON string escaping은 한 input byte를 최악 6 bytes(`\u00XX`)로 확장한다.
pub const COMPOSER_HISTORY_FILE_MAX_BYTES: usize = COMPOSER_HISTORY_MAX_BYTES * 6 + 1024;
/// 펼침 상태 텍스트 영역 상한(줄) — 넘으면 내부 스크롤.
const MAX_TEXT_ROWS: usize = 8;
/// 펼침 상태 최소 줄 수 — 빈 버퍼여도 여러 줄 컴포저로 보이게.
const EXPANDED_MIN_ROWS: usize = 3;
/// 접힘↔펼침 높이 트윈 시간(초).
const ANIM_SECONDS: f32 = 0.16;

fn composer_frame(visuals: &egui::Visuals) -> egui::Frame {
    let tokens = crate::ui::designall::tokens(visuals);
    egui::Frame::NONE
        .fill(tokens.input_background)
        .stroke(crate::ui::designall::separator_stroke(visuals))
        .corner_radius(egui::CornerRadius::same(
            crate::ui::designall::INTERACTION_CORNER_RADIUS,
        ))
        .inner_margin(egui::Margin::symmetric(12, 10))
}

/// web-remote P6a `encode_input`과 동일한 붙여넣기 판정 임계값.
const INPUT_PASTE_THRESHOLD: usize = 512;

/// Connector overview/tool-page 상한과 맞춘 composer projection 한계. 이 leaf는 전체
/// 서버×도구 catalog를 materialize하지 않고 보이는 서버와 단일 page만 순회한다.
const MCP_SERVER_LIMIT: usize = 256;
const MCP_TOOL_PAGE_LIMIT: usize = 256;
const MCP_SERVER_ROW_HEIGHT: f32 = 26.0;
const MCP_SERVER_LIST_HEIGHT: f32 = MCP_SERVER_ROW_HEIGHT * 4.0;
const MCP_TOOL_ROW_HEIGHT: f32 = 28.0;
const MCP_TOOL_LIST_HEIGHT: f32 = MCP_TOOL_ROW_HEIGHT * 6.0;
const CONTEXT_FILE_PATH_MAX_BYTES: usize = 32 * 1024;
pub const COMPOSER_ATTACHMENT_MAX_ITEMS: usize = 16;
pub const COMPOSER_ATTACHMENT_MAX_BYTES: usize = 256 * 1024;

/// 모델 후보 — PTY 에이전트에는 외부 모델 제어 프로토콜이 없으므로 **슬래시 커맨드
/// 텍스트 삽입** 방식이다(사용자가 검토 후 전송). CLI가 받는 대표 이름의 하드코딩 목록.
const CLAUDE_MODELS: &[&str] = &["opus", "sonnet", "haiku"];
const CODEX_MODELS: &[&str] = &["gpt-5.5-codex", "gpt-5.5", "gpt-5.6-sol"];

/// 컴포저가 App에 돌려주는 액션. 실제 전송(인코딩 + WriteInput)은 App이 한다.
#[derive(Debug, Clone, PartialEq)]
pub enum ComposerAction {
    /// 전송과 bounded history snapshot 영속화를 App에 요청한다. Snapshot은 UI와 Arc로
    /// 공유하므로 action 생성 시 전체 history를 다시 clone하지 않는다.
    Send(ComposerSubmission),
    /// Connector service의 bounded tool page를 비동기로 요청한다. Leaf는 DB/service를
    /// 호출하지 않고 App composition root가 이 값을 Connector intent로 변환한다.
    RequestMcpToolPage { server_id: ServerId, offset: usize },
    /// Native picker/file access는 App host가 수행한다. Request는 root가 나중에 동일 값을
    /// `complete_context_file`로 돌려주는 bounded, non-sensitive continuation identity다.
    RequestContextFile(ContextFileRequest),
    /// Clipboard/image materialization은 App host가 수행한다. Leaf는 placeholder와
    /// latest-only continuation identity만 보관한다.
    RequestClipboardAttachment(ClipboardAttachmentRequest),
}

#[derive(Clone, PartialEq, Eq)]
pub struct ComposerSubmission {
    submission_id: u64,
    required_draft_revision: u64,
    prompt: std::sync::Arc<str>,
    history: std::sync::Arc<[std::sync::Arc<str>]>,
}

impl std::fmt::Debug for ComposerSubmission {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ComposerSubmission")
            .field("prompt_bytes", &self.prompt.len())
            .field("history_items", &self.history.len())
            .field("history_bytes", &history_bytes(&self.history))
            .finish()
    }
}

impl ComposerSubmission {
    pub(crate) fn required_draft_revision(&self) -> u64 {
        self.required_draft_revision
    }
    #[cfg(test)]
    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    #[cfg(test)]
    pub fn history(&self) -> &[std::sync::Arc<str>] {
        &self.history
    }

    pub fn into_parts(
        self,
    ) -> (
        std::sync::Arc<str>,
        std::sync::Arc<[std::sync::Arc<str>]>,
        u64,
    ) {
        (self.prompt, self.history, self.submission_id)
    }
}

/// 한 번의 composer context-file continuation. Clone/Debug 가능한 값은 request identity,
/// workspace/root와 placeholder coordinate뿐이며 선택된 raw path는 포함하지 않는다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextFileRequest {
    request_id: u64,
    target: AttachTarget,
}

/// Clipboard/image host continuation. Raw clipboard bytes/paths are deliberately absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardAttachmentRequest {
    request_id: u64,
    target: AttachTarget,
}

impl ClipboardAttachmentRequest {
    #[cfg(test)]
    fn request_id(&self) -> u64 {
        self.request_id
    }

    #[cfg(test)]
    fn token(&self) -> &str {
        &self.target.token
    }
}

/// App host worker가 raw clipboard/file 결과를 bounded continuation으로 축소한 뒤 UI로
/// 전달하는 payload. Debug는 raw path를 절대 노출하지 않는다.
pub struct ClipboardAttachmentPayload {
    paths: Vec<PathBuf>,
    total_bytes: usize,
}

impl std::fmt::Debug for ClipboardAttachmentPayload {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ClipboardAttachmentPayload")
            .field("items", &self.paths.len())
            .field("bytes", &self.total_bytes)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardAttachmentErrorCode {
    Empty,
    ResourceLimit,
}

impl ClipboardAttachmentPayload {
    pub fn try_new(paths: Vec<PathBuf>) -> Result<Self, ClipboardAttachmentErrorCode> {
        if paths.is_empty() {
            return Err(ClipboardAttachmentErrorCode::Empty);
        }
        let Some(total_bytes) = attachment_path_bytes(&paths) else {
            return Err(ClipboardAttachmentErrorCode::ResourceLimit);
        };
        Ok(Self { paths, total_bytes })
    }
}

impl ContextFileRequest {
    #[cfg(test)]
    fn request_id(&self) -> u64 {
        self.request_id
    }

    #[cfg(test)]
    fn workspace_id(&self) -> &str {
        &self.target.workspace_id
    }

    pub fn workspace_root(&self) -> Option<&Path> {
        self.target.workspace_root.as_deref()
    }

    #[cfg(test)]
    fn token(&self) -> &str {
        &self.target.token
    }
}

/// 렌더에 필요한 프레임 데이터 (App이 채워 넘긴다 — 경계상 평면 값만).
pub struct ComposerContext<'a> {
    pub workspace_id: &'a str,
    /// Stable persisted terminal draft identity; host workspace/root remain separate.
    pub draft_key: &'a str,
    pub runtime_generation: u64,
    pub send_key: ComposerSendKey,
    /// 포커스된 터미널 세션이 있는가 — 없으면 전송을 막고 자리표시로 안내한다.
    pub can_send: bool,
    /// 포커스 세션에서 감지된 에이전트 — 모델 후보 목록 선택. None이면 모델 버튼 숨김.
    pub agent: Option<crate::agent_surface::AgentProvider>,
    /// 활성 워크스페이스 루트 — 컨텍스트 파일 @멘션의 상대경로 기준.
    pub workspace_root: Option<&'a Path>,
    /// 포커스 중 접힘에 쓸 단축키 — FocusComposer의 **유효 바인딩**(App이 shortcuts
    /// 설정에서 조회). 하드코딩하면 리바인드 시 열기는 새 키, 닫기는 옛 키가 된다
    /// (codex P2). None(비활성)이면 접힘 단축키도 없다.
    pub collapse_shortcut: Option<egui::KeyboardShortcut>,
    /// App-owned latest-only Connector snapshot. Composer는 enabled server 요약과 정확히
    /// 한 tool page만 읽고, storage/service 구체 타입은 보지 않는다.
    pub connector_snapshot: &'a ConnectorSnapshot,
}

struct ToolbarOutput {
    send_clicked: bool,
    action: Option<ComposerAction>,
}

struct ToolbarInput<'a> {
    egui_ctx: &'a egui::Context,
    text_id: egui::Id,
    buffer: &'a mut String,
    may_emit_action: bool,
}

/// 첨부 태스크 시작 시점의 대상 스냅샷 — 완료가 워크스페이스 전환 **뒤에** 와도
/// 시작 시점의 드래프트에 삽입하기 위해 캡처한다(codex P2 — 엉뚱한 드래프트 오염 방지).
#[derive(Debug, Clone, PartialEq, Eq)]
struct AttachTarget {
    workspace_id: String,
    draft_key: String,
    runtime_generation: u64,
    workspace_root: Option<PathBuf>,
    /// 시작 시점 커서에 **동기로 삽입해 둔** 플레이스홀더 토큰. 완료 시 문자열 치환으로
    /// 결과가 들어가므로, 변환 중 사용자가 타이핑/이동/전환해도 토큰이 텍스트와 함께
    /// 밀려 항상 올바른 자리에 삽입된다 — 커서 인덱스 앵커는 그 사이 낡는다(codex P2).
    token: String,
    /// 삽입 시점에 insert_snippet이 **실제로 붙인** 패딩 공백(선행, 후행) — 실패/대체로
    /// 토큰을 걷을 때 정확히 이만큼만 걷는다. 인접 공백으로 추정하면 토큰이 사용자
    /// 공백/들여쓰기 옆일 때 사용자 공백 하나를 삼킨다(codex P2).
    padding: (bool, bool),
}

/// 하단 도크 컴포저 상태. App이 소유하고 매 프레임 `render`를 호출한다.
/// PTY queue admission only; it never represents AI execution or completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptAdmissionOutcome {
    Accepted,
    Rejected,
    Unknown,
}

#[derive(Clone)]
struct ComposerDelivery {
    submission_id: u64,
    generation: u64,
    workspace_id: String,
    prompt: std::sync::Arc<str>,
    outcome: Option<PromptAdmissionOutcome>,
    awaiting_checkpoint: bool,
}

pub struct ComposerUi {
    /// 워크스페이스별 드래프트 — 전환해도 초안이 유지된다.
    buffers: HashMap<String, String>,
    owners: HashMap<String, String>,
    uncertain_drafts: HashSet<String>,
    generations: HashMap<String, u64>,
    cached_drafts: HashMap<String, DraftRecord>,
    dirty_keys: HashSet<String>,
    draft_revision: u64,
    read_only: bool,
    input_limited: bool,
    delivery_limited: bool,
    active_workspace_id: String,
    active_generation: u64,
    deliveries: HashMap<String, ComposerDelivery>,
    submission_sequence: u64,
    /// 펼침 상태 — 포커스/⌘J로 열리고, ⌘J/바깥 클릭으로 접힌다.
    expanded: bool,
    /// ⌘J(전역 단축키) → 다음 렌더에서 펼침 + 포커스 요청.
    focus_requested: bool,
    /// 보낸 프롬프트(오래된 것 → 최신). 시작 시 파일에서 1회 로드하고, 전송 시에는
    /// App-owned persistence intent와 Arc snapshot만 만든다(leaf 파일 쓰기 없음).
    history: std::sync::Arc<[std::sync::Arc<str>]>,
    /// 히스토리 탐색 위치 — None이면 미탐색.
    history_pos: Option<usize>,
    /// 다음 show 후 커서를 이 문자 인덱스로 — recall/삽입은 TextEdit 상태 저장 뒤에
    /// 버퍼를 바꾸므로, 갱신하지 않으면 다음 타이핑이 삽입 텍스트 앞/안에 들어가
    /// 깨진다(codex P2).
    pending_cursor: Option<usize>,
    /// 접힘 단축키 소비 시 포커스 반납을 **다음 입력 프레임으로 지연** — 같은 프레임에
    /// 반납하면 뒤에 렌더되는 터미널 raw 루프(input.raw.events — consume_key가 안
    /// 닿는 별도 클론)가 접힘 키를 실행할 수 있다(codex P1 — 비-macOS Ctrl+J=LF;
    /// 983d213의 mac_cmd 가드가 같은 함정의 macOS 대응이다). 접힘 프레임엔 아직
    /// 컴포저가 포커스라 터미널이 입력을 먹지 않고, 다음 프레임엔 그 키가 지나갔다.
    /// (대상 TextEdit Id, 접힘 프레임의 input.time, 접힘 코드의 논리 키) — egui
    /// multi-pass(request_discard)는 같은 입력 프레임을 재패스하므로 "다음 render 호출"이
    /// 아니라 **input.time 증가**로 새 입력 프레임을 판정해야 한다(재패스에서 반납하면
    /// raw 키가 아직 살아 있다). 논리 키는 **릴리스 대기**용: 코드를 누른 채면 반납 후의
    /// key-repeat이 터미널 raw 경로에서 실행된다(비-macOS Ctrl+J 홀드 = LF 연발 —
    /// macOS는 mac_cmd 가드가 전부 막는다, codex P2). 포커스를 쥔 동안은 터미널이
    /// 입력을 안 먹으므로 repeat도 안전하다.
    pending_surrender: Option<(egui::Id, f64, egui::Key)>,
    /// 마지막으로 렌더한 워크스페이스 — 전환 감지용(히스토리 탐색 리셋, codex P3).
    last_workspace: Option<String>,
    /// Clipboard/image host continuation. Worker/channel/filesystem은 App이 소유하며 leaf는
    /// latest request 하나만 보관한다.
    pending_attachment: Option<ClipboardAttachmentRequest>,
    /// 첨부 플레이스홀더 토큰 단조 카운터 — 대체된 옛 태스크의 토큰과 구별한다.
    attach_seq: u64,
    /// **비활성** 워크스페이스 드래프트의 토큰 치환이 예약한 캐럿/선택 보정(워크스페이스
    /// → 리베이스된 (primary, secondary) 문자 인덱스 — 선택 방향 보존, codex P2).
    /// TextEditState는 위젯 단위 상태라 전환으로 그 워크스페이스가 다시 활성이 되는
    /// 시점(sync_workspace)에 적용하는 것이 안전하다(codex P1 — 치환이 길이를 바꿔
    /// 캐럿이 경로 안에 박히는 문제).
    pending_caret: HashMap<String, (usize, usize)>,
    /// Composer 전용 로컬 선택. 실제 도구는 보관하지 않고 최신 Connector snapshot의
    /// matching ToolPage만 빌려 렌더한다.
    mcp_selected_server: Option<ServerId>,
    /// Native picker continuation은 정확히 하나만 보관한다. 새 요청은 옛 token을 먼저
    /// 제거하고 대체하므로 queue/RAM이 증가하지 않는다.
    pending_context_file: Option<ContextFileRequest>,
    context_file_seq: u64,
}

impl ComposerUi {
    /// 시작 시 1회 히스토리를 로드한다 — UI 프레임 경로에서 파일 IO 금지 원칙.
    pub fn new(history_path: PathBuf) -> Self {
        Self {
            buffers: HashMap::new(),
            owners: HashMap::new(),
            uncertain_drafts: HashSet::new(),
            generations: HashMap::new(),
            cached_drafts: HashMap::new(),
            dirty_keys: HashSet::new(),
            draft_revision: 0,
            read_only: false,
            input_limited: false,
            delivery_limited: false,
            active_workspace_id: String::new(),
            active_generation: 0,
            deliveries: HashMap::new(),
            submission_sequence: 0,
            expanded: false,
            focus_requested: false,
            history: load_history(&history_path).into(),
            history_pos: None,
            pending_cursor: None,
            pending_surrender: None,
            last_workspace: None,
            pending_attachment: None,
            attach_seq: 0,
            pending_caret: HashMap::new(),
            mcp_selected_server: None,
            pending_context_file: None,
            context_file_seq: 0,
        }
    }

    pub(crate) fn restore_drafts(&mut self, snapshot: DraftSnapshot) {
        if !self.buffers.is_empty()
            || !self.cached_drafts.is_empty()
            || snapshot.validate().is_err()
        {
            self.input_limited = true;
            return;
        }
        for draft in snapshot.drafts {
            if draft.delivery_uncertain {
                self.uncertain_drafts.insert(draft.key.clone());
            }
            self.buffers
                .insert(draft.key.clone(), draft.text.to_string());
            self.owners
                .insert(draft.key.clone(), draft.workspace_id.clone());
            self.cached_drafts.insert(draft.key.clone(), draft);
        }
    }
    pub(crate) fn draft_revision(&self) -> u64 {
        self.draft_revision
    }
    pub(crate) fn active_draft_key(&self) -> &str {
        self.last_workspace.as_deref().unwrap_or("")
    }
    pub(crate) fn set_read_only(&mut self, read_only: bool) {
        self.read_only = read_only;
        if read_only {
            self.focus_requested = false;
            self.pending_surrender = None;
        }
    }
    fn mark_dirty(&mut self, key: &str) {
        if self.buffers.get(key).is_none_or(|text| text.is_empty())
            && !self.uncertain_drafts.contains(key)
            && self.last_workspace.as_deref() != Some(key)
        {
            self.owners.remove(key);
        } else if !self.owners.contains_key(key) {
            let owner = if let Some(delivery) = self.deliveries.get(key) {
                delivery.workspace_id.clone()
            } else if self.last_workspace.as_deref() == Some(key)
                && !self.active_workspace_id.is_empty()
            {
                self.active_workspace_id.clone()
            } else {
                key.to_owned()
            };
            self.owners.insert(key.to_owned(), owner);
        }
        self.dirty_keys.insert(key.to_owned());
        self.draft_revision = self
            .draft_revision
            .checked_add(1)
            .expect("draft revision exhausted");
    }
    pub(crate) fn checkpoint(&mut self) -> Arc<DraftSnapshot> {
        for key in self.dirty_keys.drain() {
            if let Some(buffer) = self.buffers.get_mut(&key) {
                compact_draft_buffer(buffer);
            }
            let uncertain = self.uncertain_drafts.contains(&key);
            let text = self.buffers.get(&key).map(String::as_str).unwrap_or("");
            if text.is_empty() && !uncertain {
                self.buffers.remove(&key);
                self.cached_drafts.remove(&key);
                self.owners.remove(&key);
                if !self.deliveries.contains_key(&key) {
                    self.generations.remove(&key);
                }
                self.pending_caret.remove(&key);
                continue;
            }
            // Only owned asynchronous placeholders are omitted: they cannot finish after restart.
            let targets = [
                self.pending_attachment.as_ref().map(|r| &r.target),
                self.pending_context_file.as_ref().map(|r| &r.target),
            ];
            let mut body = None;
            for target in targets
                .into_iter()
                .flatten()
                .filter(|target| target.draft_key == key)
            {
                let current = body.as_deref().unwrap_or(text);
                if let Some((pattern, start)) =
                    find_token_pattern(current, &target.token, "", target.padding)
                {
                    let mut stripped = current.to_owned();
                    stripped.replace_range(start..start + pattern.len(), "");
                    body = Some(stripped);
                }
            }
            let text: Arc<str> = body.map_or_else(|| Arc::from(text), Arc::from);
            self.cached_drafts.insert(
                key.clone(),
                DraftRecord {
                    key: key.clone(),
                    workspace_id: self
                        .owners
                        .get(&key)
                        .cloned()
                        .unwrap_or_else(|| key.clone()),
                    delivery_uncertain: uncertain,
                    text,
                },
            );
        }
        Arc::new(DraftSnapshot {
            drafts: self.cached_drafts.values().cloned().collect(),
        })
    }
    fn max_bytes_for(&self, key: &str) -> usize {
        if self.read_only || key.is_empty() || key.len() > DRAFT_KEY_MAX_BYTES {
            return self.buffers.get(key).map_or(0, String::len);
        }
        let owner = self.owners.get(key).map(String::as_str).unwrap_or_else(|| {
            if self.active_workspace_id.is_empty() {
                key
            } else {
                &self.active_workspace_id
            }
        });
        let metadata = self
            .owners
            .iter()
            .filter(|(stored, _)| stored.as_str() != key)
            .map(|(stored, owner)| stored.len() + owner.len())
            .sum::<usize>();
        if owner.is_empty()
            || owner.len() > DRAFT_WORKSPACE_MAX_BYTES
            || metadata + key.len() + owner.len() > DRAFT_METADATA_MAX_BYTES
            || (!self.owners.contains_key(key) && self.owners.len() >= DRAFT_MAX_ITEMS)
        {
            return 0;
        }
        let others = self
            .buffers
            .iter()
            .filter(|(stored, _)| stored.as_str() != key)
            .map(|(_, text)| text.len())
            .sum::<usize>();
        COMPOSER_PROMPT_MAX_BYTES.min(DRAFT_TOTAL_MAX_BYTES.saturating_sub(others))
    }
    fn insert_bounded(
        &mut self,
        buffer: &mut String,
        cursor: Option<usize>,
        text: &str,
    ) -> Option<InsertedSnippet> {
        let key = self.last_workspace.clone().unwrap_or_default();
        let max = if key.is_empty() {
            COMPOSER_PROMPT_MAX_BYTES
        } else {
            self.max_bytes_for(&key)
        };
        // Compute exact boundary padding before constructing a piece.
        let byte = cursor
            .and_then(|position| buffer.char_indices().nth(position).map(|(i, _)| i))
            .unwrap_or(buffer.len());
        let leading = buffer[..byte]
            .chars()
            .next_back()
            .is_some_and(|c| !c.is_whitespace());
        let trailing = buffer[byte..]
            .chars()
            .next()
            .is_some_and(|c| !c.is_whitespace());
        if self.read_only
            || buffer
                .len()
                .saturating_add(text.len())
                .saturating_add(usize::from(leading) + usize::from(trailing))
                > max
        {
            self.input_limited = true;
            return None;
        }
        let inserted = insert_snippet(buffer, cursor, text);
        if !key.is_empty() {
            self.mark_dirty(&key)
        }
        Some(inserted)
    }
    fn prepend_bounded_model(&mut self, buffer: &mut String, model: &str) -> bool {
        let key = self.last_workspace.clone().unwrap_or_default();
        let max = if key.is_empty() {
            COMPOSER_PROMPT_MAX_BYTES
        } else {
            self.max_bytes_for(&key)
        };
        if self.read_only || buffer.len().saturating_add(8).saturating_add(model.len()) > max {
            self.input_limited = true;
            return false;
        }
        prepend_model_command(buffer, model);
        if !key.is_empty() {
            self.mark_dirty(&key)
        }
        true
    }
    fn replace_bounded(&mut self, buffer: &mut String, text: &str) -> bool {
        let key = self.last_workspace.clone().unwrap_or_default();
        let max = if key.is_empty() {
            COMPOSER_PROMPT_MAX_BYTES
        } else {
            self.max_bytes_for(&key)
        };
        if self.read_only || text.len() > max {
            self.input_limited = true;
            return false;
        }
        if buffer != text {
            *buffer = text.to_owned();
            if !key.is_empty() {
                self.mark_dirty(&key)
            }
        }
        true
    }
    fn target_current(&self, target: &AttachTarget) -> bool {
        self.generations
            .get(&target.draft_key)
            .copied()
            .unwrap_or(0)
            == target.runtime_generation
    }
    fn bind_context(&mut self, egui_ctx: &egui::Context, ctx: &ComposerContext<'_>) {
        if self
            .generations
            .get(ctx.draft_key)
            .is_some_and(|generation| *generation != ctx.runtime_generation)
        {
            self.cancel_requests_for(egui_ctx, ctx.draft_key);
            if let Some(delivery) = self.deliveries.get_mut(ctx.draft_key)
                && delivery.outcome.is_none()
            {
                delivery.outcome = Some(PromptAdmissionOutcome::Unknown);
            }
        }
        self.active_workspace_id = ctx.workspace_id.to_owned();
        self.active_generation = ctx.runtime_generation;
        self.sync_workspace(egui_ctx, ctx.draft_key);
        if self
            .buffers
            .get(ctx.draft_key)
            .is_some_and(|text| !text.is_empty())
            || self.deliveries.contains_key(ctx.draft_key)
        {
            self.generations
                .insert(ctx.draft_key.to_owned(), ctx.runtime_generation);
            if !self.owners.contains_key(ctx.draft_key) && self.max_bytes_for(ctx.draft_key) > 0 {
                self.owners
                    .insert(ctx.draft_key.to_owned(), ctx.workspace_id.to_owned());
            }
        }
    }
    fn cancel_requests_for(&mut self, egui_ctx: &egui::Context, key: &str) {
        let mut targets = Vec::with_capacity(2);
        if self
            .pending_attachment
            .as_ref()
            .is_some_and(|r| r.target.draft_key == key)
        {
            targets.push(self.pending_attachment.take().unwrap().target)
        }
        if self
            .pending_context_file
            .as_ref()
            .is_some_and(|r| r.target.draft_key == key)
        {
            targets.push(self.pending_context_file.take().unwrap().target)
        }
        let active = self.last_workspace.clone().unwrap_or_default();
        for target in targets {
            let mut buffer = self.buffers.remove(&active).unwrap_or_default();
            self.resolve_attach(egui_ctx, &target, "", &active, &mut buffer);
            if !buffer.is_empty() || self.owners.contains_key(&active) {
                self.buffers.insert(active.clone(), buffer);
            }
        }
    }
    /// Runtime retirement cancels continuations but retains restorable text.
    pub(crate) fn retire_generation(&mut self, egui_ctx: &egui::Context, generation: u64) {
        let keys = self
            .generations
            .iter()
            .filter(|(_, value)| **value == generation)
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in keys {
            self.cancel_requests_for(egui_ctx, &key);
            if let Some(delivery) = self.deliveries.get_mut(&key)
                && delivery.outcome.is_none()
            {
                delivery.outcome = Some(PromptAdmissionOutcome::Unknown);
            }
            self.generations.remove(&key);
        }
    }
    /// Called only after an admitted explicit permanent close is absent from authoritative mux.
    pub(crate) fn delete_draft(&mut self, egui_ctx: &egui::Context, key: &str) {
        self.cancel_requests_for(egui_ctx, key);
        let existed = self.buffers.remove(key).is_some()
            || self.cached_drafts.contains_key(key)
            || self.uncertain_drafts.contains(key);
        self.uncertain_drafts.remove(key);
        self.deliveries.remove(key);
        self.owners.remove(key);
        self.generations.remove(key);
        self.pending_caret.remove(key);
        if existed {
            self.mark_dirty(key);
            self.owners.remove(key);
        }
    }
    pub(crate) fn delete_workspace_drafts(&mut self, egui_ctx: &egui::Context, workspace_id: &str) {
        let keys = self
            .owners
            .iter()
            .filter(|(_, owner)| owner.as_str() == workspace_id)
            .map(|(key, _)| key.clone())
            .chain(
                self.deliveries
                    .iter()
                    .filter(|(_, delivery)| delivery.workspace_id == workspace_id)
                    .map(|(key, _)| key.clone()),
            )
            .collect::<HashSet<_>>();
        for key in keys {
            self.delete_draft(egui_ctx, &key)
        }
    }

    /// ⌘J 전역 단축키(컴포저 비포커스일 때만 발화) — 펼침 + 포커스 요청.
    pub fn request_focus(&mut self) {
        self.focus_requested = true;
    }

    /// App host가 native picker를 완료한 뒤 결과를 돌려주는 continuation seam.
    ///
    /// 정확히 현재 pending request만 소비하므로 대체된 picker의 늦은 결과는 무시한다.
    /// 선택 경로는 이 호출 동안만 존재하고, bounded @mention으로 변환된 뒤 보관하지
    /// 않는다. 취소/과대 경로는 빈 치환으로 처리해 placeholder와 삽입 패딩을 걷는다.
    pub fn complete_context_file(
        &mut self,
        egui_ctx: &egui::Context,
        request: ContextFileRequest,
        selected_path: Option<PathBuf>,
        active_workspace: &str,
    ) -> bool {
        if self.pending_context_file.as_ref() != Some(&request)
            || !self.target_current(&request.target)
        {
            return false;
        }
        self.pending_context_file = None;

        let replacement = selected_path
            .filter(|path| path.as_os_str().as_encoded_bytes().len() <= CONTEXT_FILE_PATH_MAX_BYTES)
            .map_or_else(String::new, |path| {
                mention_path(request.target.workspace_root.as_deref(), &path)
            });
        if request.target.draft_key == active_workspace {
            let Some(mut active_buffer) = self.buffers.remove(active_workspace) else {
                return false;
            };
            let resolved = self.resolve_attach(
                egui_ctx,
                &request.target,
                &replacement,
                active_workspace,
                &mut active_buffer,
            );
            self.buffers
                .insert(active_workspace.to_owned(), active_buffer);
            resolved
        } else {
            let mut unused_active_buffer = String::new();
            self.resolve_attach(
                egui_ctx,
                &request.target,
                &replacement,
                active_workspace,
                &mut unused_active_buffer,
            )
        }
    }

    /// 활성 워크스페이스의 현재 컴포저 입력(없으면 빈 문자열). 프롬프트 라이브러리의
    /// "현재 내용 저장" 프리필용 읽기 전용 접근자.
    pub fn current_text(&self, active_workspace: &str) -> &str {
        self.buffers
            .get(active_workspace)
            .map(String::as_str)
            .unwrap_or("")
    }

    /// 프롬프트 라이브러리 팔레트가 고른 텍스트를 활성 워크스페이스의 컴포저 버퍼에
    /// 삽입한다(끝에 경계 공백 보정). 실제 전송은 기존 Send 경로(사용자 검토 후)가 한다.
    pub fn insert_text(&mut self, active_workspace: &str, text: &str) {
        if text.is_empty() {
            return;
        }
        let mut buffer = self.buffers.remove(active_workspace).unwrap_or_default();
        let max = self.max_bytes_for(active_workspace);
        if self.read_only
            || buffer
                .len()
                .saturating_add(text.len())
                .saturating_add(usize::from(
                    !buffer.is_empty() && !buffer.ends_with(char::is_whitespace),
                ))
                > max
        {
            self.input_limited = true;
            if !buffer.is_empty() {
                self.buffers.insert(active_workspace.to_owned(), buffer);
            }
            return;
        }
        let inserted = insert_snippet(&mut buffer, None, text);
        compact_draft_buffer(&mut buffer);
        self.buffers.insert(active_workspace.to_owned(), buffer);
        self.mark_dirty(active_workspace);
        self.pending_cursor = Some(inserted.cursor);
        self.request_focus();
    }

    /// App host가 clipboard/image 변환을 완료한 뒤 bounded path 결과를 돌려준다.
    /// 현재 latest request와 정확히 일치하지 않는 늦은 결과는 path materialization 전에
    /// 버린다. 취소/실패/limit 초과는 placeholder 제거로 fail-closed 처리한다.
    pub fn complete_clipboard_attachment(
        &mut self,
        egui_ctx: &egui::Context,
        request: ClipboardAttachmentRequest,
        payload: Option<ClipboardAttachmentPayload>,
        active_workspace: &str,
    ) -> bool {
        if self.pending_attachment.as_ref() != Some(&request)
            || !self.target_current(&request.target)
        {
            return false;
        }
        self.pending_attachment = None;
        let replacement = payload.map_or_else(String::new, |payload| {
            joined_mentions(request.target.workspace_root.as_deref(), &payload.paths)
        });
        if request.target.draft_key == active_workspace {
            let Some(mut active_buffer) = self.buffers.remove(active_workspace) else {
                return false;
            };
            let resolved = self.resolve_attach(
                egui_ctx,
                &request.target,
                &replacement,
                active_workspace,
                &mut active_buffer,
            );
            self.buffers
                .insert(active_workspace.to_owned(), active_buffer);
            resolved
        } else {
            let mut unused_active_buffer = String::new();
            self.resolve_attach(
                egui_ctx,
                &request.target,
                &replacement,
                active_workspace,
                &mut unused_active_buffer,
            )
        }
    }

    /// 컴포저 TextEdit의 egui Id — 워크스페이스별 상태(커서 등) 분리.
    fn text_id(workspace_id: &str) -> egui::Id {
        let _ = workspace_id;
        egui::Id::new("composer_text_active")
    }

    pub fn render(
        &mut self,
        ui: &mut egui::Ui,
        catalog: &i18n::Catalog,
        ctx: &ComposerContext<'_>,
    ) -> Option<ComposerAction> {
        self.bind_context(ui.ctx(), ctx);
        let mut buffer = self.buffers.remove(ctx.draft_key).unwrap_or_default();
        let action = self.render_inner(ui, catalog, ctx, &mut buffer);
        compact_draft_buffer(&mut buffer);
        if !buffer.is_empty() || self.owners.contains_key(ctx.draft_key) {
            self.buffers.insert(ctx.draft_key.to_owned(), buffer);
        }
        action
    }

    fn render_inner(
        &mut self,
        ui: &mut egui::Ui,
        catalog: &i18n::Catalog,
        ctx: &ComposerContext<'_>,
        buffer: &mut String,
    ) -> Option<ComposerAction> {
        let egui_ctx = ui.ctx().clone();
        let text_id = Self::text_id(ctx.draft_key);
        // 워크스페이스 전환 감지 — 히스토리 탐색 위치/커서 예약은 이전 워크스페이스
        // 문맥이므로 리셋한다(드래프트는 워크스페이스별 맵이 이미 분리 — codex P3).
        self.sync_workspace(&egui_ctx, ctx.draft_key);
        // 지연된 포커스 반납(접힘 단축키 — pending_surrender 필드 주석 참조). 새 입력
        // 프레임(time 증가) **이면서 접힘 키가 릴리스된 뒤**에만 반납한다 — 그때의
        // raw 이벤트에는 접힘 키(초타·repeat 모두)가 이미 없으므로 터미널로 새지 않는다.
        if let Some((id, collapsed_at, key)) = self.pending_surrender
            && egui_ctx.input(|input| input.time > collapsed_at && !input.keys_down.contains(&key))
        {
            self.pending_surrender = None;
            egui_ctx.memory_mut(|memory| memory.surrender_focus(id));
        }
        if self.read_only {
            egui_ctx.memory_mut(|memory| memory.surrender_focus(text_id));
        }
        // 직전 프레임의 포커스 — 이번 프레임 TextEdit이 그려지기 전이라 memory가 그 값이다.
        let had_focus = egui_ctx.memory(|memory| memory.has_focus(text_id));
        let focus_requested = std::mem::take(&mut self.focus_requested);
        if focus_requested {
            self.expanded = true;
        }

        // ── 키 가로채기 (TextEdit이 그려지기 전에 소비 여부를 판단) ──
        let mut send_requested = false;
        let mut action = None;
        if had_focus {
            let send_modifiers = match ctx.send_key {
                ComposerSendKey::Enter => egui::Modifiers::NONE,
                ComposerSendKey::CmdEnter => egui::Modifiers::COMMAND,
                ComposerSendKey::CtrlEnter => egui::Modifiers::CTRL,
            };
            // 접기 — FocusComposer의 유효 바인딩(리바인드/비활성 반영, codex P2).
            // 포커스 반납은 다음 프레임으로 지연한다(pending_surrender 필드 주석 —
            // 같은 프레임 반납은 터미널 raw 루프로 키가 샌다, codex P1).
            // 접힘 바인딩이 현재 **전송 코드와 같으면 전송이 우선**한다 — FocusComposer를
            // ⌘/Ctrl+Enter로 리바인드하면 접힘이 먼저 소비해 키보드 전송이 불가능해진다
            // (codex P2). 판정은 dispatcher와 같은 matches_exact 규칙(shadows_send_chord).
            let collapse_shortcut = ctx.collapse_shortcut.filter(|shortcut| {
                // 별칭 접기는 비-macOS만 — 물리 Ctrl이 두 플래그를 켜는 플랫폼 한정
                // (shadows_send_chord 주석 참조, codex P2 7차).
                !shadows_send_chord(shortcut, send_modifiers, cfg!(not(target_os = "macos")))
            });
            if let Some(shortcut) = collapse_shortcut
                && consume_key_exact(&egui_ctx, shortcut.modifiers, shortcut.logical_key)
            {
                self.expanded = false;
                self.pending_surrender = Some((
                    text_id,
                    egui_ctx.input(|input| input.time),
                    shortcut.logical_key,
                ));
            }
            // IME 가드: macOS 한글 IME의 조합 확정 Enter는 `Ime::Commit`으로 소비되고
            // "\n" commit은 egui가 무시한다(egui 0.35 + winit 백포트 — 리뷰로 검증).
            // 그래도 통합 밖 IME/이벤트 순서 변화로 Key(Enter)가 같은 프레임에 새어
            // 들어올 수 있으므로, 이번 프레임에 Ime 이벤트가 있으면 Enter를 전송으로
            // 치지 않는다 — 조합 확정이 프롬프트 오발사가 되지 않게.
            let ime_active =
                egui_ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Ime(_))));
            if !ime_active && consume_key_exact(&egui_ctx, send_modifiers, egui::Key::Enter) {
                send_requested = true;
            }
            self.intercept_history_keys(&egui_ctx, text_id, buffer);
            if action.is_none() {
                action = self.start_attach_if_requested(&egui_ctx, ctx, buffer);
            }
        }

        // ── 전송 판정 (빈 내용/세션 없음이면 무시 — Enter는 이미 소비돼 개행도 안 된다) ──
        // 키 경유 전송은 TextEdit을 그리기 전에 처리해 비워진 버퍼가 이번 프레임에 보인다.
        if send_requested && action.is_none() {
            action = self.try_submit(buffer, ctx.can_send, ctx.draft_key);
            send_requested = false;
        }

        let row_h = ui.text_style_height(&egui::TextStyle::Body);
        let line_count = buffer.split('\n').count().max(1);
        let target_rows = if self.expanded {
            line_count.clamp(EXPANDED_MIN_ROWS, MAX_TEXT_ROWS)
        } else {
            1
        };
        let text_h = egui_ctx.animate_value_with_time(
            text_id.with("height"),
            row_h * target_rows as f32,
            ANIM_SECONDS,
        );

        let hint = if !ctx.can_send {
            catalog.t("composer.no_session", &[])
        } else {
            catalog.t("composer.placeholder", &[])
        };
        // 개행 키 자동 보완: Enter 전송이면 Shift+Enter=개행, ⌘/Ctrl+Enter 전송이면
        // Enter=개행(기본 return_key 유지).
        let return_key = match ctx.send_key {
            ComposerSendKey::Enter => {
                egui::KeyboardShortcut::new(egui::Modifiers::SHIFT, egui::Key::Enter)
            }
            ComposerSendKey::CmdEnter | ComposerSendKey::CtrlEnter => {
                egui::KeyboardShortcut::new(egui::Modifiers::NONE, egui::Key::Enter)
            }
        };

        initialize_bounded_undo(&egui_ctx, text_id);
        let max_bytes = self.max_bytes_for(ctx.draft_key);
        let incoming = egui_ctx.input(|input| {
            input
                .events
                .iter()
                .map(|event| match event {
                    egui::Event::Text(text)
                    | egui::Event::Paste(text)
                    | egui::Event::Ime(egui::ImeEvent::Commit(text)) => text.len(),
                    egui::Event::Ime(egui::ImeEvent::Preedit { text, .. }) => text.len(),
                    egui::Event::Key {
                        key: egui::Key::Enter | egui::Key::Tab,
                        pressed: true,
                        ..
                    } => 1,
                    _ => 0,
                })
                .fold(0usize, usize::saturating_add)
        });
        let undo=egui_ctx.input(|input|input.events.iter().any(|event|matches!(event,egui::Event::Key{key:egui::Key::Z|egui::Key::Y,pressed:true,modifiers,..} if modifiers.command)));
        let may_receive = had_focus || egui_ctx.input(|input| input.pointer.any_pressed());
        let backup = (may_receive && (incoming > max_bytes.saturating_sub(buffer.len()) || undo))
            .then(|| buffer.clone());
        let backup_state = backup
            .as_ref()
            .and_then(|_| egui::TextEdit::load_state(&egui_ctx, text_id));
        let backup_undo = backup_state.as_ref().map(|state| state.undoer());
        let mut rejected = false;
        let card = composer_frame(ui.visuals());
        let card_response = card.show(ui, |ui| {
            if self.read_only {
                ui.disable();
            }
            let output = egui::ScrollArea::vertical()
                .id_salt(text_id.with("scroll"))
                .max_height(text_h)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    egui::TextEdit::multiline(&mut BoundedTextBuffer {
                        text: buffer,
                        max_bytes,
                        rejected: &mut rejected,
                    })
                    .interactive(!self.read_only)
                    .id(text_id)
                    // 카드가 이미 배경/테두리를 그린다 — TextEdit 자체 프레임은 투명.
                    .frame(egui::Frame::NONE)
                    .desired_width(f32::INFINITY)
                    .desired_rows(target_rows)
                    .hint_text(hint)
                    .return_key(Some(return_key))
                    .show(ui)
                })
                .inner;
            // Restored or blurred drafts still need an explicit send control and
            // its key/admission feedback; collapsing only reduces the editor.
            if self.expanded || !buffer.trim().is_empty() {
                ui.add_space(6.0);
                let toolbar = self.toolbar(
                    ui,
                    catalog,
                    ctx,
                    ToolbarInput {
                        egui_ctx: &egui_ctx,
                        text_id,
                        buffer,
                        may_emit_action: action.is_none(),
                    },
                );
                send_requested |= toolbar.send_clicked;
                if action.is_none() {
                    action = toolbar.action;
                }
            }
            output
        });
        let mut output = card_response.inner;
        if rejected {
            if let Some(backup) = backup {
                *buffer = backup;
            }
            if let Some(mut state) = backup_state {
                if let Some(undo) = backup_undo {
                    state.set_undoer(undo);
                }
                state.clone().store(&egui_ctx, text_id);
                output.state = state;
            }
            self.input_limited = true;
        } else if output.response.changed() {
            self.input_limited = false;
            self.mark_dirty(ctx.draft_key);
        }
        if self.delivery_limited {
            ui.label(
                egui::RichText::new(catalog.t("composer.draft.delivery_limit", &[]))
                    .color(ui.visuals().warn_fg_color)
                    .size(11.0),
            );
        }
        if self.input_limited {
            ui.label(
                egui::RichText::new(catalog.t("composer.draft.limit", &[]))
                    .color(ui.visuals().warn_fg_color)
                    .size(11.0),
            );
        }

        // recall/삽입 직후 커서를 예약된 위치로 — TextEdit이 상태를 저장한 **뒤에**
        // 버퍼를 바꾸는 경로(히스토리 recall·툴바/첨부 삽입)는 커서가 옛 위치(삽입
        // 텍스트 앞/안)에 남아 다음 타이핑이 그 안에 끼어든다(codex P2).
        if let Some(cursor) = self.pending_cursor.take() {
            let mut state = output.state.clone();
            state
                .cursor
                .set_char_range(Some(egui::text::CCursorRange::one(
                    egui::text::CCursor::new(cursor.min(buffer.chars().count())),
                )));
            state.store(&egui_ctx, text_id);
        }
        if focus_requested {
            output.response.request_focus();
        }
        // 접힘 직후 프레임엔 아직 포커스를 쥐고 있다(지연 반납) — 그걸 근거로
        // 다시 펼치면 접힘이 무효가 되므로 반납 대기 중엔 펼치지 않는다.
        if output.response.has_focus() && self.pending_surrender.is_none() {
            self.expanded = true;
        }
        if output.response.changed() {
            // 사용자가 직접 편집 — 히스토리 탐색 종료(recall 텍스트가 드래프트가 된다).
            self.history_pos = None;
        }

        // 전송 버튼 경유 요청 — 키 경유와 같은 규칙(빈 내용/세션 없음 무시).
        if send_requested && action.is_none() {
            action = self.try_submit(buffer, ctx.can_send, ctx.draft_key);
        }
        if action.is_some() {
            // 전송 후에도 포커스 유지 — 연속 프롬프트 작성(ChatGPT 관례).
            output.response.request_focus();
        }

        // 파일 드래그&드롭 — 도크 위에 놓인 OS 파일 드롭을 @멘션으로 삽입.
        self.accept_dropped_files(
            &egui_ctx,
            text_id,
            ui.min_rect(),
            ctx.workspace_root,
            buffer,
        );

        // 바깥 클릭 → 접힘 (팝업 메뉴 클릭은 제외 — 메뉴는 도크 밖에 그려질 수 있다).
        let clicked_outside = egui_ctx.input(|i| {
            i.pointer.any_pressed()
                && i.pointer
                    .interact_pos()
                    .is_some_and(|pos| !ui.min_rect().contains(pos))
        });
        if self.expanded && clicked_outside && !egui_ctx.any_popup_open() {
            self.expanded = false;
            if output.response.has_focus() {
                egui_ctx.memory_mut(|memory| memory.surrender_focus(text_id));
            }
        }
        // 애니메이션 중에는 매 프레임 다시 그린다.
        if (text_h - row_h * target_rows as f32).abs() > 0.5 {
            egui_ctx.request_repaint();
        }
        action
    }

    /// Stage an immutable submission while retaining the original draft. History and
    /// draft clearing settle only from the exact operation's actual PTY admission.
    pub(crate) fn try_submit(
        &mut self,
        buffer: &str,
        can_send: bool,
        workspace_id: &str,
    ) -> Option<ComposerAction> {
        if self.read_only
            || !can_send
            || self.submission_blocked(workspace_id)
            || buffer.trim().is_empty()
            || buffer.len() > COMPOSER_PROMPT_MAX_BYTES
            || buffer.len() > self.max_bytes_for(workspace_id)
            || self.attach_pending_in(workspace_id, buffer)
        {
            return None;
        }
        let owner = self
            .owners
            .get(workspace_id)
            .map(String::as_str)
            .unwrap_or_else(|| {
                if self.active_workspace_id.is_empty() {
                    workspace_id
                } else {
                    &self.active_workspace_id
                }
            });
        if workspace_id.is_empty()
            || workspace_id.len() > DRAFT_KEY_MAX_BYTES
            || owner.len() > DRAFT_WORKSPACE_MAX_BYTES
            || self
                .deliveries
                .iter()
                .filter(|(key, _)| key.as_str() != workspace_id)
                .map(|(key, delivery)| key.len() + delivery.workspace_id.len())
                .sum::<usize>()
                .saturating_add(workspace_id.len() + owner.len())
                > DRAFT_METADATA_MAX_BYTES
        {
            self.delivery_limited = true;
            return None;
        }
        let delivery_bytes = self
            .deliveries
            .iter()
            .filter(|(key, _)| key.as_str() != workspace_id)
            .map(|(_, delivery)| delivery.prompt.len())
            .sum::<usize>();
        if delivery_bytes.saturating_add(buffer.len()) > COMPOSER_DELIVERY_MAX_BYTES
            || (!self.deliveries.contains_key(workspace_id)
                && self.deliveries.len() >= COMPOSER_DELIVERY_MAX_ITEMS)
        {
            self.delivery_limited = true;
            return None;
        }
        self.delivery_limited = false;
        self.generations
            .insert(workspace_id.to_owned(), self.active_generation);
        self.submission_sequence = self.submission_sequence.checked_add(1)?;
        let submission_id = self.submission_sequence;
        // Retain the exact original until the runtime acknowledges its PTY reservation.
        let prompt: std::sync::Arc<str> = buffer.into();
        let mut history = self.history.iter().cloned().collect::<Vec<_>>();
        push_history(&mut history, std::sync::Arc::clone(&prompt));
        self.deliveries.insert(
            workspace_id.to_owned(),
            ComposerDelivery {
                submission_id,
                generation: self.active_generation,
                workspace_id: owner.to_owned(),
                prompt: std::sync::Arc::clone(&prompt),
                outcome: None,
                awaiting_checkpoint: true,
            },
        );
        self.uncertain_drafts.insert(workspace_id.to_owned());
        self.mark_dirty(workspace_id);
        Some(ComposerAction::Send(ComposerSubmission {
            submission_id,
            required_draft_revision: self.draft_revision,
            prompt,
            history: history.into(),
        }))
    }

    fn submission_blocked(&self, workspace_id: &str) -> bool {
        self.uncertain_drafts.contains(workspace_id)
            || self.deliveries.get(workspace_id).is_some_and(|delivery| {
                delivery.outcome.is_none()
                    || delivery.outcome == Some(PromptAdmissionOutcome::Unknown)
            })
    }

    /// A queued host checkpoint is valid only for this still-pending original generation.
    pub(crate) fn pending_submission_matches(
        &self,
        key: &str,
        submission_id: u64,
        prompt: &str,
        generation: u64,
    ) -> bool {
        self.uncertain_drafts.contains(key)
            && self.generations.get(key) == Some(&generation)
            && self.deliveries.get(key).is_some_and(|delivery| {
                delivery.submission_id == submission_id
                    && delivery.generation == generation
                    && delivery.prompt.as_ref() == prompt
                    && delivery.outcome.is_none()
            })
    }

    /// The host has not crossed the PTY queue boundary. An exact original-generation
    /// rejection may therefore clear uncertainty even after that runtime retired; it
    /// cannot settle a revived/replaced submission or erase the user's retained text.
    pub(crate) fn reject_unqueued_submission(
        &mut self,
        key: &str,
        submission_id: u64,
        prompt: &str,
        generation: u64,
    ) {
        if let Some(delivery) = self.deliveries.get_mut(key)
            && delivery.submission_id == submission_id
            && delivery.generation == generation
            && delivery.prompt.as_ref() == prompt
            && matches!(
                delivery.outcome,
                None | Some(PromptAdmissionOutcome::Unknown)
            )
        {
            delivery.outcome = Some(PromptAdmissionOutcome::Rejected);
            delivery.awaiting_checkpoint = false;
            self.uncertain_drafts.remove(key);
            self.mark_dirty(key);
        }
    }

    pub(crate) fn mark_submission_dispatched(
        &mut self,
        key: &str,
        submission_id: u64,
        prompt: &str,
        generation: u64,
    ) {
        if self.pending_submission_matches(key, submission_id, prompt, generation)
            && let Some(delivery) = self.deliveries.get_mut(key)
        {
            delivery.awaiting_checkpoint = false;
        }
    }

    /// Host rejection and runtime results settle only their original submitted snapshot.
    /// Edits made while awaiting the acknowledgement remain untouched.
    pub fn settle_submission(
        &mut self,
        workspace_id: &str,
        submission_id: u64,
        prompt: &str,
        outcome: PromptAdmissionOutcome,
    ) -> Option<std::sync::Arc<[std::sync::Arc<str>]>> {
        let delivery = self.deliveries.get_mut(workspace_id)?;
        if self.generations.get(workspace_id).copied().unwrap_or(0) != delivery.generation {
            return None;
        }
        if delivery.submission_id != submission_id || delivery.prompt.as_ref() != prompt {
            return None;
        }
        if outcome != PromptAdmissionOutcome::Accepted {
            delivery.outcome = Some(outcome);
            delivery.awaiting_checkpoint = false;
            if outcome == PromptAdmissionOutcome::Unknown {
                self.uncertain_drafts.insert(workspace_id.to_owned());
            } else {
                self.uncertain_drafts.remove(workspace_id);
            }
            self.mark_dirty(workspace_id);
            return None;
        }
        delivery.outcome = Some(PromptAdmissionOutcome::Accepted);
        delivery.awaiting_checkpoint = false;
        delivery.prompt = std::sync::Arc::from("");
        self.uncertain_drafts.remove(workspace_id);
        self.mark_dirty(workspace_id);
        if let Some(buffer) = self.buffers.get_mut(workspace_id)
            && buffer == prompt
        {
            buffer.clear();
            compact_draft_buffer(buffer);
        }
        let mut history = self.history.iter().cloned().collect::<Vec<_>>();
        push_history(&mut history, std::sync::Arc::from(prompt));
        self.history = history.into();
        self.history_pos = None;
        Some(std::sync::Arc::clone(&self.history))
    }

    fn acknowledge_uncertain_delivery(&mut self, key: &str) {
        if self
            .deliveries
            .get(key)
            .is_some_and(|delivery| delivery.outcome.is_none())
        {
            return;
        }
        if self.uncertain_drafts.remove(key) {
            if let Some(delivery) = self.deliveries.get_mut(key)
                && delivery.outcome == Some(PromptAdmissionOutcome::Unknown)
            {
                delivery.outcome = Some(PromptAdmissionOutcome::Rejected);
            }
            self.mark_dirty(key);
        }
    }

    /// 이 버퍼에 **진행 중 작업의 토큰**이 남아 있는가 — 이 상태로 전송하면 리터럴
    /// placeholder가 PTY로 간다(codex P1 — 전송 차단 조건).
    /// 정확한 판정(현재 태스크의 토큰 포함 여부)이라 사용자가 토큰을 지웠거나(취소)
    /// 히스토리 recall로 토큰 없는 버퍼가 되면 전송이 자연 허용된다.
    fn attach_pending_in(&self, workspace_id: &str, buffer: &str) -> bool {
        self.pending_attachment.as_ref().is_some_and(|request| {
            request.target.draft_key == workspace_id && buffer.contains(&request.target.token)
        }) || self.pending_context_file.as_ref().is_some_and(|request| {
            request.target.draft_key == workspace_id && buffer.contains(&request.target.token)
        })
    }

    /// ↑/↓ 히스토리 탐색 — 여러 줄 편집의 커서 이동과 충돌하지 않게 (사용자 확정 사양):
    /// 입력창이 비었거나 탐색 중일 때만, 그리고 커서가 첫 줄일 때 ↑ / 마지막 줄일 때 ↓만
    /// 히스토리로 소비한다. 그 외 화살표는 TextEdit이 평소처럼 커서를 움직인다.
    fn intercept_history_keys(
        &mut self,
        egui_ctx: &egui::Context,
        text_id: egui::Id,
        buffer: &mut String,
    ) {
        if self.history.is_empty() {
            return;
        }
        let cursor_chars = cursor_char_index(egui_ctx, text_id).unwrap_or(0);
        let (on_first_line, on_last_line) = cursor_line_info(buffer, cursor_chars);
        let browsing = self.history_pos.is_some();

        if (buffer.is_empty() || browsing)
            && on_first_line
            && consume_key_exact(egui_ctx, egui::Modifiers::NONE, egui::Key::ArrowUp)
        {
            let next = match self.history_pos {
                None => self.history.len() - 1,
                Some(0) => 0,
                Some(pos) => pos - 1,
            };
            self.history_pos = Some(next);
            let recalled = Arc::clone(&self.history[next]);
            if !self.replace_bounded(buffer, &recalled) {
                return;
            }
            self.pending_cursor = Some(buffer.chars().count());
        }
        if browsing
            && on_last_line
            && consume_key_exact(egui_ctx, egui::Modifiers::NONE, egui::Key::ArrowDown)
        {
            match self.history_pos {
                Some(pos) if pos + 1 < self.history.len() => {
                    self.history_pos = Some(pos + 1);
                    let recalled = Arc::clone(&self.history[pos + 1]);
                    if !self.replace_bounded(buffer, &recalled) {
                        return;
                    }
                    self.pending_cursor = Some(buffer.chars().count());
                }
                _ => {
                    // 최신을 지나면 빈 드래프트로 복귀 — 탐색 종료.
                    self.history_pos = None;
                    if !buffer.is_empty() {
                        buffer.clear();
                        let key = self.last_workspace.clone().unwrap_or_default();
                        self.mark_dirty(&key);
                    }
                }
            }
        }
    }

    /// 활성 워크스페이스 변경 추적 — 히스토리 탐색 위치와 커서 예약은 이전 워크스페이스
    /// 문맥이라 이어가면 첫 ↑가 남의 탐색 위치를 물려받는다(codex P3). 드래프트는
    /// 워크스페이스별 맵이 이미 분리하므로 리셋이 가장 단순하다.
    fn sync_workspace(&mut self, egui_ctx: &egui::Context, workspace_id: &str) {
        if self.last_workspace.as_deref() == Some(workspace_id) {
            return;
        }
        if let Some(old) = self.last_workspace.as_ref()
            && (self.owners.contains_key(old)
                || self.buffers.get(old).is_some_and(|text| !text.is_empty()))
            && let Some(caret) = stored_char_range(egui_ctx, Self::text_id(old))
        {
            self.pending_caret.insert(old.clone(), caret);
        }
        forget_bounded_text_state(egui_ctx, Self::text_id(workspace_id));
        initialize_bounded_undo(egui_ctx, Self::text_id(workspace_id));
        self.last_workspace = Some(workspace_id.to_owned());
        self.history_pos = None;
        self.pending_cursor = None;
        // 비활성 중 토큰 치환이 예약해 둔 캐럿/선택 보정 — 재활성 프레임 show **전에**
        // 즉시 반영해 이 프레임의 입력부터 올바른 위치를 쓰고, 선택 방향도 보존한다
        // (codex P1/P2 — pending_cursor 경유는 post-show라 늦고 단일 캐럿뿐).
        if let Some((primary, secondary)) = self.pending_caret.remove(workspace_id) {
            store_caret_range_now(egui_ctx, Self::text_id(workspace_id), primary, secondary);
        }
    }

    /// 이미지-only 클립보드 붙여넣기 감지 → App-owned materialization intent 반환.
    ///
    /// macOS: egui-winit이 ⌘V를 이벤트 없이 소비하므로(Event::Paste도 Key도 없음)
    /// AppKit native monitor의 기록으로만 감지된다 — 터미널(workspace)과 같은 원천.
    /// 컴포저가 포커스일 때만 drain하므로 터미널 흐름을 훔치지 않는다(터미널은
    /// text_edit_focused면 키보드 비활성이라 그 프레임 기록을 쓰지 않는다).
    /// 비-macOS: native monitor가 no-op라 터미널과 같은 Ctrl+Shift+V press 판정으로
    /// 폴백한다(codex P2 — is_attach_paste_shortcut 참조).
    fn start_attach_if_requested(
        &mut self,
        egui_ctx: &egui::Context,
        ctx: &ComposerContext<'_>,
        buffer: &mut String,
    ) -> Option<ComposerAction> {
        let native_paste = crate::native_key_monitor::drain().clipboard_paste;
        let shortcut_paste = egui_ctx.input(|i| i.events.iter().any(is_attach_paste_shortcut));
        if !(native_paste || shortcut_paste) {
            return None;
        }
        // 텍스트 paste(Event::Paste)가 같은 프레임에 있으면 TextEdit 기본 붙여넣기가
        // 처리한다 — 스크린샷 등 이미지/파일 클립보드만 백그라운드로 경로화한다.
        let has_text_paste =
            egui_ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Paste(_))));
        if has_text_paste {
            return None;
        }
        self.begin_attachment_request(egui_ctx, ctx.draft_key, ctx.workspace_root, buffer)
            .map(ComposerAction::RequestClipboardAttachment)
    }

    /// 첨부 continuation 등록 — 그 시점 커서에 플레이스홀더 토큰을 **동기로 삽입**하고
    /// request에는 (워크스페이스, 토큰)을 캡처한다. 완료는 토큰 문자열 치환이므로
    /// 변환 중 타이핑/전환에도 위치가 낡지 않는다(codex P2 — AttachTarget 주석).
    ///
    /// 진행 중이던 request는 **최신 것으로 대체**한다. 옛 토큰 제거를 **먼저** 하고
    /// 그 다음(제거로 시프트된) 캐럿을 읽어 새 토큰을 삽입한다 — 제거 전에 캡처한
    /// 커서 인덱스는 제거로 낡는다(codex P2).
    fn begin_attachment_request(
        &mut self,
        egui_ctx: &egui::Context,
        workspace_id: &str,
        workspace_root: Option<&Path>,
        buffer: &mut String,
    ) -> Option<ClipboardAttachmentRequest> {
        if self.read_only
            || workspace_id.is_empty()
            || workspace_id.len() > DRAFT_KEY_MAX_BYTES
            || workspace_root.is_some_and(|root| {
                root.as_os_str().as_encoded_bytes().len() > CONTEXT_FILE_PATH_MAX_BYTES
            })
        {
            return None;
        }
        if let Some(old) = self.pending_attachment.take() {
            let _ = self.resolve_attach(egui_ctx, &old.target, "", workspace_id, buffer);
        }
        self.attach_seq = self.attach_seq.wrapping_add(1).max(1);
        let token = attach_token(self.attach_seq);
        // 캐럿 소스로 쓴 예약은 소진한다(take) — 남겨두면 post-show에 옛 인덱스(토큰
        // 앞/안)가 지금 저장할 캐럿을 덮는다.
        let cursor = self
            .pending_cursor
            .take()
            .or_else(|| cursor_char_index(egui_ctx, Self::text_id(workspace_id)));
        let inserted = self.insert_bounded(buffer, cursor, &token)?;
        // 토큰 삽입 캐럿도 show **전에** 즉시 저장한다(resolve_attach와 동일 원칙 —
        // codex P2 7차, 마지막 남은 post-show 예약 자리였다). 붙여넣기 제스처와 Text
        // 이벤트가 한 입력 프레임에 배치되면 예약으로는 그 프레임의 텍스트가 옛 캐럿
        // (토큰 앞/안)에 들어가고 예약 인덱스도 토큰 안을 가리키게 된다.
        store_caret_now(egui_ctx, Self::text_id(workspace_id), inserted.cursor);
        let request = ClipboardAttachmentRequest {
            request_id: self.attach_seq,
            target: AttachTarget {
                workspace_id: if self.active_workspace_id.is_empty() {
                    workspace_id.to_owned()
                } else {
                    self.active_workspace_id.clone()
                },
                draft_key: workspace_id.to_owned(),
                runtime_generation: self
                    .generations
                    .get(workspace_id)
                    .copied()
                    .unwrap_or(self.active_generation),
                workspace_root: workspace_root.map(Path::to_path_buf),
                token,
                padding: (inserted.leading_space, inserted.trailing_space),
            },
        };
        self.generations
            .insert(workspace_id.to_owned(), request.target.runtime_generation);
        self.pending_attachment = Some(request.clone());
        Some(request)
    }

    /// Native picker를 시작하기 위한 순수 intent를 만든다. 파일 picker/thread/channel/I/O는
    /// 이 leaf에서 만들지 않는다. 이전 요청은 exact token/padding removal로 먼저 취소하고
    /// 새 요청 하나만 보관한다(latest-only backlog 1).
    fn begin_context_file_request(
        &mut self,
        egui_ctx: &egui::Context,
        workspace_id: &str,
        workspace_root: Option<&Path>,
        buffer: &mut String,
    ) -> Option<(ContextFileRequest, usize)> {
        if self.read_only
            || workspace_id.is_empty()
            || workspace_id.len() > DRAFT_KEY_MAX_BYTES
            || workspace_root.is_some_and(|root| {
                root.as_os_str().as_encoded_bytes().len() > CONTEXT_FILE_PATH_MAX_BYTES
            })
        {
            return None;
        }

        if let Some(old) = self.pending_context_file.take() {
            let _ = self.resolve_attach(egui_ctx, &old.target, "", workspace_id, buffer);
        }
        self.context_file_seq = self.context_file_seq.wrapping_add(1).max(1);
        let token = context_file_token(self.context_file_seq);
        let cursor = self
            .pending_cursor
            .take()
            .or_else(|| cursor_char_index(egui_ctx, Self::text_id(workspace_id)));
        let inserted = self.insert_bounded(buffer, cursor, &token)?;
        let request = ContextFileRequest {
            request_id: self.context_file_seq,
            target: AttachTarget {
                workspace_id: if self.active_workspace_id.is_empty() {
                    workspace_id.to_owned()
                } else {
                    self.active_workspace_id.clone()
                },
                draft_key: workspace_id.to_owned(),
                runtime_generation: self
                    .generations
                    .get(workspace_id)
                    .copied()
                    .unwrap_or(self.active_generation),
                workspace_root: workspace_root.map(Path::to_path_buf),
                token,
                padding: (inserted.leading_space, inserted.trailing_space),
            },
        };
        self.generations
            .insert(workspace_id.to_owned(), request.target.runtime_generation);
        self.pending_context_file = Some(request.clone());
        Some((request, inserted.cursor))
    }

    /// 첨부 종결 — **태스크 시작 시점의 워크스페이스** 드래프트에서 토큰을 결과로
    /// 치환한다(빈 문자열이면 제거 — insert_snippet이 붙인 인접 패딩 공백 하나까지
    /// 걷어 이중 공백을 남기지 않는다). 완료 전에 전환됐으면 활성 버퍼 대신 그
    /// 워크스페이스의 드래프트(맵)를 갱신한다(codex P2).
    /// **토큰이 없으면(사용자가 지움) 결과를 조용히 버린다 — 취소 의미론.**
    ///
    /// 치환은 길이를 바꾸므로 캐럿을 델타만큼 리베이스한다(codex P1) — 활성이면
    /// pending_cursor 예약으로, 비활성이면 pending_caret 맵에 저장했다가
    /// sync_workspace(재활성 시점)에서 적용한다.
    fn resolve_attach(
        &mut self,
        egui_ctx: &egui::Context,
        target: &AttachTarget,
        replacement: &str,
        active_workspace: &str,
        active_buffer: &mut String,
    ) -> bool {
        let is_active = target.draft_key == active_workspace;
        let max = self
            .max_bytes_for(&target.draft_key)
            .saturating_sub(if !is_active { active_buffer.len() } else { 0 });
        let current = if is_active {
            active_buffer.as_str()
        } else {
            self.buffers
                .get(&target.draft_key)
                .map(String::as_str)
                .unwrap_or("")
        };
        let overflow = find_token_pattern(current, &target.token, replacement, target.padding)
            .is_some_and(|(pattern, _)| {
                current
                    .len()
                    .saturating_sub(pattern.len())
                    .saturating_add(replacement.len())
                    > max
            });
        let replacement = if overflow {
            self.input_limited = true;
            ""
        } else {
            replacement
        };
        let replaced = {
            let draft = if is_active {
                &mut *active_buffer
            } else if let Some(draft) = self.buffers.get_mut(&target.draft_key) {
                draft
            } else {
                return false; // 드래프트 자체가 사라짐(워크스페이스 소멸 등) — 폐기
            };
            let Some((pattern, byte_start)) =
                find_token_pattern(draft, &target.token, replacement, target.padding)
            else {
                return false; // 사용자가 토큰을 지움 — 취소
            };
            let replace_start = draft[..byte_start].chars().count();
            let pattern_chars = pattern.chars().count();
            draft.replace_range(byte_start..byte_start + pattern.len(), replacement);
            compact_draft_buffer(draft);
            (replace_start, pattern_chars)
        };
        self.mark_dirty(&target.draft_key);
        let (replace_start, pattern_chars) = replaced;
        let replacement_chars = replacement.chars().count();
        let rebase = |endpoint: usize| {
            rebase_caret(endpoint, replace_start, pattern_chars, replacement_chars)
        };
        // 캐럿/선택 리베이스 — 대상 워크스페이스의 현재 상태를 읽어 델타 적용.
        let ws_text_id = Self::text_id(&target.draft_key);
        if is_active {
            // show **전에** TextEditState에 즉시 반영한다(codex P1) — 치환은 show 전에
            // 일어나므로 post-show 예약(pending_cursor)으로는 같은 프레임의 타이핑이
            // 낡은 캐럿으로 치환된 경로 안에 들어간다. post-show 예약은 툴바 삽입
            // (show 뒤 버퍼 변경) 전용으로 남긴다.
            if let Some(reserved) = self.pending_cursor.take() {
                // 예약(단일 캐럿)이 캐럿 소스 — 리베이스 값으로 즉시 저장하고 낡은
                // 예약이 post-show에 이를 덮지 않게 비운 상태를 유지한다.
                store_caret_now(egui_ctx, ws_text_id, rebase(reserved));
            } else if let Some((primary, secondary)) = stored_char_range(egui_ctx, ws_text_id) {
                // 선택은 양 끝점을 각각 리베이스해 **방향 그대로** 보존한다 — primary만
                // 남기면 토큰이 딴 곳이어도 사용자 선택이 풀린다(codex P2).
                store_caret_range_now(egui_ctx, ws_text_id, rebase(primary), rebase(secondary));
            }
        } else if let Some(&(primary, secondary)) = self.pending_caret.get(&target.draft_key) {
            self.pending_caret.insert(
                target.draft_key.clone(),
                (rebase(primary), rebase(secondary)),
            );
        }
        !overflow
    }

    /// OS 파일 드롭 — 포인터가 도크 위에 있을 때만 받아 @멘션으로 삽입한다.
    ///
    /// `i.pointer.latest_pos()`는 OS 드래그 중 갱신되지 않는다 — winit 0.30이 macOS
    /// `draggingUpdated:`를 구현하지 않아서다(`file_tree.rs`의 `os_drag_pointer_pos`
    /// 문서 참고). 그 결과 드래그 시작 전 마우스가 우연히 도크 위에 있었을 때만 드롭이
    /// 들어가고, 그 외엔 조용히 버려졌다(2026-08-14 사용자: "아예 안 들어가고 있어").
    /// file_tree.rs와 같은 패턴으로 AppKit 좌표를 우선 쓰고 실패(kittest 등)하면
    /// egui 포인터로 폴백한다.
    fn accept_dropped_files(
        &mut self,
        egui_ctx: &egui::Context,
        text_id: egui::Id,
        dock_rect: egui::Rect,
        workspace_root: Option<&Path>,
        buffer: &mut String,
    ) {
        let has_dropped = egui_ctx.input(|i| !i.raw.dropped_files.is_empty());
        if !has_dropped {
            return;
        }
        let drop_pos = crate::ui::file_tree::os_drag_pointer_pos(egui_ctx)
            .or_else(|| egui_ctx.input(|i| i.pointer.latest_pos()));
        if !drop_pos.is_some_and(|pos| dock_rect.contains(pos)) {
            return;
        }
        let dropped: Vec<PathBuf> = egui_ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|file| file.path().to_path_buf())
                .take(COMPOSER_ATTACHMENT_MAX_ITEMS + 1)
                .collect()
        });
        if attachment_path_bytes(&dropped).is_none() {
            return;
        }
        // 여러 파일은 합쳐 1회 삽입(순서 보존 — 첨부 완료 치환과 동일 규칙).
        let Some(inserted) =
            self.insert_bounded(buffer, None, &joined_mentions(workspace_root, &dropped))
        else {
            return;
        };
        // 드롭 처리는 post-show 캐럿 반영 **이후**에 돈다 — pending_cursor 예약은 다음
        // 프레임에나 적용돼 그 사이 타이핑이 낡은 캐럿을 쓰고 전환 시 유실된다(codex P2).
        // 첨부 완료(resolve_attach)와 같은 원칙으로 즉시 저장한다.
        store_caret_now(egui_ctx, text_id, inserted.cursor);
        self.expanded = true;
    }

    /// 컴포저 하단 툴바(펼침 상태 또는 미전송 초안) — 셀렉터 3종 + 전송. 셀렉터는 전부 "검토 가능한
    /// 텍스트 삽입"이다: PTY 에이전트에는 외부 제어 프로토콜이 없어 선택이 상태를 직접
    /// 바꿀 수 없고, 사용자가 삽입된 텍스트를 보고 전송한다. 반환: 전송 버튼 클릭.
    fn toolbar(
        &mut self,
        ui: &mut egui::Ui,
        catalog: &i18n::Catalog,
        ctx: &ComposerContext<'_>,
        input: ToolbarInput<'_>,
    ) -> ToolbarOutput {
        let ToolbarInput {
            egui_ctx,
            text_id,
            buffer,
            may_emit_action,
        } = input;
        let mut send_clicked = false;
        let mut action = None;
        // 삽입 후 커서 예약 — 팝업 클로저 안에서는 TextEditState를 바로 갱신하지 않고
        // 로컬에 모았다가 마지막에 반영한다.
        let mut inserted_cursor: Option<usize> = None;
        ui.horizontal(|ui| {
            // ① 컨텍스트 파일 — bounded placeholder + intent만 반환한다. Native picker와
            //    선택 경로는 App host가 소유하고 complete_context_file로 돌려준다.
            if ui
                .small_button("@")
                .on_hover_text(catalog.t("composer.context_hint", &[]))
                .clicked()
                && may_emit_action
                && let Some((request, cursor)) = self.begin_context_file_request(
                    egui_ctx,
                    ctx.draft_key,
                    ctx.workspace_root,
                    buffer,
                )
            {
                inserted_cursor = Some(cursor);
                action = Some(ComposerAction::RequestContextFile(request));
            }
            // ② 모델 — 감지된 에이전트의 후보를 `/model <이름>`으로 버퍼 맨 앞에 삽입.
            if let Some(agent) = ctx.agent {
                let models: &[&str] = match agent {
                    crate::agent_surface::AgentProvider::Claude => CLAUDE_MODELS,
                    crate::agent_surface::AgentProvider::Codex => CODEX_MODELS,
                    // Kimi·Grok의 `/model` 어휘를 실측하지 않았다. 추측 목록을 띄우면
                    // 고르는 순간 프롬프트로 흘러 턴을 태운다(Codex에서 실증된 사고).
                    crate::agent_surface::AgentProvider::Kimi
                    | crate::agent_surface::AgentProvider::Grok => &[],
                };
                let resp = ui
                    .add_enabled(!models.is_empty(), egui::Button::new("/model").small())
                    .on_hover_text(catalog.t("composer.model_hint", &[]));
                egui::Popup::menu(&resp).show(|ui| {
                    for model in models {
                        if ui.button(*model).clicked() && self.prepend_bounded_model(buffer, model)
                        {
                            inserted_cursor = Some(format!("/model {model}\n").chars().count());
                        }
                    }
                });
            }
            // ③ MCP 도구 — Connector latest-only snapshot에서 enabled server와 정확히 한
            // bounded page만 읽는다. 필요한 page는 intent로 반환해 App이 비동기 dispatch한다.
            if let Some(request) =
                self.mcp_picker(ui, catalog, ctx, text_id, buffer, &mut inserted_cursor)
                && action.is_none()
            {
                action = Some(request);
            }
            // 우측: 전송 버튼 + 전송 키 힌트. 첨부 변환 중(토큰 pending)엔 전송을 막고
            // 사유를 힌트로 보인다 — try_submit과 같은 조건(codex P1).
            let attach_pending = self.attach_pending_in(ctx.draft_key, buffer);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let enabled = ctx.can_send
                    && !buffer.trim().is_empty()
                    && !attach_pending
                    && !self.submission_blocked(ctx.draft_key);
                if ui
                    .add_enabled(enabled, egui::Button::new("↑").corner_radius(1))
                    .on_hover_text(catalog.t("composer.send", &[]))
                    .clicked()
                {
                    send_clicked = true;
                }
                if let Some(delivery) = self.deliveries.get_mut(ctx.draft_key) {
                    let key = match delivery.outcome {
                        None if delivery.awaiting_checkpoint => "composer.delivery.persisting",
                        None => "composer.delivery.pending",
                        Some(PromptAdmissionOutcome::Rejected) => "composer.delivery.rejected",
                        Some(PromptAdmissionOutcome::Unknown) => "composer.delivery.unknown",
                        Some(PromptAdmissionOutcome::Accepted) => "composer.delivery.accepted",
                    };
                    ui.label(egui::RichText::new(catalog.t(key, &[])).size(11.0).weak());
                    if delivery.outcome == Some(PromptAdmissionOutcome::Unknown)
                        && ui
                            .small_button(catalog.t("composer.delivery.allow_resend", &[]))
                            .clicked()
                    {
                        // Explicit acknowledgement only. Never resend automatically.
                        self.acknowledge_uncertain_delivery(ctx.draft_key);
                    }
                }
                if !self.deliveries.contains_key(ctx.draft_key)
                    && self.uncertain_drafts.contains(ctx.draft_key)
                {
                    ui.label(
                        egui::RichText::new(catalog.t("composer.delivery.unknown", &[]))
                            .size(11.0)
                            .weak(),
                    );
                    if ui
                        .small_button(catalog.t("composer.delivery.allow_resend", &[]))
                        .clicked()
                    {
                        self.acknowledge_uncertain_delivery(ctx.draft_key);
                    }
                }
                let hint_key = if attach_pending {
                    "composer.attach_pending"
                } else {
                    match ctx.send_key {
                        ComposerSendKey::Enter => "composer.hint.enter",
                        ComposerSendKey::CmdEnter => "composer.hint.cmd_enter",
                        ComposerSendKey::CtrlEnter => "composer.hint.ctrl_enter",
                    }
                };
                ui.add(egui::Label::new(
                    egui::RichText::new(catalog.t(hint_key, &[]))
                        .size(11.0)
                        .weak(),
                ));
            });
        });
        if inserted_cursor.is_some() {
            self.pending_cursor = inserted_cursor;
        }
        ToolbarOutput {
            send_clicked,
            action,
        }
    }

    fn mcp_picker(
        &mut self,
        ui: &mut egui::Ui,
        catalog: &i18n::Catalog,
        ctx: &ComposerContext<'_>,
        text_id: egui::Id,
        buffer: &mut String,
        inserted_cursor: &mut Option<usize>,
    ) -> Option<ComposerAction> {
        let egui_ctx = ui.ctx().clone();
        let snapshot = ctx.connector_snapshot;
        let enabled_count = enabled_server_count(snapshot);
        let mut selected = self
            .mcp_selected_server
            .clone()
            .filter(|server_id| enabled_server(snapshot, server_id).is_some())
            .or_else(|| enabled_server_at(snapshot, 0).map(|server| server.id.clone()));

        let response = ui
            .small_button("MCP")
            .on_hover_text(catalog.t("composer.tools_hint", &[]));
        let popup_id = egui::Popup::default_response_id(&response);
        let was_open = egui::Popup::is_id_open(ui.ctx(), popup_id);
        let mut request = None;
        if response.clicked()
            && !was_open
            && let Some(server) = selected
                .as_ref()
                .and_then(|server_id| enabled_server(snapshot, server_id))
            && server.tool_count > 0
            && matching_tool_page(snapshot, &server.id).is_none()
        {
            request = Some(ComposerAction::RequestMcpToolPage {
                server_id: server.id.clone(),
                offset: 0,
            });
        }

        egui::Popup::menu(&response).width(320.0).show(|ui| {
            if enabled_count == 0 {
                ui.weak(catalog.t("composer.tools_empty", &[]));
                return;
            }

            egui::ScrollArea::vertical()
                .id_salt(("composer_mcp_servers", ctx.draft_key))
                .max_height(MCP_SERVER_LIST_HEIGHT)
                .show_rows(ui, MCP_SERVER_ROW_HEIGHT, enabled_count, |ui, rows| {
                    for index in rows {
                        let Some(server) = enabled_server_at(snapshot, index) else {
                            continue;
                        };
                        let is_selected = selected.as_ref() == Some(&server.id);
                        if ui
                            .add_sized(
                                [ui.available_width(), MCP_SERVER_ROW_HEIGHT],
                                egui::Button::selectable(is_selected, &server.name),
                            )
                            .clicked()
                        {
                            selected = Some(server.id.clone());
                            if server.tool_count > 0 {
                                request = Some(ComposerAction::RequestMcpToolPage {
                                    server_id: server.id.clone(),
                                    offset: 0,
                                });
                            }
                        }
                    }
                });
            ui.separator();

            let Some(server) = selected
                .as_ref()
                .and_then(|server_id| enabled_server(snapshot, server_id))
            else {
                ui.weak(catalog.t("composer.tools_empty", &[]));
                return;
            };
            if server.tool_count == 0 {
                ui.weak(catalog.t("composer.tools_empty", &[]));
                return;
            }
            let Some(page) = matching_tool_page(snapshot, &server.id) else {
                // 요청은 popup open/selection 이벤트가 정확히 한 번 생성한다. 결과를
                // 기다리는 stable frame은 저장소를 poll하거나 intent를 반복하지 않는다.
                ui.weak("…");
                return;
            };

            let item_count = page.items.len().min(MCP_TOOL_PAGE_LIMIT);
            egui::ScrollArea::vertical()
                .id_salt((
                    "composer_mcp_tools",
                    ctx.draft_key,
                    server.id.as_str(),
                    page.offset,
                ))
                .max_height(MCP_TOOL_LIST_HEIGHT)
                .show_rows(ui, MCP_TOOL_ROW_HEIGHT, item_count, |ui, rows| {
                    for tool in &page.items[rows] {
                        record_mcp_tool_row_rendered();
                        // 도구 이름만 맨몸으로 삽입하면 에이전트가 명령으로도 함수
                        // 호출로도 못 읽으므로 기존 자연어 지시문 계약을 유지한다.
                        let button = ui.button(format!("{} · {}", server.name, tool.name));
                        let button = match tool.description.as_deref() {
                            Some(description) if !description.is_empty() => {
                                button.on_hover_text(description)
                            }
                            _ => button,
                        };
                        if button.clicked() {
                            let cursor = cursor_char_index(&egui_ctx, text_id);
                            let phrase = format!(
                                "{} ",
                                catalog.t(
                                    "composer.mcp_insert_template",
                                    &[("tool", tool.name.as_str())],
                                )
                            );
                            if let Some(inserted) = self.insert_bounded(buffer, cursor, &phrase) {
                                *inserted_cursor = Some(inserted.cursor);
                            }
                        }
                    }
                });

            let total = page
                .total
                .min(connector_contract::ResourceLimits::PRODUCTION_CEILING.tools_per_server);
            let loaded_end = page.offset.saturating_add(item_count).min(total);
            ui.horizontal(|ui| {
                if page.offset > 0 && ui.small_button("‹").clicked() {
                    request = Some(ComposerAction::RequestMcpToolPage {
                        server_id: server.id.clone(),
                        offset: page.offset.saturating_sub(MCP_TOOL_PAGE_LIMIT),
                    });
                }
                ui.weak(format!("{loaded_end}/{total}"));
                if loaded_end < total && ui.small_button("›").clicked() {
                    request = Some(ComposerAction::RequestMcpToolPage {
                        server_id: server.id.clone(),
                        offset: loaded_end,
                    });
                }
            });
        });

        self.mcp_selected_server = selected;
        request
    }
}

/// Release erased text capacity and compact only substantially shrunken edits. Stable
/// frames do not copy bodies; post-mutation capacity stays below max(2*len,64KiB).
fn compact_draft_buffer(text: &mut String) {
    if text.is_empty() {
        *text = String::new();
    } else if text.capacity() > text.len().saturating_mul(2).max(64 * 1024) {
        *text = text.as_str().to_owned();
    }
}

fn enabled_server_count(snapshot: &ConnectorSnapshot) -> usize {
    snapshot
        .servers
        .iter()
        .filter(|server| server.enabled)
        .take(MCP_SERVER_LIMIT)
        .count()
}

fn enabled_server_at(snapshot: &ConnectorSnapshot, index: usize) -> Option<&ServerSummary> {
    (index < MCP_SERVER_LIMIT)
        .then(|| {
            snapshot
                .servers
                .iter()
                .filter(|server| server.enabled)
                .nth(index)
        })
        .flatten()
}

fn enabled_server<'a>(
    snapshot: &'a ConnectorSnapshot,
    server_id: &ServerId,
) -> Option<&'a ServerSummary> {
    snapshot
        .servers
        .iter()
        .filter(|server| server.enabled)
        .take(MCP_SERVER_LIMIT)
        .find(|server| &server.id == server_id)
}

fn matching_tool_page<'a>(
    snapshot: &'a ConnectorSnapshot,
    server_id: &ServerId,
) -> Option<&'a ToolPage> {
    snapshot
        .tool_page
        .as_ref()
        .filter(|page| &page.server_id == server_id)
}

fn record_mcp_tool_row_rendered() {
    #[cfg(test)]
    MCP_TOOL_ROWS_RENDERED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(test)]
static MCP_TOOL_ROWS_RENDERED: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// 정확히 이 (modifiers, key) 조합의 key-down만 소비한다. egui `consume_key`의
/// `matches_logically`는 여분 Shift/Alt를 무시해 Shift+Enter(개행)가 Enter(전송)로도
/// 매칭된다 — 개행 키가 전송으로 새지 않게 `matches_exact`를 쓴다.
fn consume_key_exact(egui_ctx: &egui::Context, modifiers: egui::Modifiers, key: egui::Key) -> bool {
    egui_ctx.input_mut(|input| {
        let mut consumed = false;
        input.events.retain(|event| {
            if consumed {
                return true;
            }
            let egui::Event::Key {
                key: event_key,
                pressed: true,
                modifiers: event_modifiers,
                ..
            } = event
            else {
                return true;
            };
            if *event_key == key && event_modifiers.matches_exact(modifiers) {
                consumed = true;
                return false;
            }
            true
        });
        consumed
    })
}

/// 현재 TextEdit 커서/선택의 (primary, secondary) 문자 인덱스 — 직전 프레임 상태.
/// 두 끝점은 어느 순서로도 올 수 있다(방향 = 선택 의미론, egui CCursorRange 계약).
fn stored_char_range(egui_ctx: &egui::Context, text_id: egui::Id) -> Option<(usize, usize)> {
    egui::text_edit::TextEditState::load(egui_ctx, text_id)
        .and_then(|state| state.cursor.char_range())
        .map(|range| (range.primary.index.into(), range.secondary.index.into()))
}

/// 현재 TextEdit 커서(primary)의 문자 인덱스 (직전 프레임 상태 — 삽입 위치 결정용).
fn cursor_char_index(egui_ctx: &egui::Context, text_id: egui::Id) -> Option<usize> {
    stored_char_range(egui_ctx, text_id).map(|(primary, _)| primary)
}

/// insert_snippet 결과 — 삽입 끝 커서와 **실제로 붙인** 패딩 공백. 제거 시 이 기록만큼만
/// 정확히 걷는다(인접 공백 추정은 사용자 공백/들여쓰기를 삼킨다 — codex P2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InsertedSnippet {
    /// 삽입 조각 끝의 문자 인덱스.
    cursor: usize,
    /// 앞에 구분 공백을 붙였다.
    leading_space: bool,
    /// 뒤에 구분 공백을 붙였다.
    trailing_space: bool,
}

/// 커서(문자 인덱스) 위치에 스니펫을 삽입한다 — 단어에 눌어붙지 않게 앞뒤 공백을 보정.
/// None이면 끝에 붙인다. 커서 반환은 호출측이 캐럿을 옮겨 다음 타이핑이 삽입 텍스트
/// 앞/안에 끼지 않게 하기 위함이다(codex P2).
fn insert_snippet(
    buffer: &mut String,
    cursor_chars: Option<usize>,
    snippet: &str,
) -> InsertedSnippet {
    let char_len = buffer.chars().count();
    let at = cursor_chars.unwrap_or(char_len).min(char_len);
    let byte = buffer
        .char_indices()
        .nth(at)
        .map(|(byte, _)| byte)
        .unwrap_or(buffer.len());
    let leading_space = byte > 0 && !buffer[..byte].ends_with(char::is_whitespace);
    let trailing_space = byte < buffer.len() && !buffer[byte..].starts_with(char::is_whitespace);
    let mut piece = String::new();
    if leading_space {
        piece.push(' ');
    }
    piece.push_str(snippet);
    if trailing_space {
        piece.push(' ');
    }
    buffer.insert_str(byte, &piece);
    InsertedSnippet {
        cursor: at + piece.chars().count(),
        leading_space,
        trailing_space,
    }
}

/// `/model <이름>`을 버퍼 맨 앞에 삽입 — claude/codex 모두 슬래시 커맨드는 줄 단위라
/// 개행으로 분리한다. 전송은 사용자가 검토 후 직접 한다.
/// 반환: 삽입 조각 끝(개행 뒤)의 문자 인덱스 — insert_snippet과 같은 커서 규약.
fn prepend_model_command(buffer: &mut String, model: &str) -> usize {
    let command = format!("/model {model}\n");
    buffer.insert_str(0, &command);
    command.chars().count()
}

/// 첨부 플레이스홀더 토큰 — 변환 중 드래프트에 보이는 자리표시이자 완료 시 치환
/// 앵커다. `⟦⟧`(U+27E6/27E7)는 프롬프트에 자연 발생하기 어렵다.
fn attach_token(seq: u64) -> String {
    format!("⟦attach-{seq}⟧")
}

/// Native context picker의 latest-only continuation anchor. Monotonic identity를 토큰에도
/// 넣어 대체된 picker의 늦은 완료가 새 요청 자리에 적용될 수 없게 한다.
fn context_file_token(seq: u64) -> String {
    format!("⟦context-file-{seq}⟧")
}

/// 캐럿(선택 없음)을 TextEditState에 **즉시** 기록한다 — show 전에 버퍼를 바꾸는
/// 경로(첨부 치환·드롭 삽입) 전용(codex P1). post-show 예약(pending_cursor)은 이
/// 프레임의 TextEdit이 이미 낡은 캐럿으로 키 입력을 처리한 뒤라 늦다.
fn store_caret_now(egui_ctx: &egui::Context, text_id: egui::Id, caret: usize) {
    store_caret_range_now(egui_ctx, text_id, caret, caret);
}

/// 선택 양 끝점을 방향 그대로 TextEditState에 즉시 기록한다 — 첨부 완료가 사용자
/// 선택을 무너뜨리지 않게 primary/secondary를 각각 보존한다(codex P2).
fn store_caret_range_now(
    egui_ctx: &egui::Context,
    text_id: egui::Id,
    primary: usize,
    secondary: usize,
) {
    let mut state = egui::text_edit::TextEditState::load(egui_ctx, text_id).unwrap_or_default();
    state.cursor.set_char_range(Some(egui::text::CCursorRange {
        primary: egui::text::CCursor::new(primary),
        secondary: egui::text::CCursor::new(secondary),
        h_pos: None,
    }));
    state.store(egui_ctx, text_id);
}

/// 치환할 토큰 패턴과 시작 바이트를 찾는다. **제거**(replacement가 빈 문자열)면
/// 삽입 시점에 기록된 패딩 공백(padding — 선행/후행)까지 패턴에 포함해 이중 공백을
/// 남기지 않는다. 인접 공백으로 패딩을 **추정하지 않는다** — 토큰이 사용자 공백/
/// 들여쓰기 옆이면 사용자 공백 하나를 삼킨다(codex P2). 패딩 붙은 패턴이 없으면
/// (사용자가 패딩 공백을 지움) bare 토큰만 제거한다. 토큰 자체가 없으면 None(취소).
fn find_token_pattern(
    draft: &str,
    token: &str,
    replacement: &str,
    padding: (bool, bool),
) -> Option<(String, usize)> {
    if replacement.is_empty() && (padding.0 || padding.1) {
        let mut pattern = String::new();
        if padding.0 {
            pattern.push(' ');
        }
        pattern.push_str(token);
        if padding.1 {
            pattern.push(' ');
        }
        if let Some(byte) = draft.find(&pattern) {
            return Some((pattern, byte));
        }
    }
    draft.find(token).map(|byte| (token.to_owned(), byte))
}

/// command/ctrl 별칭 접기 — 비-macOS 물리 Ctrl은 이벤트/리바인드 캡처에 두 플래그를
/// 다 켠다. 가림 판정 **이 비교에 한해** command(·mac_cmd)를 ctrl로 접어 같은 물리
/// 코드를 하나로 본다. shift/alt는 그대로 구분한다.
fn fold_command_into_ctrl(modifiers: egui::Modifiers) -> egui::Modifiers {
    egui::Modifiers {
        alt: modifiers.alt,
        ctrl: modifiers.ctrl || modifiers.command || modifiers.mac_cmd,
        shift: modifiers.shift,
        mac_cmd: false,
        command: false,
    }
}

/// 접힘 바인딩이 현재 전송 코드(…+Enter)와 같은 물리 이벤트를 소비할 수 있는가.
///
/// `fold_ctrl_command_alias`(호출측이 `cfg!(not(target_os = "macos"))`를 넘긴다 —
/// 인자화해 양 플랫폼을 유닛으로 검증):
/// - **true(비-macOS)**: 물리 Ctrl 이벤트가 ctrl|command를 **둘 다** 켜서,
///   `Command+Enter` 바인딩과 `Ctrl+Enter` 전송이 서로는 매치되지 않아도 실제
///   이벤트는 양쪽을 만족한다(codex P2 6차) — 별칭을 접은 뒤 구조 비교.
/// - **false(macOS)**: Cmd와 Ctrl은 서로 다른 물리 키다 — 접으면 Cmd+Enter 바인딩
///   vs Ctrl+Enter 전송을 충돌로 오판해 접힘 단축키가 죽는다(codex P2 7차 회귀).
///   dispatcher와 같은 matches_exact 양방향 OR(5차)로 판정한다.
fn shadows_send_chord(
    collapse: &egui::KeyboardShortcut,
    send_modifiers: egui::Modifiers,
    fold_ctrl_command_alias: bool,
) -> bool {
    if collapse.logical_key != egui::Key::Enter {
        return false;
    }
    if fold_ctrl_command_alias {
        fold_command_into_ctrl(collapse.modifiers) == fold_command_into_ctrl(send_modifiers)
    } else {
        collapse.modifiers.matches_exact(send_modifiers)
            || send_modifiers.matches_exact(collapse.modifiers)
    }
}

/// 치환 델타에 따른 캐럿 리베이스(codex P1) — 치환 구간 앞이면 그대로, 뒤면 델타만큼
/// 이동, 구간 안이면 치환 결과 끝으로 스냅한다.
fn rebase_caret(
    caret: usize,
    replace_start: usize,
    pattern_chars: usize,
    replacement_chars: usize,
) -> usize {
    if caret <= replace_start {
        caret
    } else if caret >= replace_start + pattern_chars {
        caret - pattern_chars + replacement_chars
    } else {
        replace_start + replacement_chars
    }
}

/// 여러 파일 경로를 공백으로 이어 **한 스니펫**으로 — 같은 커서에 파일별로 반복
/// 삽입하면 역순이 되므로(codex P3) 호출측은 이걸 1회 삽입한다. 순서 보존.
fn joined_mentions(root: Option<&Path>, paths: &[PathBuf]) -> String {
    let mut joined = String::new();
    for path in paths {
        if !joined.is_empty() {
            joined.push(' ');
        }
        joined.push_str(&mention_path(root, path));
    }
    joined
}

fn attachment_path_bytes(paths: &[PathBuf]) -> Option<usize> {
    if paths.is_empty() || paths.len() > COMPOSER_ATTACHMENT_MAX_ITEMS {
        return None;
    }
    let total = paths.iter().try_fold(0usize, |total, path| {
        let bytes = path.as_os_str().as_encoded_bytes().len();
        if bytes > CONTEXT_FILE_PATH_MAX_BYTES {
            return None;
        }
        total.checked_add(bytes)
    })?;
    (total <= COMPOSER_ATTACHMENT_MAX_BYTES).then_some(total)
}

/// 비-macOS 이미지 첨부 트리거 — 터미널(ui/workspace.rs is_clipboard_paste_shortcut)의
/// 비-macOS 분기와 같은 Ctrl+Shift+V press 판정(codex P2 — native monitor가 no-op라
/// 폴백 필요). macOS는 AppKit native monitor가 주 경로라 여기선 항상 false(release
/// fallback은 제스처 중복 제거 상태가 필요해 터미널만 쓴다).
fn is_attach_paste_shortcut(event: &egui::Event) -> bool {
    if cfg!(target_os = "macos") {
        return false;
    }
    matches!(
        event,
        egui::Event::Key {
            key: egui::Key::V,
            pressed: true,
            modifiers,
            ..
        } if modifiers.ctrl && modifiers.shift
    )
}

/// 파일 경로 → 버퍼 삽입 텍스트. 워크스페이스 루트 하위면 `@상대경로`(claude @멘션 규약),
/// 밖이면 절대경로 그대로. macOS 드래그&드롭·파일 피커는 파일명을 NFD(자소분해)로
/// 넘겨 한글이 깨져 보이므로(terminal 크레이트 alacritty_backend.rs composed_char와
/// 동일 문제) NFC로 합성해 삽입한다.
fn mention_path(root: Option<&Path>, path: &Path) -> String {
    // strip_prefix 비교는 정규화 전 원본 경로끼리 해야 한다 — root도 OS가 준 그대로라
    // NFD일 수 있고, 한쪽만 NFC로 바꾸면 같은 경로인데도 접두 판정이 어긋난다.
    // NFC 변환은 최종 표시 문자열에만 적용한다.
    match root.and_then(|root| path.strip_prefix(root).ok()) {
        Some(rel) if !rel.as_os_str().is_empty() => {
            format!("@{}", rel.display().to_string().nfc().collect::<String>())
        }
        _ => path.display().to_string().nfc().collect::<String>(),
    }
}

/// (커서가 첫 줄인가, 마지막 줄인가) — 문자 인덱스 기준. 빈 버퍼는 (true, true).
fn cursor_line_info(text: &str, cursor_chars: usize) -> (bool, bool) {
    let on_first = !text.chars().take(cursor_chars).any(|c| c == '\n');
    let on_last = !text.chars().skip(cursor_chars).any(|c| c == '\n');
    (on_first, on_last)
}

fn history_bytes(history: &[std::sync::Arc<str>]) -> usize {
    history.iter().map(|entry| entry.len()).sum()
}

/// 히스토리에 추가 — 연속 중복은 접고 item/aggregate 상한을 넘으면 오래된 것부터
/// 버린다. 호출은 load 또는 Send 이벤트에만 있고 stable frame에서는 실행되지 않는다.
fn push_history(history: &mut Vec<std::sync::Arc<str>>, entry: std::sync::Arc<str>) {
    if entry.len() > COMPOSER_PROMPT_MAX_BYTES {
        return;
    }
    if history.last() == Some(&entry) {
        return;
    }
    history.push(entry);
    while history.len() > COMPOSER_HISTORY_MAX_ITEMS
        || history_bytes(history) > COMPOSER_HISTORY_MAX_BYTES
    {
        history.remove(0);
    }
}

/// 히스토리 로드(시작 시 1회) — jsonl 한 줄 = JSON 문자열 하나. 깨진 줄은 건너뛴다.
fn load_history(path: &Path) -> Vec<std::sync::Arc<str>> {
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let mut content = String::new();
    if file
        .take((COMPOSER_HISTORY_FILE_MAX_BYTES + 1) as u64)
        .read_to_string(&mut content)
        .is_err()
        || content.len() > COMPOSER_HISTORY_FILE_MAX_BYTES
    {
        return Vec::new();
    }
    let mut history = Vec::new();
    for line in content.lines() {
        if let Ok(entry) = serde_json::from_str::<String>(line) {
            push_history(&mut history, entry.into());
        }
    }
    history
}

#[cfg(test)]
fn save_history(path: &Path, history: &[std::sync::Arc<str>]) {
    let mut out = String::new();
    for entry in history {
        if let Ok(line) = serde_json::to_string(entry) {
            out.push_str(&line);
            out.push('\n');
        }
    }
    if let Err(e) = std::fs::write(path, out) {
        tracing::warn!("컴포저 히스토리 저장 실패: {e:#}");
    }
}

/// 컴포저 텍스트를 PTY 입력 바이트로 인코딩한다 — **web-remote P6a
/// (`crates/web-remote/src/dashboard.rs::encode_input`)와 동일 규칙**. 그쪽 함수는
/// crate-private라 복제한다(규칙 변경 시 양쪽을 함께 고칠 것):
/// 1) 개행 정규화(\r\n·\r→\n) 후 C0 제어문자 strip(\t·\n 제외).
/// 2) \n→\r (터미널 Enter는 CR).
/// 3) 여러 줄이거나 512B 초과면 붙여넣기로 간주 — 세션 bracketed paste 모드가
///    켜져 있으면 `ESC[200~ … ESC[201~` wrap.
/// 4) submit이면 wrap **밖에** Enter(\r)를 덧붙인다.
///
/// 빈 결과(공백뿐 + submit 없음)는 None.
pub fn encode_prompt_input(text: &str, submit: bool, bracketed: bool) -> Option<Vec<u8>> {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let cleaned: String = normalized
        .chars()
        .filter(|c| !c.is_control() || *c == '\t' || *c == '\n')
        .collect();
    let is_paste = cleaned.contains('\n') || cleaned.len() > INPUT_PASTE_THRESHOLD;
    let body = cleaned.replace('\n', "\r");
    if body.is_empty() && !submit {
        return None;
    }
    let mut bytes = Vec::with_capacity(body.len() + 16);
    if is_paste && bracketed {
        bytes.extend_from_slice(b"\x1b[200~");
        bytes.extend_from_slice(body.as_bytes());
        bytes.extend_from_slice(b"\x1b[201~");
    } else {
        bytes.extend_from_slice(body.as_bytes());
    }
    if submit {
        bytes.push(b'\r');
    }
    Some(bytes)
}

/// 도크 컴포저 전송을 bracketed-paste TUI 세션에 맞게 인코딩한다.
///
/// Codex 같은 TUI는 bracketed paste 이벤트를 명시적으로 인식하므로, 본문을
/// `ESC[200~ ... ESC[201~`로 감싸 단일 paste로 만든다. 이렇게 하면 본문 안의
/// 개행이나 뒤따르는 Enter가 paste-burst heuristic으로 잘못 처리되는 것을 막을 수
/// 있다. 본문과 submit용 CR은 반드시 별도 `WriteInput`으로 나뉘어 전송되어야
/// 한다(호출측 `app.rs`가 그렇게 한다).
///
/// bracketed paste 안의 개행은 literal `\n`으로 남긴다. 공용 인코더의 `\n→\r`
/// 변환은 "Enter 키" 의미를 위한 것이고, paste 안에서는 오히려 불필요한 Enter
/// 이벤트를 만들 수 있다.
pub fn encode_tui_paste_submission(text: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let cleaned: String = normalized
        .chars()
        .filter(|c| !c.is_control() || *c == '\t' || *c == '\n')
        .collect();
    if cleaned.is_empty() {
        return None;
    }
    let mut body = Vec::with_capacity(cleaned.len() + 12);
    body.extend_from_slice(b"\x1b[200~");
    body.extend_from_slice(cleaned.as_bytes());
    body.extend_from_slice(b"\x1b[201~");
    Some((body, b"\r".to_vec()))
}

/// 전송 대상 에이전트에 따라 한 번에 복수 WriteInput이 필요한지 미리 결정한다.
/// App은 이 계획대로 runtime에 순서대로 명령을 보낸다.
#[derive(Debug, Clone, PartialEq)]
pub enum ComposerInputPlan {
    /// Claude·일반 셸용: 단일 WriteInput.
    Single(Vec<u8>),
    /// TUI용: bracketed paste 본문과 분리된 submit CR.
    BracketedPaste { body: Vec<u8>, submit: Vec<u8> },
}

impl ComposerInputPlan {
    pub fn into_parts(self) -> Vec<Vec<u8>> {
        match self {
            Self::Single(bytes) => vec![bytes],
            Self::BracketedPaste { body, submit } => vec![body, submit],
        }
    }
}

/// 프로바이더/터미널 상태에 맞는 입력 계획을 만든다.
///
/// - Codex로 감지됐거나 터미널이 DEC 2004를 켠 세션은 명시적 bracketed paste + 별도
///   CR을 쓴다. 짧은 입력을 일반 키 입력으로 보내면서 Codex 감지 타이밍에 의존하던
///   문제를 없앤다.
/// - bracketed paste를 쓰지 않는 일반 셸은 기존 `encode_prompt_input` 경로를 유지한다.
pub fn plan_composer_input(
    text: &str,
    submit: bool,
    bracketed: bool,
    provider: Option<AgentProvider>,
) -> Option<ComposerInputPlan> {
    let force_bracketed = bracketed || provider == Some(AgentProvider::Codex);
    if force_bracketed {
        let (body, enter) = encode_tui_paste_submission(text)?;
        return Some(if submit {
            ComposerInputPlan::BracketedPaste {
                body,
                submit: enter,
            }
        } else {
            ComposerInputPlan::Single(body)
        });
    }
    encode_prompt_input(text, submit, false).map(ComposerInputPlan::Single)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pr5_real_same_workspace_persisted_sessions_keep_independent_drafts() {
        let catalog = i18n::Catalog::load("en-US").unwrap();
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, state: &mut (ComposerUi, String, ConnectorSnapshot)| {
                state.0.render(
                    ui,
                    &catalog,
                    &ComposerContext {
                        workspace_id: TEST_WS,
                        draft_key: &state.1,
                        runtime_generation: 1,
                        send_key: ComposerSendKey::CmdEnter,
                        can_send: true,
                        agent: None,
                        workspace_root: None,
                        collapse_shortcut: None,
                        connector_snapshot: &state.2,
                    },
                );
            },
            (
                ComposerUi::new(test_history_path("pr5-sessions")),
                "persisted-session-A".to_owned(),
                ConnectorSnapshot::default(),
            ),
        );
        use egui_kittest::kittest::Queryable;
        harness.run();
        harness
            .get_by_role(egui::accesskit::Role::MultilineTextInput)
            .click();
        harness.run();
        harness
            .input_mut()
            .events
            .push(egui::Event::Text("first".into()));
        harness.run();
        harness.state_mut().1 = "persisted-session-B".into();
        harness.run();
        harness
            .get_by_role(egui::accesskit::Role::MultilineTextInput)
            .click();
        harness.run();
        harness
            .input_mut()
            .events
            .push(egui::Event::Text("second".into()));
        harness.run();
        assert_eq!(
            harness.state().0.current_text("persisted-session-A"),
            "first"
        );
        assert_eq!(
            harness.state().0.current_text("persisted-session-B"),
            "second"
        );
    }

    #[test]
    fn pr5_real_ime_cannot_grow_draft_past_one_mib() {
        let catalog = i18n::Catalog::load("en-US").unwrap();
        let mut harness = composer_harness(
            &catalog,
            ComposerSendKey::CmdEnter,
            test_history_path("pr5-ime"),
        );
        let original = "가".repeat(COMPOSER_PROMPT_MAX_BYTES / 3);
        harness
            .state_mut()
            .0
            .buffers
            .insert(TEST_WS.into(), original.clone());
        harness.run();
        focus_composer(&mut harness);
        store_caret_now(
            &harness.ctx,
            ComposerUi::text_id(TEST_WS),
            original.chars().count(),
        );
        harness
            .ctx
            .memory_mut(|memory| memory.request_focus(ComposerUi::text_id(TEST_WS)));
        assert!(
            harness
                .ctx
                .memory(|memory| memory.has_focus(ComposerUi::text_id(TEST_WS)))
        );
        harness
            .input_mut()
            .events
            .push(egui::Event::Ime(egui::ImeEvent::Preedit {
                text: "나".into(),
                active_range_chars: None,
            }));
        harness.run();
        harness
            .input_mut()
            .events
            .push(egui::Event::Ime(egui::ImeEvent::Commit("나".into())));
        harness.run();
        assert_eq!(
            buffer_of(&harness).len(),
            original.len(),
            "live IME bypasses the send-only cap"
        );
    }

    #[test]
    fn pr5_real_typing_and_paste_refuse_overflow_without_losing_selection() {
        let catalog = i18n::Catalog::load("en-US").unwrap();
        let mut harness = composer_harness(
            &catalog,
            ComposerSendKey::CmdEnter,
            test_history_path("pr5-overflow"),
        );
        let original = "가".repeat(COMPOSER_PROMPT_MAX_BYTES / 3);
        harness
            .state_mut()
            .0
            .buffers
            .insert(TEST_WS.into(), original.clone());
        harness.run();
        let id = ComposerUi::text_id(TEST_WS);
        harness.ctx.memory_mut(|memory| memory.request_focus(id));
        store_caret_now(&harness.ctx, id, original.chars().count());
        harness
            .input_mut()
            .events
            .push(egui::Event::Text("나".into()));
        harness.run();
        assert_eq!(
            buffer_of(&harness).len(),
            original.len(),
            "typing must not bypass byte budget"
        );
        harness.ctx.memory_mut(|memory| memory.request_focus(id));
        store_caret_range_now(&harness.ctx, id, 1, 0);
        harness.input_mut().events.push(egui::Event::Paste(
            "x".repeat(COMPOSER_PROMPT_MAX_BYTES + 1),
        ));
        harness.run();
        assert_eq!(
            buffer_of(&harness),
            original,
            "over-limit paste keeps original selection body"
        );
        assert_eq!(stored_char_range(&harness.ctx, id), Some((1, 0)));
    }

    #[test]
    fn pr5_restart_existing_draft_fixture_is_restored() {
        let dir = std::env::temp_dir().join(format!("deppy-pr5-startup-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("composer_drafts.json");
        std::fs::write(&path, r#"{"drafts":[{"key":"persisted-session-A","workspace_id":"ws-test","text":"한글 recovered"}]}"#).unwrap();
        let startup = DraftSnapshot::load_startup(&path);
        assert!(startup.error.is_none());
        let mut composer = ComposerUi::new(dir.join("composer_history.jsonl"));
        composer.restore_drafts(startup.snapshot);
        assert_eq!(
            composer.current_text("persisted-session-A"),
            "한글 recovered"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn pr5_aggregate_item_metadata_and_programmatic_ingestion_preserve_existing_drafts() {
        let mut composer = ComposerUi::new(test_history_path("pr5-budget"));
        let full: Arc<str> = Arc::from("x".repeat(COMPOSER_PROMPT_MAX_BYTES));
        composer.restore_drafts(DraftSnapshot {
            drafts: (0..32)
                .map(|i| DraftRecord {
                    key: format!("session-{i}"),
                    workspace_id: "ws".into(),
                    delivery_uncertain: false,
                    text: Arc::clone(&full),
                })
                .collect(),
        });
        composer.insert_text("session-new", "Korean 한글");
        assert!(composer.current_text("session-new").is_empty());
        assert_eq!(
            composer.buffers.values().map(String::len).sum::<usize>(),
            DRAFT_TOTAL_MAX_BYTES
        );
        assert!(composer.input_limited);
        composer.insert_text("session-0", "overflow");
        assert_eq!(
            composer.current_text("session-0").len(),
            COMPOSER_PROMPT_MAX_BYTES
        );
        composer.delete_draft(&egui::Context::default(), "session-0");
        composer.checkpoint();
        composer.insert_text("session-new", "한글");
        assert_eq!(composer.current_text("session-new"), "한글");
        let mut composer = ComposerUi::new(test_history_path("pr5-items"));
        composer.restore_drafts(DraftSnapshot {
            drafts: (0..DRAFT_MAX_ITEMS)
                .map(|i| DraftRecord {
                    key: format!("session-{i}"),
                    workspace_id: "ws".into(),
                    delivery_uncertain: false,
                    text: Arc::from("dirty"),
                })
                .collect(),
        });
        composer.insert_text("session-over-item", "new");
        assert!(composer.current_text("session-over-item").is_empty());
        assert_eq!(composer.buffers.len(), DRAFT_MAX_ITEMS);
        let mut composer = ComposerUi::new(test_history_path("pr5-metadata"));
        composer.restore_drafts(DraftSnapshot {
            drafts: (0..64)
                .map(|i| DraftRecord {
                    key: format!("{i:03}{}", "k".repeat(4085)),
                    workspace_id: "owner-id".into(),
                    delivery_uncertain: false,
                    text: Arc::from("dirty"),
                })
                .collect(),
        });
        assert_eq!(composer.buffers.len(), 64);
        composer.insert_text(&"z".repeat(1000), "new");
        assert_eq!(
            composer.buffers.len(),
            64,
            "metadata cap must refuse rather than evict"
        );
    }
    #[test]
    fn pr5_checkpoint_shares_unchanged_body_and_strips_only_owned_pending_tokens() {
        let egui_ctx = egui::Context::default();
        let mut composer = ComposerUi::new(test_history_path("pr5-checkpoint"));
        composer.insert_text("A", "user ⟦attach-999⟧");
        composer.insert_text("B", "unchanged");
        let first = composer.checkpoint();
        let previous = Arc::clone(
            &first
                .drafts
                .iter()
                .find(|draft| draft.key == "B")
                .unwrap()
                .text,
        );
        composer.last_workspace = Some("A".into());
        composer.active_workspace_id = "actual-workspace".into();
        composer.active_generation = 4;
        let mut buffer = composer.buffers.remove("A").unwrap();
        let request = composer
            .begin_attachment_request(&egui_ctx, "A", None, &mut buffer)
            .unwrap();
        composer.buffers.insert("A".into(), buffer);
        let second = composer.checkpoint();
        let body = &second
            .drafts
            .iter()
            .find(|draft| draft.key == "A")
            .unwrap()
            .text;
        assert_eq!(body.as_ref(), "user ⟦attach-999⟧");
        assert!(composer.current_text("A").contains(request.token()));
        assert!(Arc::ptr_eq(
            &previous,
            &second
                .drafts
                .iter()
                .find(|draft| draft.key == "B")
                .unwrap()
                .text
        ));
        let mut restored = ComposerUi::new(test_history_path("pr5-restored"));
        restored.restore_drafts((*second).clone());
        assert_eq!(restored.current_text("A"), "user ⟦attach-999⟧");
    }
    #[test]
    fn pr5_late_context_file_routes_original_session_root_and_cannot_resurrect_deleted_draft() {
        let egui_ctx = egui::Context::default();
        let mut composer = ComposerUi::new(test_history_path("pr5-late"));
        let connectors = ConnectorSnapshot::default();
        let context = |key| ComposerContext {
            workspace_id: "workspace",
            draft_key: key,
            runtime_generation: 7,
            send_key: ComposerSendKey::CmdEnter,
            can_send: true,
            agent: None,
            workspace_root: None,
            collapse_shortcut: None,
            connector_snapshot: &connectors,
        };
        composer.bind_context(&egui_ctx, &context("A"));
        let mut buffer = "first".to_owned();
        let (request, _) = composer
            .begin_context_file_request(&egui_ctx, "A", Some(Path::new("/original")), &mut buffer)
            .unwrap();
        assert_eq!(request.target.workspace_id, "workspace");
        assert_eq!(request.target.draft_key, "A");
        composer.buffers.insert("A".into(), buffer);
        composer.mark_dirty("A");
        composer.bind_context(&egui_ctx, &context("B"));
        composer.insert_text("B", "second");
        assert!(composer.complete_context_file(
            &egui_ctx,
            request,
            Some(PathBuf::from("/original/file.txt")),
            "B"
        ));
        assert_eq!(composer.current_text("A"), "first @file.txt");
        assert_eq!(composer.current_text("B"), "second");
        composer.bind_context(&egui_ctx, &context("A"));
        let mut buffer = composer.buffers.remove("A").unwrap();
        let (request, _) = composer
            .begin_context_file_request(&egui_ctx, "A", None, &mut buffer)
            .unwrap();
        composer.buffers.insert("A".into(), buffer);
        composer.delete_draft(&egui_ctx, "A");
        assert!(!composer.complete_context_file(
            &egui_ctx,
            request,
            Some(PathBuf::from("/late")),
            "B"
        ));
        assert!(composer.current_text("A").is_empty());
        assert!(
            !composer
                .checkpoint()
                .drafts
                .iter()
                .any(|draft| draft.key == "A")
        );
        assert_eq!(composer.current_text("B"), "second");
    }
    #[test]
    fn pr5_retired_runtime_preserves_draft_but_discards_attachments_and_old_ack() {
        let egui_ctx = egui::Context::default();
        let mut composer = ComposerUi::new(test_history_path("pr5-generation"));
        let connectors = ConnectorSnapshot::default();
        let context = |generation| ComposerContext {
            workspace_id: "workspace",
            draft_key: "A",
            runtime_generation: generation,
            send_key: ComposerSendKey::CmdEnter,
            can_send: true,
            agent: None,
            workspace_root: None,
            collapse_shortcut: None,
            connector_snapshot: &connectors,
        };
        composer.bind_context(&egui_ctx, &context(7));
        composer.insert_text("A", "exact prompt");
        let prompt = composer.current_text("A").to_owned();
        let Some(ComposerAction::Send(submission)) = composer.try_submit(&prompt, true, "A") else {
            panic!("submission")
        };
        let (_, _, id) = submission.into_parts();
        let mut buffer = composer.buffers.remove("A").unwrap();
        let request = composer
            .begin_attachment_request(&egui_ctx, "A", None, &mut buffer)
            .unwrap();
        composer.buffers.insert("A".into(), buffer);
        composer.retire_generation(&egui_ctx, 7);
        assert_eq!(composer.current_text("A"), "exact prompt");
        assert!(!composer.complete_clipboard_attachment(&egui_ctx, request, None, "A"));
        composer.bind_context(&egui_ctx, &context(8));
        assert!(
            composer.submission_blocked("A"),
            "unknown prior delivery needs explicit resend acknowledgment"
        );
        assert!(
            composer
                .settle_submission("A", id, &prompt, PromptAdmissionOutcome::Accepted)
                .is_none()
        );
        assert_eq!(composer.current_text("A"), "exact prompt");
        composer.acknowledge_uncertain_delivery("A");
        assert!(composer.try_submit(&prompt, true, "A").is_some());
    }
    #[test]
    fn pr5_unknown_delivery_payload_budget_and_permanent_delete_release_retention() {
        let mut composer = ComposerUi::new(test_history_path("pr5-delivery-budget"));
        let body = "x".repeat(COMPOSER_PROMPT_MAX_BYTES);
        for i in 0..8 {
            let key = format!("session-{i}");
            let Some(ComposerAction::Send(submission)) = composer.try_submit(&body, true, &key)
            else {
                panic!("bounded admission")
            };
            let (_, _, id) = submission.into_parts();
            composer.settle_submission(&key, id, &body, PromptAdmissionOutcome::Unknown);
        }
        assert_eq!(
            composer
                .deliveries
                .values()
                .map(|delivery| delivery.prompt.len())
                .sum::<usize>(),
            COMPOSER_DELIVERY_MAX_BYTES
        );
        assert!(composer.try_submit("next", true, "session-new").is_none());
        assert!(composer.submission_blocked("session-0"));
        composer.delete_draft(&egui::Context::default(), "session-0");
        assert!(composer.try_submit("next", true, "session-new").is_some());
        assert!(!composer.deliveries.contains_key("session-0"));
    }
    #[test]
    fn pr5_real_composer_undo_is_bounded_and_never_crosses_session_drafts() {
        let ctx = egui::Context::default();
        let catalog = i18n::Catalog::load("en-US").unwrap();
        let connectors = ConnectorSnapshot::default();
        let mut composer = ComposerUi::new(test_history_path("pr5-undo"));
        composer.insert_text("A", "base");
        let mut time = 0.0;
        let frame = |composer: &mut ComposerUi, key: &str, time: f64, events: Vec<egui::Event>| {
            ctx.run_ui(
                egui::RawInput {
                    time: Some(time),
                    events,
                    ..Default::default()
                },
                |ui| {
                    composer.render(
                        ui,
                        &catalog,
                        &ComposerContext {
                            workspace_id: "workspace",
                            draft_key: key,
                            runtime_generation: 1,
                            send_key: ComposerSendKey::CmdEnter,
                            can_send: true,
                            agent: None,
                            workspace_root: None,
                            collapse_shortcut: None,
                            connector_snapshot: &connectors,
                        },
                    );
                },
            )
            .drop_without_applying_deltas();
        };
        frame(&mut composer, "A", time, Vec::new());
        let id = ComposerUi::text_id("A");
        ctx.memory_mut(|memory| memory.request_focus(id));
        store_caret_now(&ctx, id, 4);
        for _ in 0..20 {
            time += 2.0;
            frame(
                &mut composer,
                "A",
                time,
                vec![egui::Event::Text("a".into())],
            );
            time += 2.0;
            frame(&mut composer, "A", time, Vec::new());
        }
        let undo = || egui::Event::Key {
            key: egui::Key::Z,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        };
        let mut undos = 0;
        for _ in 0..25 {
            let previous = composer.current_text("A").to_owned();
            time += 2.0;
            frame(&mut composer, "A", time, vec![undo()]);
            if composer.current_text("A") != previous {
                undos += 1
            }
        }
        assert!(
            undos > 0 && undos <= super::super::text_input::TEXT_EDIT_MAX_UNDOS,
            "actual undo count={undos}"
        );
        let retained_a = composer.current_text("A").to_owned();
        composer.insert_text("B", "B-only");
        time += 2.0;
        frame(&mut composer, "B", time, Vec::new());
        assert_eq!(id, ComposerUi::text_id("B"));
        ctx.memory_mut(|memory| memory.request_focus(id));
        time += 2.0;
        frame(&mut composer, "B", time, vec![undo()]);
        assert_eq!(composer.current_text("B"), "B-only");
        assert_eq!(composer.current_text("A"), retained_a);
        composer.set_read_only(true);
        ctx.memory_mut(|memory| memory.request_focus(id));
        time += 2.0;
        frame(&mut composer, "B", time, Vec::new());
        assert!(
            !ctx.memory(|memory| memory.has_focus(id)),
            "read-only recovery must release focus for terminal typing"
        );
    }
    #[test]
    fn pr5_workspace_delete_retires_unknown_delivery_even_after_its_draft_was_cleared() {
        let ctx = egui::Context::default();
        let mut composer = ComposerUi::new(test_history_path("pr5-cleared-delivery"));
        let connectors = ConnectorSnapshot::default();
        composer.bind_context(
            &ctx,
            &ComposerContext {
                workspace_id: "actual-workspace",
                draft_key: "session-A",
                runtime_generation: 1,
                send_key: ComposerSendKey::CmdEnter,
                can_send: true,
                agent: None,
                workspace_root: None,
                collapse_shortcut: None,
                connector_snapshot: &connectors,
            },
        );
        composer.insert_text("session-A", "original");
        let Some(ComposerAction::Send(submission)) =
            composer.try_submit("original", true, "session-A")
        else {
            panic!("submission")
        };
        let (_, _, id) = submission.into_parts();
        composer.settle_submission("session-A", id, "original", PromptAdmissionOutcome::Unknown);
        composer.buffers.get_mut("session-A").unwrap().clear();
        composer.mark_dirty("session-A");
        composer.checkpoint();
        assert!(
            composer.owners.contains_key("session-A"),
            "empty uncertainty marker keeps its original workspace owner"
        );
        assert!(composer.deliveries.contains_key("session-A"));
        composer.delete_workspace_drafts(&ctx, "other-workspace");
        assert!(composer.submission_blocked("session-A"));
        composer.delete_workspace_drafts(&ctx, "actual-workspace");
        assert!(!composer.deliveries.contains_key("session-A"));
        assert!(!composer.generations.contains_key("session-A"));
    }

    #[test]
    fn pr5_durable_pending_and_empty_unknown_restore_blocked_without_fabricated_receipts() {
        use crate::composer_drafts::DraftFileVersion;
        let dir =
            std::env::temp_dir().join(format!("deppy-pr5-uncertain-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("drafts.json");
        let mut composer = ComposerUi::new(dir.join("history.jsonl"));
        composer.insert_text("A", "exact prompt");
        let Some(ComposerAction::Send(submission)) = composer.try_submit("exact prompt", true, "A")
        else {
            panic!("submission")
        };
        let (_, _, id) = submission.into_parts();
        let snapshot = composer.checkpoint();
        assert!(snapshot.drafts[0].delivery_uncertain);
        let version = snapshot
            .save_checked(&path, DraftFileVersion::Missing)
            .unwrap();
        let startup = DraftSnapshot::load_startup(&path);
        assert!(startup.error.is_none());
        let mut restored = ComposerUi::new(dir.join("unused-history.jsonl"));
        restored.restore_drafts(startup.snapshot);
        assert!(restored.deliveries.is_empty());
        assert_eq!(restored.submission_sequence, 0);
        assert!(restored.submission_blocked("A"));
        assert!(restored.try_submit("exact prompt", true, "A").is_none());
        restored.acknowledge_uncertain_delivery("A");
        assert!(!restored.submission_blocked("A"));
        assert!(!restored.checkpoint().drafts[0].delivery_uncertain);
        assert!(restored.try_submit("reviewed prompt", true, "A").is_some());
        composer.settle_submission("A", id, "exact prompt", PromptAdmissionOutcome::Unknown);
        composer.buffers.get_mut("A").unwrap().clear();
        composer.mark_dirty("A");
        let empty = composer.checkpoint();
        assert_eq!(empty.drafts.len(), 1);
        assert!(empty.drafts[0].text.is_empty());
        assert!(empty.drafts[0].delivery_uncertain);
        empty.save_checked(&path, version).unwrap();
        let mut restored = ComposerUi::new(dir.join("unused-history.jsonl"));
        restored.restore_drafts(DraftSnapshot::load_startup(&path).snapshot);
        assert!(restored.current_text("A").is_empty());
        assert!(restored.submission_blocked("A"));
        assert_eq!(restored.owners.get("A").map(String::as_str), Some("A"));
        restored.acknowledge_uncertain_delivery("A");
        assert!(restored.checkpoint().drafts.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn pr5_uncertainty_checkpoint_invalidates_on_exact_ack_and_old_format_defaults_safe() {
        let dir =
            std::env::temp_dir().join(format!("deppy-pr5-old-format-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("drafts.json");
        std::fs::write(
            &path,
            br#"{"drafts":[{"key":"A","workspace_id":"workspace","text":"legacy"}]}"#,
        )
        .unwrap();
        let loaded = DraftSnapshot::load_startup(&path);
        assert!(loaded.error.is_none());
        assert!(!loaded.snapshot.drafts[0].delivery_uncertain);
        let mut composer = ComposerUi::new(dir.join("history.jsonl"));
        composer.restore_drafts(loaded.snapshot);
        let Some(ComposerAction::Send(submission)) = composer.try_submit("legacy", true, "A")
        else {
            panic!("submission")
        };
        let (_, _, id) = submission.into_parts();
        assert!(composer.checkpoint().drafts[0].delivery_uncertain);
        let revision = composer.draft_revision();
        composer.settle_submission("A", id, "legacy", PromptAdmissionOutcome::Rejected);
        assert!(composer.draft_revision() > revision);
        assert!(!composer.checkpoint().drafts[0].delivery_uncertain);
        assert!(!composer.submission_blocked("A"));
        let Some(ComposerAction::Send(submission)) = composer.try_submit("legacy", true, "A")
        else {
            panic!("submission")
        };
        let (_, _, id) = submission.into_parts();
        assert!(composer.checkpoint().drafts[0].delivery_uncertain);
        composer.insert_text("A", "follow-up");
        let retained = composer.current_text("A").to_owned();
        composer.settle_submission("A", id, "legacy", PromptAdmissionOutcome::Accepted);
        let saved = composer.checkpoint();
        assert_eq!(saved.drafts[0].text.as_ref(), retained);
        assert!(!saved.drafts[0].delivery_uncertain);
        assert!(!composer.submission_blocked("A"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn pr5_checkpoint_releases_cleared_capacity_and_compacts_shrunk_dirty_drafts() {
        let mut composer = ComposerUi::new(test_history_path("pr5-capacity"));
        let body = "x".repeat(32 * 1024);
        for i in 0..300 {
            let key = format!("cleared-{i}");
            composer.insert_text(&key, &body);
            composer.buffers.get_mut(&key).unwrap().clear();
            composer.mark_dirty(&key);
            composer.checkpoint();
        }
        let retained: usize = composer.buffers.values().map(String::capacity).sum();
        assert!(
            composer.buffers.is_empty(),
            "cleared entries retained={} bytes in {} buffers",
            retained,
            composer.buffers.len()
        );
        composer.insert_text("uncertain", &body);
        let Some(ComposerAction::Send(submission)) = composer.try_submit(&body, true, "uncertain")
        else {
            panic!("submission")
        };
        let (_, _, id) = submission.into_parts();
        composer.settle_submission("uncertain", id, &body, PromptAdmissionOutcome::Unknown);
        composer.buffers.get_mut("uncertain").unwrap().clear();
        composer.mark_dirty("uncertain");
        let saved = composer.checkpoint();
        assert!(saved.drafts[0].delivery_uncertain);
        assert_eq!(
            composer.buffers["uncertain"].capacity(),
            0,
            "empty marker must not retain old1MiB capacity"
        );
        composer.insert_text("small", &"x".repeat(COMPOSER_PROMPT_MAX_BYTES));
        composer.buffers.get_mut("small").unwrap().truncate(1);
        composer.mark_dirty("small");
        composer.checkpoint();
        assert_eq!(composer.current_text("small"), "x");
        assert!(
            composer.buffers["small"].capacity() <= 64 * 1024,
            "large-shrunk dirty body retained excess capacity"
        );
    }

    #[test]
    fn pr5_readonly_recovery_refuses_programmatic_mutation_and_retains_exact_draft() {
        let mut composer = ComposerUi::new(test_history_path("pr5-readonly"));
        composer.insert_text("A", "original");
        let revision = composer.draft_revision();
        composer.set_read_only(true);
        composer.insert_text("A", "overwrite");
        assert_eq!(composer.current_text("A"), "original");
        assert_eq!(composer.draft_revision(), revision);
        assert!(composer.try_submit("original", true, "A").is_none());
    }

    #[test]
    fn pr1_submission_retains_original_draft_until_pty_acceptance() {
        let mut ui = ComposerUi::new(test_history_path("pr1-retention"));
        let draft = "한글 prompt\n두번째 줄".to_owned();
        let original = draft.clone();
        assert!(ui.try_submit(&draft, true, TEST_WS).is_some());
        assert_eq!(
            draft, original,
            "staging or stale target rejection must not consume the draft"
        );
        assert!(
            ui.history.is_empty(),
            "history records actual admission, not a Send click"
        );
    }

    #[test]
    fn pr1_host_or_stale_target_rejection_keeps_exact_original_and_allows_manual_retry() {
        let mut ui = ComposerUi::new(test_history_path("pr1-rejection"));
        let original = "한글 😀\nline two".to_owned();
        let draft = original.clone();
        let ComposerAction::Send(submission) = ui.try_submit(&draft, true, TEST_WS).unwrap() else {
            panic!()
        };
        ui.buffers.insert(TEST_WS.to_owned(), draft);
        ui.settle_submission(
            TEST_WS,
            submission.submission_id,
            submission.prompt(),
            PromptAdmissionOutcome::Rejected,
        );
        assert_eq!(ui.current_text(TEST_WS), original);
        assert!(ui.history.is_empty());
        let draft = ui.buffers.remove(TEST_WS).unwrap();
        assert!(ui.try_submit(&draft, true, TEST_WS).is_some());
    }

    #[test]
    fn pr1_acceptance_clears_only_the_original_snapshot_and_records_history_once() {
        let mut ui = ComposerUi::new(test_history_path("pr1-accepted"));
        let draft = "original".to_owned();
        ui.try_submit(&draft, true, TEST_WS).unwrap();
        ui.buffers
            .insert(TEST_WS.to_owned(), "new edit while waiting".to_owned());
        assert!(
            ui.settle_submission(TEST_WS, 1, "original", PromptAdmissionOutcome::Accepted)
                .is_some()
        );
        assert_eq!(ui.current_text(TEST_WS), "new edit while waiting");
        assert_eq!(
            ui.history.iter().map(AsRef::as_ref).collect::<Vec<_>>(),
            vec!["original"]
        );
        assert!(
            ui.settle_submission(TEST_WS, 1, "original", PromptAdmissionOutcome::Accepted)
                .is_none()
        );
    }

    #[test]
    fn pr1_unknown_or_pending_submission_never_automatically_retries() {
        let mut ui = ComposerUi::new(test_history_path("pr1-unknown"));
        let draft = "keep me".to_owned();
        ui.try_submit(&draft, true, TEST_WS).unwrap();
        assert!(ui.try_submit(&draft, true, TEST_WS).is_none());
        ui.settle_submission(TEST_WS, 1, "keep me", PromptAdmissionOutcome::Unknown);
        assert!(ui.try_submit(&draft, true, TEST_WS).is_none());
        assert_eq!(draft, "keep me");
        assert!(ui.history.is_empty());
    }

    #[test]
    fn pr1_late_old_ack_cannot_consume_a_same_text_explicit_retry() {
        let mut ui = ComposerUi::new(test_history_path("pr1-late-ack"));
        let draft = "same text".to_owned();
        ui.try_submit(&draft, true, TEST_WS).unwrap();
        ui.settle_submission(TEST_WS, 1, "same text", PromptAdmissionOutcome::Unknown);
        ui.acknowledge_uncertain_delivery(TEST_WS);
        ui.try_submit(&draft, true, TEST_WS).unwrap();
        ui.buffers.insert(TEST_WS.to_owned(), draft);
        assert!(
            ui.settle_submission(TEST_WS, 1, "same text", PromptAdmissionOutcome::Accepted)
                .is_none()
        );
        assert_eq!(ui.current_text(TEST_WS), "same text");
        assert!(ui.history.is_empty());
        assert!(
            ui.settle_submission(TEST_WS, 2, "same text", PromptAdmissionOutcome::Accepted)
                .is_some()
        );
        assert!(ui.current_text(TEST_WS).is_empty());
    }

    #[test]
    fn designall_composer는_그림자없는_입력표면이다() {
        let frame = composer_frame(&egui::Visuals::dark());
        assert_eq!(frame.shadow, egui::epaint::Shadow::NONE);
        assert_eq!(frame.corner_radius, egui::CornerRadius::same(4));
        assert_eq!(frame.fill, crate::ui::designall::DARK.input_background);
    }

    fn arc_str(value: &str) -> std::sync::Arc<str> {
        std::sync::Arc::from(value)
    }

    fn assert_single_send(actions: &[ComposerAction], expected: &str) {
        assert_eq!(actions.len(), 1, "expected one Send action: {actions:?}");
        let ComposerAction::Send(submission) = &actions[0] else {
            panic!("expected Send action: {:?}", actions[0]);
        };
        assert_eq!(submission.prompt(), expected);
        assert_eq!(
            submission.history().last().map(AsRef::as_ref),
            Some(expected)
        );
        assert!(submission.history().len() <= COMPOSER_HISTORY_MAX_ITEMS);
        assert!(history_bytes(submission.history()) <= COMPOSER_HISTORY_MAX_BYTES);
    }

    // ── 순수 함수 유닛 테스트 ──

    #[test]
    fn encode_prompt_input_은_p6a_규칙을_따른다() {
        // C0 제어문자 strip(\t 제외) + \n→\r + submit CR.
        assert_eq!(
            encode_prompt_input("a\x1bb\tc", true, false),
            Some(b"ab\tc\r".to_vec())
        );
        // 여러 줄 + bracketed → ESC[200~…ESC[201~ wrap, CR은 wrap 밖.
        assert_eq!(
            encode_prompt_input("one\ntwo", true, true),
            Some(b"\x1b[200~one\rtwo\x1b[201~\r".to_vec())
        );
        // 여러 줄 + bracketed OFF → wrap 없음.
        assert_eq!(
            encode_prompt_input("one\ntwo", true, false),
            Some(b"one\rtwo\r".to_vec())
        );
        // \r\n·\r 정규화 후 \r로 통일.
        assert_eq!(
            encode_prompt_input("a\r\nb\rc", false, false),
            Some(b"a\rb\rc".to_vec())
        );
        // 빈 결과 + submit 없음 → None.
        assert_eq!(encode_prompt_input("\x1b", false, false), None);
        // 빈 결과라도 submit이면 CR 한 개.
        assert_eq!(encode_prompt_input("", true, false), Some(b"\r".to_vec()));
        // 한 줄이라도 512B 초과면 붙여넣기로 간주 — bracketed wrap.
        let long = "x".repeat(600);
        let encoded = encode_prompt_input(&long, true, true).unwrap();
        assert!(encoded.starts_with(b"\x1b[200~"));
        assert!(encoded.ends_with(b"\x1b[201~\r"));
    }

    #[test]
    fn tui_paste_submission은_본문과_enter를_별도_write로_나눈다() {
        assert_eq!(
            encode_tui_paste_submission("hello"),
            Some((b"\x1b[200~hello\x1b[201~".to_vec(), b"\r".to_vec(),)),
            "짧은 한 줄도 Codex에는 명시적 paste로 보내야 burst 판정을 피한다"
        );
        assert_eq!(
            encode_tui_paste_submission("one\ntwo"),
            Some((b"\x1b[200~one\ntwo\x1b[201~".to_vec(), b"\r".to_vec(),))
        );
    }

    #[test]
    fn composer_plan은_감지_전_codex도_bracketed_세션이면_명시적_paste로_보낸다() {
        assert_eq!(
            plan_composer_input("hi", true, true, None),
            Some(ComposerInputPlan::BracketedPaste {
                body: b"\x1b[200~hi\x1b[201~".to_vec(),
                submit: b"\r".to_vec(),
            })
        );
        assert_eq!(
            plan_composer_input("hi", true, false, Some(AgentProvider::Codex)),
            Some(ComposerInputPlan::BracketedPaste {
                body: b"\x1b[200~hi\x1b[201~".to_vec(),
                submit: b"\r".to_vec(),
            })
        );
        assert_eq!(
            plan_composer_input("hi", true, false, None),
            Some(ComposerInputPlan::Single(b"hi\r".to_vec()))
        );
    }

    #[test]
    fn push_history_는_연속_중복을_접고_상한을_지킨다() {
        let mut history = Vec::new();
        push_history(&mut history, arc_str("a"));
        push_history(&mut history, arc_str("a")); // 연속 중복 — 접힘
        push_history(&mut history, arc_str("b"));
        push_history(&mut history, arc_str("a")); // 떨어진 중복 — 유지
        assert_eq!(
            history.iter().map(AsRef::as_ref).collect::<Vec<_>>(),
            vec!["a", "b", "a"]
        );

        let mut full = Vec::new();
        for i in 0..150 {
            push_history(&mut full, format!("prompt-{i}").into());
        }
        assert_eq!(full.len(), COMPOSER_HISTORY_MAX_ITEMS);
        assert_eq!(full.first().map(AsRef::as_ref), Some("prompt-50"));
        assert_eq!(full.last().map(AsRef::as_ref), Some("prompt-149"));

        let mut bytes_bounded = Vec::new();
        push_history(&mut bytes_bounded, "x".repeat(600 * 1024).into());
        push_history(&mut bytes_bounded, "y".repeat(600 * 1024).into());
        assert_eq!(bytes_bounded.len(), 1);
        assert!(history_bytes(&bytes_bounded) <= COMPOSER_HISTORY_MAX_BYTES);
    }

    #[test]
    fn send는_bounded_arc_history_snapshot을_공유하고_file을_쓰지_않는다() {
        let path = test_history_path("send-history-intent");
        std::fs::remove_file(&path).ok();
        let mut ui = ComposerUi::new(path.clone());
        let buffer = "hello".to_owned();
        let action = ui.try_submit(&buffer, true, TEST_WS).unwrap();
        let ComposerAction::Send(submission) = action else {
            panic!("expected Send");
        };
        assert_eq!(submission.prompt(), "hello");
        assert!(
            ui.history.is_empty(),
            "history waits for actual PTY admission"
        );
        assert_eq!(
            submission.history().last().map(AsRef::as_ref),
            Some("hello")
        );
        assert!(!path.exists(), "leaf Send는 history file을 쓰지 않는다");

        let oversized = "x".repeat(COMPOSER_PROMPT_MAX_BYTES + 1);
        assert!(ui.try_submit(&oversized, true, TEST_WS).is_none());
        assert_eq!(oversized.len(), COMPOSER_PROMPT_MAX_BYTES + 1);
    }

    #[test]
    fn history_파일_저장_후_재로드_라운드트립() {
        let path = std::env::temp_dir().join(format!(
            "deppy-composer-history-{}-roundtrip.jsonl",
            std::process::id()
        ));
        // 여러 줄 프롬프트도 jsonl(JSON 문자열 이스케이프)로 한 줄에 보존된다.
        let history = vec![
            arc_str("한 줄 프롬프트"),
            arc_str("여러 줄\n프롬프트\n\t탭 포함"),
        ];
        save_history(&path, &history);
        assert_eq!(load_history(&path), history);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn load_history_는_깨진_줄을_건너뛴다() {
        let path = std::env::temp_dir().join(format!(
            "deppy-composer-history-{}-corrupt.jsonl",
            std::process::id()
        ));
        std::fs::write(&path, "\"ok\"\nnot-json\n\"also ok\"\n").unwrap();
        assert_eq!(load_history(&path), vec![arc_str("ok"), arc_str("also ok")]);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn cursor_line_info_는_첫_줄과_마지막_줄을_판정한다() {
        assert_eq!(cursor_line_info("", 0), (true, true));
        assert_eq!(cursor_line_info("one", 1), (true, true));
        let text = "one\ntwo\nthree";
        assert_eq!(cursor_line_info(text, 0), (true, false)); // 첫 줄
        assert_eq!(cursor_line_info(text, 5), (false, false)); // 가운데 줄
        assert_eq!(cursor_line_info(text, 13), (false, true)); // 마지막 줄
    }

    #[test]
    fn insert_snippet_은_경계_공백을_보정하고_커서와_패딩을_기록한다() {
        let mut buffer = String::new();
        assert_eq!(
            insert_snippet(&mut buffer, None, "@a.txt"),
            InsertedSnippet {
                cursor: 6,
                leading_space: false,
                trailing_space: false,
            }
        );
        assert_eq!(buffer, "@a.txt");
        let mut buffer = "fix".to_owned();
        // "fix" 뒤 공백 보정 포함 삽입 조각 " @a.txt"의 끝 = 10.
        assert_eq!(
            insert_snippet(&mut buffer, None, "@a.txt"),
            InsertedSnippet {
                cursor: 10,
                leading_space: true,
                trailing_space: false,
            }
        );
        assert_eq!(buffer, "fix @a.txt");
        let mut buffer = "fix bug".to_owned();
        // "fix|" 커서 뒤가 공백 — 커서(3) + " @a.txt"(7) = 10 → "fix @a.txt| bug".
        assert_eq!(insert_snippet(&mut buffer, Some(3), "@a.txt").cursor, 10);
        assert_eq!(buffer, "fix @a.txt bug");
        // 기존 공백(들여쓰기) 옆이면 패딩을 붙이지 않는다 — 제거 시 그대로 복원 근거.
        let mut buffer = "line\n  ".to_owned();
        assert_eq!(
            insert_snippet(&mut buffer, None, "@a.txt"),
            InsertedSnippet {
                cursor: 13,
                leading_space: false,
                trailing_space: false,
            }
        );
        assert_eq!(buffer, "line\n  @a.txt");
    }

    #[test]
    fn mention_path_는_워크스페이스_루트_기준_상대경로다() {
        let root = Path::new("/proj");
        assert_eq!(
            mention_path(Some(root), Path::new("/proj/src/main.rs")),
            "@src/main.rs"
        );
        // 루트 밖/루트 미지정 → 절대경로 그대로.
        assert_eq!(
            mention_path(Some(root), Path::new("/etc/hosts")),
            "/etc/hosts"
        );
        assert_eq!(mention_path(None, Path::new("/etc/hosts")), "/etc/hosts");
    }

    #[test]
    fn mention_path_는_nfd_입력을_nfc로_합성한다() {
        // macOS 드래그&드롭/파일 피커가 넘기는 NFD(자소분해) 경로 — "한" = ᄒ+ᅡ+ᆫ.
        let nfd_han = "\u{1112}\u{1161}\u{11AB}";
        let root = PathBuf::from(format!("/proj/{nfd_han}"));
        let nfd_file = format!("{nfd_han}.txt");
        let path = root.join(&nfd_file);
        // 루트 상대경로: root/path 양쪽 다 NFD라도 strip_prefix가 원본끼리 비교되어
        // 정상 매칭되고, 표시 문자열만 NFC로 합성된다.
        assert_eq!(
            mention_path(Some(&root), &path),
            format!("@{nfd_file}").nfc().collect::<String>()
        );
        // 절대경로(루트 밖): 표시 문자열도 NFC로 합성된다.
        let outside = PathBuf::from(format!("/etc/{nfd_file}"));
        assert_eq!(
            mention_path(Some(&root), &outside),
            outside.display().to_string().nfc().collect::<String>()
        );
    }

    #[test]
    fn prepend_model_command_는_버퍼_맨_앞에_삽입하고_커서를_개행_뒤로_돌려준다() {
        let mut buffer = "프롬프트".to_owned();
        // "/model opus\n" = 12자 — 커서가 개행 뒤라 다음 타이핑이 커맨드를 깨지 않는다.
        assert_eq!(prepend_model_command(&mut buffer, "opus"), 12);
        assert_eq!(buffer, "/model opus\n프롬프트");
    }

    #[test]
    fn joined_mentions_는_순서를_보존한다() {
        let paths = [PathBuf::from("/x/1.png"), PathBuf::from("/x/2.png")];
        assert_eq!(joined_mentions(None, &paths), "/x/1.png /x/2.png");
    }

    #[test]
    fn 첨부_path_payload는_item과_byte_상한을_지킨다() {
        let exact = (0..COMPOSER_ATTACHMENT_MAX_ITEMS)
            .map(|index| PathBuf::from(format!("/x/{index}.png")))
            .collect::<Vec<_>>();
        assert_eq!(attachment_path_bytes(&exact), Some(134));
        assert!(ClipboardAttachmentPayload::try_new(exact).is_ok());

        let over_items = (0..=COMPOSER_ATTACHMENT_MAX_ITEMS)
            .map(|index| PathBuf::from(format!("/x/{index}.png")))
            .collect::<Vec<_>>();
        assert_eq!(attachment_path_bytes(&over_items), None);
        assert_eq!(attachment_path_bytes(&[]), None);
        assert_eq!(
            attachment_path_bytes(&[PathBuf::from("x".repeat(CONTEXT_FILE_PATH_MAX_BYTES + 1),)]),
            None
        );
        let over_bytes = (0..COMPOSER_ATTACHMENT_MAX_ITEMS)
            .map(|_| PathBuf::from("x".repeat(20 * 1024)))
            .collect::<Vec<_>>();
        assert_eq!(attachment_path_bytes(&over_bytes), None);
    }

    /// 터미널(ui/workspace.rs is_clipboard_paste_shortcut) 테스트와 같은 cfg 분기 검증:
    /// 비-macOS는 Ctrl+Shift+V press가 첨부 트리거, macOS는 native monitor가 주 경로.
    #[test]
    fn 비_macos_첨부_트리거는_ctrl_shift_v_press다() {
        let key = |pressed: bool, ctrl: bool, shift: bool| egui::Event::Key {
            key: egui::Key::V,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers: egui::Modifiers {
                ctrl,
                shift,
                ..Default::default()
            },
        };
        if cfg!(target_os = "macos") {
            assert!(
                !is_attach_paste_shortcut(&key(true, true, true)),
                "macOS는 AppKit native monitor가 주 경로 — 폴백 미사용"
            );
        } else {
            assert!(is_attach_paste_shortcut(&key(true, true, true)));
        }
        // release/보조키 불일치는 어느 플랫폼에서도 트리거가 아니다.
        assert!(!is_attach_paste_shortcut(&key(false, true, true)));
        assert!(!is_attach_paste_shortcut(&key(true, true, false)));
        assert!(!is_attach_paste_shortcut(&key(true, false, true)));
    }

    fn complete_attachment_for_draft(
        ui: &mut ComposerUi,
        egui_ctx: &egui::Context,
        request: ClipboardAttachmentRequest,
        paths: Option<Vec<PathBuf>>,
        draft: &mut String,
    ) -> bool {
        let workspace_id = request.target.workspace_id.clone();
        let payload = paths.and_then(|paths| ClipboardAttachmentPayload::try_new(paths).ok());
        ui.buffers
            .insert(workspace_id.clone(), std::mem::take(draft));
        let completed = ui.complete_clipboard_attachment(egui_ctx, request, payload, &workspace_id);
        *draft = ui.buffers.remove(&workspace_id).unwrap_or_default();
        completed
    }

    /// codex P2 회귀: 변환이 도는 동안 사용자가 타이핑해도 완료 결과는 **토큰 자리**에
    /// 들어간다 — 커서 인덱스 앵커였다면 옛 위치에 삽입돼 입력 순서가 섞였다.
    #[test]
    fn 첨부_pending_중_타이핑해도_결과는_토큰_자리에_들어간다() {
        let egui_ctx = egui::Context::default();
        let path = test_history_path("attach-token-typing");
        let mut ui = ComposerUi::new(path.clone());
        let mut active = String::new();
        let request = ui
            .begin_attachment_request(&egui_ctx, TEST_WS, None, &mut active)
            .unwrap();
        let token = request.target.token.clone();
        assert_eq!(active, token, "토큰이 동기로 삽입돼야 한다");
        // codex P2(7차): 삽입 캐럿은 예약이 아니라 **즉시** TextEditState에 — 같은
        // 프레임에 배치된 Text 이벤트가 토큰 뒤에서 시작해야 한다.
        assert_eq!(
            stored_char_range(&egui_ctx, ComposerUi::text_id(TEST_WS)),
            Some((token.chars().count(), token.chars().count())),
            "동기 삽입 캐럿은 show 전에 즉시 토큰 뒤로 저장된다"
        );
        assert_eq!(ui.pending_cursor, None, "post-show 예약을 남기지 않는다");
        // 변환 중 사용자 입력 — 토큰 앞뒤로 타이핑.
        active = format!("before {active} after");
        assert!(complete_attachment_for_draft(
            &mut ui,
            &egui_ctx,
            request,
            Some(vec![PathBuf::from("/x/shot.png")]),
            &mut active,
        ));
        assert_eq!(active, "before /x/shot.png after");
        assert!(ui.pending_attachment.is_none());
        std::fs::remove_file(&path).ok();
    }

    /// codex P1 회귀: 치환은 길이를 바꾼다 — 캐럿이 토큰 뒤에 있었으면 델타만큼
    /// 리베이스돼야 다음 타이핑이 경로 안에 박히지 않는다. 치환은 show **전**이므로
    /// 리베이스는 post-show 예약이 아니라 TextEditState에 **즉시** 반영돼야 같은
    /// 프레임의 타이핑이 올바른 캐럿을 쓴다(codex P1, 4차).
    #[test]
    fn 토큰_치환_시_토큰_뒤_캐럿이_델타만큼_즉시_이동한다() {
        let egui_ctx = egui::Context::default();
        let path = test_history_path("attach-caret-rebase");
        let mut ui = ComposerUi::new(path.clone());
        let mut active = String::new();
        let request = ui
            .begin_attachment_request(&egui_ctx, TEST_WS, None, &mut active)
            .unwrap();
        // 사용자가 토큰 **뒤에** 타이핑, 캐럿은 끝.
        active.push_str(" tail");
        ui.pending_cursor = Some(active.chars().count());
        assert!(complete_attachment_for_draft(
            &mut ui,
            &egui_ctx,
            request,
            Some(vec![PathBuf::from("/x/shot.png")]),
            &mut active,
        ));
        assert_eq!(active, "/x/shot.png tail");
        assert_eq!(
            cursor_char_index(&egui_ctx, ComposerUi::text_id(TEST_WS)),
            Some(active.chars().count()),
            "끝에 있던 캐럿은 델타만큼 밀려 TextEditState에 즉시(pre-show) 반영돼야 한다"
        );
        assert_eq!(
            ui.pending_cursor, None,
            "낡은 post-show 예약이 즉시 반영한 캐럿을 덮으면 안 된다"
        );
        std::fs::remove_file(&path).ok();
    }

    /// App host의 늦은 첫 결과는 새 continuation 자리에 적용되면 안 된다.
    #[test]
    fn 새_첨부가_이전_요청을_대체하고_stale_completion은_무시된다() {
        let egui_ctx = egui::Context::default();
        let path = test_history_path("attach-stale-replace");
        let mut ui = ComposerUi::new(path.clone());
        let mut active = String::new();
        let old = ui
            .begin_attachment_request(&egui_ctx, TEST_WS, None, &mut active)
            .unwrap();
        let old_token = old.target.token.clone();
        let new = ui
            .begin_attachment_request(&egui_ctx, TEST_WS, None, &mut active)
            .unwrap();
        assert_ne!(old.request_id(), new.request_id());
        assert!(!active.contains(&old_token));
        assert_eq!(active, new.token());

        ui.buffers.insert(TEST_WS.to_owned(), active);
        assert!(!ui.complete_clipboard_attachment(
            &egui_ctx,
            old,
            Some(ClipboardAttachmentPayload::try_new(vec![PathBuf::from("/x/stale.png")]).unwrap()),
            TEST_WS,
        ));
        assert_eq!(ui.buffers[TEST_WS], new.token());
        assert!(ui.complete_clipboard_attachment(&egui_ctx, new, None, TEST_WS));
        assert!(ui.buffers[TEST_WS].is_empty());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn rebase_caret_은_치환_구간_기준으로_이동한다() {
        // 구간(5..15, 결과 3자) 앞: 그대로 / 뒤: 델타(-7) / 안: 결과 끝(8) 스냅.
        assert_eq!(rebase_caret(2, 5, 10, 3), 2);
        assert_eq!(rebase_caret(20, 5, 10, 3), 13);
        assert_eq!(rebase_caret(15, 5, 10, 3), 8);
        assert_eq!(rebase_caret(7, 5, 10, 3), 8);
    }

    /// codex P2 회귀: pending 중 워크스페이스를 전환해도 결과는 **원 드래프트의 토큰**을
    /// 치환하고 활성 버퍼는 건드리지 않는다. 치환은 문자열 기반이라 돌아가서 타이핑해도
    /// 멘션 안에 박히지 않는다(토큰 삽입 시점에 커서가 이미 토큰 뒤로 이동).
    #[test]
    fn 첨부_pending_중_전환해도_원_드래프트의_토큰이_치환된다() {
        let egui_ctx = egui::Context::default();
        let path = test_history_path("attach-token-switch");
        let mut ui = ComposerUi::new(path.clone());
        // ws-a가 활성일 때 첨부 시작 (드래프트에 이미 내용).
        ui.sync_workspace(&egui_ctx, "ws-a");
        let mut draft_a = "a-draft".to_owned();
        let request = ui
            .begin_attachment_request(&egui_ctx, "ws-a", None, &mut draft_a)
            .unwrap();
        let token = request.target.token.clone();
        assert_eq!(draft_a, format!("a-draft {token}"));
        // ws-a의 TextEdit 캐럿(끝) — 실제로는 위젯이 매 프레임 저장한다.
        let caret_a = draft_a.chars().count();
        let ws_a_id = ComposerUi::text_id("ws-a");
        let mut state = egui::text_edit::TextEditState::default();
        state
            .cursor
            .set_char_range(Some(egui::text::CCursorRange::one(
                egui::text::CCursor::new(caret_a),
            )));
        state.store(&egui_ctx, ws_a_id);
        // 전환: render()가 하듯 ws-a 드래프트는 맵으로 돌아가고 ws-b가 활성이 된다.
        ui.buffers.insert("ws-a".to_owned(), draft_a);
        ui.sync_workspace(&egui_ctx, "ws-b");
        let active_b = "b-draft".to_owned();
        assert!(ui.complete_clipboard_attachment(
            &egui_ctx,
            request,
            Some(ClipboardAttachmentPayload::try_new(vec![PathBuf::from("/x/shot.png")]).unwrap()),
            "ws-b",
        ));
        assert_eq!(active_b, "b-draft", "활성(ws-b) 버퍼는 무접촉이어야 한다");
        assert_eq!(
            ui.buffers.get("ws-a").unwrap(),
            "a-draft /x/shot.png",
            "원 드래프트의 토큰 자리가 치환돼야 한다"
        );
        // codex P1: 비활성 치환의 캐럿 보정은 맵에 예약됐다가 재활성(sync) 시점에
        // TextEditState로 적용된다 — 끝(토큰 뒤)에 있던 캐럿은 델타만큼 밀려 여전히 끝.
        let expected = ui.buffers.get("ws-a").unwrap().chars().count();
        assert_eq!(ui.pending_caret.get("ws-a"), Some(&(expected, expected)));
        ui.sync_workspace(&egui_ctx, "ws-a");
        assert_eq!(
            stored_char_range(&egui_ctx, ws_a_id),
            Some((expected, expected)),
            "재활성 프레임 show 전에 캐럿이 즉시 반영돼야 한다"
        );
        assert!(ui.pending_caret.is_empty(), "예약은 적용 후 비워진다");
        std::fs::remove_file(&path).ok();
    }

    /// codex 권장 취소 의미론: 사용자가 토큰을 지웠으면 결과를 조용히 버린다.
    /// 실패(이미지 아님 등)도 토큰을 남기지 않는다.
    #[test]
    fn 토큰을_지우면_첨부_결과를_버리고_실패도_토큰을_제거한다() {
        let egui_ctx = egui::Context::default();
        let path = test_history_path("attach-token-cancel");
        let mut ui = ComposerUi::new(path.clone());
        // 취소: 토큰 삭제 후 완료 도착 → 폐기.
        let mut active = String::new();
        let request = ui
            .begin_attachment_request(&egui_ctx, TEST_WS, None, &mut active)
            .unwrap();
        active.clear(); // 사용자가 토큰을 지움
        assert!(!complete_attachment_for_draft(
            &mut ui,
            &egui_ctx,
            request,
            Some(vec![PathBuf::from("/x/shot.png")]),
            &mut active,
        ));
        assert!(active.is_empty(), "토큰이 없으면 결과를 조용히 버린다");
        // 실패(이미지/파일 아님): 토큰 제거.
        let request = ui
            .begin_attachment_request(&egui_ctx, TEST_WS, None, &mut active)
            .unwrap();
        assert!(!active.is_empty());
        assert!(complete_attachment_for_draft(
            &mut ui,
            &egui_ctx,
            request,
            None,
            &mut active,
        ));
        assert!(active.is_empty(), "실패해도 플레이스홀더를 남기지 않는다");
        std::fs::remove_file(&path).ok();
    }

    /// codex P2 회귀(5차): 토큰이 사용자 공백/들여쓰기 옆이면 삽입이 패딩을 붙이지
    /// 않았으므로 실패 제거도 공백을 걷으면 안 된다 — 인접 공백 추정은 실패한
    /// 붙여넣기가 프롬프트(들여쓰기)를 변형하게 만든다.
    #[test]
    fn 실패한_첨부_제거는_기록된_패딩만_걷어_사용자_공백을_보존한다() {
        let egui_ctx = egui::Context::default();
        let path = test_history_path("attach-padding-preserve");
        let mut ui = ComposerUi::new(path.clone());
        // 들여쓰기 2칸 끝에 커서 — 삽입은 패딩 없이 붙는다.
        let mut active = "line\n  ".to_owned();
        ui.pending_cursor = Some(active.chars().count());
        let request = ui
            .begin_attachment_request(&egui_ctx, TEST_WS, None, &mut active)
            .unwrap();
        assert_eq!(
            request.target.padding,
            (false, false),
            "기존 공백 옆 삽입은 패딩을 기록하지 않는다"
        );
        assert!(complete_attachment_for_draft(
            &mut ui,
            &egui_ctx,
            request,
            None,
            &mut active,
        ));
        assert_eq!(active, "line\n  ", "사용자 들여쓰기가 그대로 보존돼야 한다");
        std::fs::remove_file(&path).ok();
    }

    /// codex P2 회귀(5차): 전송 코드 가림 판정은 dispatcher(take_triggered_action)와
    /// 같은 matches_exact 규칙 — 비-macOS 리바인드 캡처는 물리 Ctrl이 CTRL|COMMAND로
    /// 저장될 수 있어 구조(Eq) 비교로는 놓친다.
    #[test]
    fn 전송_코드_가림_판정은_dispatcher와_같은_모디파이어_규칙을_쓴다() {
        let chord = |m| egui::KeyboardShortcut::new(m, egui::Key::Enter);
        let ctrl_and_command = egui::Modifiers {
            ctrl: true,
            command: true,
            ..Default::default()
        };
        const NON_MACOS: bool = true; // fold_ctrl_command_alias — 인자화로 양 플랫폼 검증
        const MACOS: bool = false;
        // ── 비-macOS(별칭 접기, codex P2 6차): 물리 Ctrl 이벤트는 ctrl|command를
        // 둘 다 켜 — Command+Enter 바인딩과 Ctrl+Enter 전송이 서로는 매치 안 돼도
        // 같은 이벤트를 만족한다.
        assert!(
            shadows_send_chord(&chord(ctrl_and_command), egui::Modifiers::CTRL, NON_MACOS),
            "비-macOS 캡처(CTRL|COMMAND)는 Ctrl+Enter 전송을 가린다"
        );
        assert!(
            shadows_send_chord(
                &chord(egui::Modifiers::COMMAND),
                egui::Modifiers::CTRL,
                NON_MACOS
            ),
            "비-macOS Command+Enter 바인딩은 Ctrl+Enter 전송을 가린다(별칭 접기)"
        );
        assert!(shadows_send_chord(
            &chord(egui::Modifiers::CTRL),
            egui::Modifiers::COMMAND,
            NON_MACOS
        ));
        // ── macOS(구분 복원, codex P2 7차 회귀): Cmd와 Ctrl은 다른 물리 키 — 접으면
        // Cmd+Enter 바인딩 vs Ctrl+Enter 전송을 충돌로 오판해 접힘 단축키가 죽는다.
        assert!(
            !shadows_send_chord(
                &chord(egui::Modifiers::COMMAND),
                egui::Modifiers::CTRL,
                MACOS
            ),
            "macOS에선 Cmd+Enter 바인딩이 Ctrl+Enter 전송과 다른 코드다"
        );
        assert!(
            shadows_send_chord(
                &chord(egui::Modifiers::COMMAND),
                egui::Modifiers::COMMAND,
                MACOS
            ),
            "같은 Cmd+Enter는 여전히 가린다"
        );
        assert!(
            shadows_send_chord(&chord(ctrl_and_command), egui::Modifiers::CTRL, MACOS),
            "두 플래그가 켜진 캡처는 matches_exact 양방향으로 여전히 잡힌다(5차)"
        );
        // ── 공통: shift/alt 차이와 비-Enter 키는 어느 플랫폼에서도 가리지 않는다.
        for platform in [NON_MACOS, MACOS] {
            assert!(
                !shadows_send_chord(
                    &chord(egui::Modifiers::CTRL | egui::Modifiers::SHIFT),
                    egui::Modifiers::CTRL,
                    platform
                ),
                "Shift가 다르면 다른 코드다"
            );
            assert!(
                !shadows_send_chord(
                    &egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::J),
                    egui::Modifiers::COMMAND,
                    platform
                ),
                "Enter가 아니면 가리지 않는다"
            );
        }
    }

    /// codex P2 회귀(5차): 첨부 완료 치환이 사용자 선택을 무너뜨리면 안 된다 —
    /// 양 끝점을 각각 리베이스해 범위와 방향(primary/secondary)을 보존한다.
    #[test]
    fn 첨부_완료가_선택_범위와_방향을_보존한다() {
        let egui_ctx = egui::Context::default();
        let path = test_history_path("attach-selection");
        let mut ui = ComposerUi::new(path.clone());
        let mut active = String::new();
        let request = ui
            .begin_attachment_request(&egui_ctx, TEST_WS, None, &mut active)
            .unwrap();
        // 사용자가 앞에 타이핑 + "ello"를 역방향 선택(primary=1 < secondary=5).
        active = format!("hello {active}");
        ui.pending_cursor = None; // 예약은 전 프레임에 이미 적용된 상태를 시뮬레이션
        let text_id = ComposerUi::text_id(TEST_WS);
        store_caret_range_now(&egui_ctx, text_id, 1, 5);
        assert!(complete_attachment_for_draft(
            &mut ui,
            &egui_ctx,
            request,
            Some(vec![PathBuf::from("/x/a.png")]),
            &mut active,
        ));
        assert_eq!(active, "hello /x/a.png");
        assert_eq!(
            stored_char_range(&egui_ctx, text_id),
            Some((1, 5)),
            "치환 구간 앞의 선택은 범위·방향 그대로여야 한다"
        );
        std::fs::remove_file(&path).ok();
    }

    /// codex P2 회귀(3차): 연타 교체는 옛 토큰을 **먼저** 제거하고(패딩 공백 포함 —
    /// 이중 공백 방지) 그 다음 리베이스된 캐럿에 새 토큰을 삽입한다 — 제거 전에 캡처한
    /// 커서 인덱스를 재사용하면 낡은 위치에 삽입된다.
    #[test]
    fn 진행_중_첨부는_최신_요청으로_대체되고_옛_토큰은_제거된다() {
        let egui_ctx = egui::Context::default();
        let path = test_history_path("attach-replace");
        let mut ui = ComposerUi::new(path.clone());
        // 중간 위치 삽입 시나리오 — 캐럿이 "fix|"(3) 지점.
        let mut active = "fix bug".to_owned();
        ui.pending_cursor = Some(3);
        let old = ui
            .begin_attachment_request(&egui_ctx, TEST_WS, None, &mut active)
            .unwrap();
        let old_token = old.target.token.clone();
        assert_eq!(active, format!("fix {old_token} bug"));
        // 워커가 느린 동안의 두 번째 ⌘V — 조용히 버리지 않고 최신 것으로 대체
        // (터미널 PendingPaste 관례, codex P2).
        let new = ui
            .begin_attachment_request(&egui_ctx, TEST_WS, None, &mut active)
            .unwrap();
        let new_token = new.target.token.clone();
        assert_ne!(old_token, new_token);
        assert!(
            !active.contains(&old_token),
            "대체된 태스크의 토큰은 제거돼야 한다"
        );
        assert_eq!(
            active,
            format!("fix {new_token} bug"),
            "새 토큰은 (제거로 시프트된) 캐럿 자리에 들어가야 한다"
        );
        assert!(
            !active.contains("  "),
            "옛 토큰 제거가 이중 공백을 남기면 안 된다"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn 워크스페이스_전환이_히스토리_탐색과_커서_예약을_리셋한다() {
        let egui_ctx = egui::Context::default();
        let path = test_history_path("ws-switch");
        let mut ui = ComposerUi::new(path.clone());
        ui.sync_workspace(&egui_ctx, "ws-a");
        ui.history_pos = Some(1);
        ui.pending_cursor = Some(3);
        ui.sync_workspace(&egui_ctx, "ws-a"); // 같은 워크스페이스 — 유지
        assert_eq!(ui.history_pos, Some(1));
        assert_eq!(ui.pending_cursor, Some(3));
        // 전환 — 이전 문맥의 탐색 위치를 물려받지 않는다(codex P3)
        ui.sync_workspace(&egui_ctx, "ws-b");
        assert!(ui.history_pos.is_none());
        assert!(ui.pending_cursor.is_none());
        std::fs::remove_file(&path).ok();
    }

    // ── kittest 상호작용 테스트 (inbox_waiting.rs 관례 — type_text 전 click 포커스 필수) ──

    const TEST_WS: &str = "ws-test";

    fn test_history_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "deppy-composer-kittest-{}-{name}.jsonl",
            std::process::id()
        ))
    }

    /// 기본 접힘 단축키(⌘J) — 실제 앱의 FocusComposer 기본 바인딩과 동일.
    fn cmd_j() -> Option<egui::KeyboardShortcut> {
        Some(egui::KeyboardShortcut::new(
            egui::Modifiers::COMMAND,
            egui::Key::J,
        ))
    }

    type ComposerHarnessState = (
        ComposerUi,
        Vec<ComposerAction>,
        connector_contract::ConnectorSnapshot,
        Option<PathBuf>,
    );

    fn composer_harness<'a>(
        catalog: &'a i18n::Catalog,
        send_key: ComposerSendKey,
        history_path: PathBuf,
    ) -> egui_kittest::Harness<'a, ComposerHarnessState> {
        composer_harness_with_collapse(catalog, send_key, history_path, cmd_j())
    }

    fn composer_harness_with_collapse<'a>(
        catalog: &'a i18n::Catalog,
        send_key: ComposerSendKey,
        history_path: PathBuf,
        collapse_shortcut: Option<egui::KeyboardShortcut>,
    ) -> egui_kittest::Harness<'a, ComposerHarnessState> {
        egui_kittest::Harness::new_ui_state(
            move |ui, (widget, captured, connector_snapshot, workspace_root): &mut ComposerHarnessState| {
                let ctx = ComposerContext {
                    workspace_id: TEST_WS,
                    draft_key: TEST_WS,
                    runtime_generation: 1,
                    send_key,
                    can_send: true,
                    agent: None,
                    workspace_root: workspace_root.as_deref(),
                    collapse_shortcut,
                    connector_snapshot,
                };
                if let Some(action) = widget.render(ui, catalog, &ctx) {
                    captured.push(action);
                }
            },
            (
                ComposerUi::new(history_path),
                Vec::new(),
                connector_contract::ConnectorSnapshot::default(),
                None,
            ),
        )
    }

    fn buffer_of(harness: &egui_kittest::Harness<'_, ComposerHarnessState>) -> String {
        harness
            .state()
            .0
            .buffers
            .get(TEST_WS)
            .cloned()
            .unwrap_or_default()
    }

    fn focus_composer(harness: &mut egui_kittest::Harness<'_, ComposerHarnessState>) {
        use egui_kittest::kittest::Queryable;
        harness
            .get_by_role(egui::accesskit::Role::MultilineTextInput)
            .click();
        harness.run();
    }

    #[test]
    fn composer_nonempty_collapsed_draft_keeps_explicit_send_control() {
        use egui_kittest::kittest::Queryable;
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let path = test_history_path("collapsed-send");
        let mut harness = composer_harness(&catalog, ComposerSendKey::Enter, path.clone());
        harness.state_mut().0.buffers.insert(
            TEST_WS.to_owned(),
            "한글 첫 줄\n두 번째 줄\n세 번째 줄".to_owned(),
        );
        harness.run();
        assert!(!harness.state().0.expanded);
        harness.get_by_label("↑").click();
        harness.run();
        assert_single_send(&harness.state().1, "한글 첫 줄\n두 번째 줄\n세 번째 줄");
        assert_eq!(buffer_of(&harness), "한글 첫 줄\n두 번째 줄\n세 번째 줄");
        let ComposerAction::Send(submission) = harness.state_mut().1.pop().unwrap() else {
            panic!()
        };
        let (prompt, _, submission_id) = submission.into_parts();
        harness.state_mut().0.settle_submission(
            TEST_WS,
            submission_id,
            &prompt,
            PromptAdmissionOutcome::Rejected,
        );
        harness.state_mut().0.expanded = false;
        harness
            .ctx
            .memory_mut(|memory| memory.surrender_focus(ComposerUi::text_id(TEST_WS)));
        harness.run();
        harness.get_by_label(&catalog.t("composer.delivery.rejected", &[]));
        assert_eq!(buffer_of(&harness), "한글 첫 줄\n두 번째 줄\n세 번째 줄");
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn kittest_enter_전송_시_send와_수용전_버퍼_보존() {
        use egui_kittest::kittest::Queryable;
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let path = test_history_path("enter-send");
        let mut harness = composer_harness(&catalog, ComposerSendKey::Enter, path.clone());
        focus_composer(&mut harness);
        harness
            .get_by_role(egui::accesskit::Role::MultilineTextInput)
            .type_text("hello agent");
        harness.run();
        harness.key_press(egui::Key::Enter);
        harness.run();
        assert_single_send(&harness.state().1, "hello agent");
        assert_eq!(
            buffer_of(&harness),
            "hello agent",
            "PTY 수용 전에는 초안을 보존한다"
        );
        assert!(harness.state().0.history.is_empty());
        assert!(
            !path.exists(),
            "Send render 경로는 history 파일을 생성/기록하면 안 된다"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn kittest_shift_enter_는_개행만_한다() {
        use egui_kittest::kittest::Queryable;
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let path = test_history_path("shift-enter");
        let mut harness = composer_harness(&catalog, ComposerSendKey::Enter, path.clone());
        focus_composer(&mut harness);
        harness
            .get_by_role(egui::accesskit::Role::MultilineTextInput)
            .type_text("a");
        harness.run();
        harness.key_press_modifiers(egui::Modifiers::SHIFT, egui::Key::Enter);
        harness.run();
        assert!(harness.state().1.is_empty(), "Shift+Enter는 전송이 아니다");
        assert_eq!(buffer_of(&harness), "a\n");
        std::fs::remove_file(&path).ok();
    }

    /// codex P1 회귀: 첨부 변환 중(토큰 pending) Enter가 리터럴 `⟦attach-N⟧`을 전송하면
    /// 안 된다 — 완료로 토큰이 치환되면 전송이 자연 복귀한다.
    #[test]
    fn kittest_첨부_pending_중_enter는_전송되지_않는다() {
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let path = test_history_path("attach-pending-enter");
        let mut harness = composer_harness(&catalog, ComposerSendKey::Enter, path.clone());
        focus_composer(&mut harness);
        // pending continuation 구성 — clipboard/file IO 없이 request만 만든다.
        let request = {
            let side_ctx = egui::Context::default();
            let ui = &mut harness.state_mut().0;
            let mut buffer = ui.buffers.remove(TEST_WS).unwrap_or_default();
            let request = ui
                .begin_attachment_request(&side_ctx, TEST_WS, None, &mut buffer)
                .unwrap();
            ui.buffers.insert(TEST_WS.to_owned(), buffer);
            request
        };
        harness.run();
        harness.key_press(egui::Key::Enter);
        harness.run();
        assert!(
            harness.state().1.is_empty(),
            "토큰 pending 중 전송은 차단돼야 한다"
        );
        assert!(
            !buffer_of(&harness).is_empty(),
            "차단된 전송이 버퍼(토큰)를 지우면 안 된다"
        );
        // 완료 → 토큰 치환 → 전송 허용 복귀.
        let egui_ctx = harness.ctx.clone();
        assert!(harness.state_mut().0.complete_clipboard_attachment(
            &egui_ctx,
            request,
            Some(ClipboardAttachmentPayload::try_new(vec![PathBuf::from("/x/shot.png")]).unwrap()),
            TEST_WS,
        ));
        assert_eq!(buffer_of(&harness), "/x/shot.png");
        harness.key_press(egui::Key::Enter);
        harness.run();
        assert_single_send(&harness.state().1, "/x/shot.png");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn kittest_빈_enter_는_무시된다() {
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let path = test_history_path("empty-enter");
        let mut harness = composer_harness(&catalog, ComposerSendKey::Enter, path.clone());
        focus_composer(&mut harness);
        harness.key_press(egui::Key::Enter);
        harness.run();
        assert!(harness.state().1.is_empty(), "빈 내용 전송은 무시해야 한다");
        assert!(
            buffer_of(&harness).is_empty(),
            "소비된 Enter가 개행이 되면 안 된다"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn kittest_cmd_enter_모드에서_enter는_개행_cmd_enter는_전송() {
        use egui_kittest::kittest::Queryable;
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let path = test_history_path("cmd-enter");
        let mut harness = composer_harness(&catalog, ComposerSendKey::CmdEnter, path.clone());
        focus_composer(&mut harness);
        harness
            .get_by_role(egui::accesskit::Role::MultilineTextInput)
            .type_text("a");
        harness.run();
        harness.key_press(egui::Key::Enter);
        harness.run();
        assert!(
            harness.state().1.is_empty(),
            "⌘Enter 모드의 Enter는 개행이다"
        );
        assert_eq!(buffer_of(&harness), "a\n");
        harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Enter);
        harness.run();
        assert_single_send(&harness.state().1, "a\n");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn kittest_cmd_j_로_펼치고_다시_접는다() {
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let path = test_history_path("cmd-j");
        let mut harness = composer_harness(&catalog, ComposerSendKey::Enter, path.clone());
        assert!(!harness.state().0.expanded, "초기 상태는 접힘이어야 한다");
        // 전역 ⌘J(App 단축키 레지스트리) → request_focus → 펼침 + 포커스.
        harness.state_mut().0.request_focus();
        harness.run();
        harness.run(); // request_focus 다음 프레임에 memory 포커스 반영
        assert!(harness.state().0.expanded, "⌘J 후 펼쳐져야 한다");
        // 포커스 상태에서 ⌘J → 컴포저가 직접 소비해 접는다.
        harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::J);
        harness.run();
        assert!(!harness.state().0.expanded, "포커스 중 ⌘J는 접어야 한다");
        std::fs::remove_file(&path).ok();
    }

    /// codex P1 회귀: 접힘 프레임에 포커스를 반납하면 같은 프레임 뒤에 렌더되는 터미널
    /// raw 루프가 접힘 키(비-macOS Ctrl+J=LF)를 실행한다 — 반납은 다음 프레임이어야 한다.
    #[test]
    fn kittest_접힘_시_포커스_반납은_다음_프레임으로_지연된다() {
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let path = test_history_path("deferred-surrender");
        let mut harness = composer_harness(&catalog, ComposerSendKey::Enter, path.clone());
        focus_composer(&mut harness);
        let text_id = ComposerUi::text_id(TEST_WS);
        assert!(harness.ctx.memory(|m| m.has_focus(text_id)));
        // key_press류는 큐 이벤트당 1프레임씩 도는 step()이라 프레임 관측이 안 된다 —
        // key-down을 다음 프레임 RawInput에 직접 넣어 **정확히 한 프레임**만 돌린다.
        harness.input_mut().events.push(egui::Event::Key {
            key: egui::Key::J,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        harness.step();
        assert!(!harness.state().0.expanded, "접힘 자체는 즉시다");
        assert!(
            harness.ctx.memory(|m| m.has_focus(text_id)),
            "접힘 프레임엔 아직 포커스를 쥐고 있어야 한다 — 터미널이 그 프레임 raw 입력을 먹지 않게"
        );
        // codex P2(6차): 릴리스 없이 코드를 **누르고 있는 동안**은 반납을 보류한다 —
        // 반납해 버리면 이후 key-repeat이 터미널 raw 경로에서 실행된다(비-macOS Ctrl+J=LF).
        harness.step();
        assert!(
            harness.ctx.memory(|m| m.has_focus(text_id)),
            "키를 누르고 있는 동안(keys_down)은 반납을 보류해야 한다"
        );
        harness.input_mut().events.push(egui::Event::Key {
            key: egui::Key::J,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        harness.step();
        assert!(
            !harness.ctx.memory(|m| m.has_focus(text_id)),
            "릴리스 후 프레임에 포커스를 반납해 터미널이 회수한다"
        );
        assert!(
            !harness.state().0.expanded,
            "반납 프레임에 다시 펼쳐지면 안 된다"
        );
        std::fs::remove_file(&path).ok();
    }

    /// codex P2 회귀: 접힘 단축키는 FocusComposer의 유효 바인딩을 따른다 — 리바인드하면
    /// 옛 ⌘J는 무시되고 새 키가 접는다. 비활성(None)이면 접힘 단축키도 없다.
    #[test]
    fn kittest_접힘_단축키는_유효_바인딩을_따른다() {
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        // 리바인드: Ctrl+K.
        let path = test_history_path("collapse-rebound");
        let rebound = Some(egui::KeyboardShortcut::new(
            egui::Modifiers::CTRL,
            egui::Key::K,
        ));
        let mut harness =
            composer_harness_with_collapse(&catalog, ComposerSendKey::Enter, path.clone(), rebound);
        focus_composer(&mut harness);
        harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::J);
        harness.run();
        assert!(
            harness.state().0.expanded,
            "리바인드 후 옛 ⌘J는 접지 않아야 한다"
        );
        harness.key_press_modifiers(egui::Modifiers::CTRL, egui::Key::K);
        harness.run();
        assert!(
            !harness.state().0.expanded,
            "새 바인딩(Ctrl+K)이 접어야 한다"
        );
        std::fs::remove_file(&path).ok();

        // 비활성: 어떤 키도 접지 않는다.
        let path = test_history_path("collapse-disabled");
        let mut harness =
            composer_harness_with_collapse(&catalog, ComposerSendKey::Enter, path.clone(), None);
        focus_composer(&mut harness);
        harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::J);
        harness.run();
        assert!(
            harness.state().0.expanded,
            "바인딩 비활성이면 접힘 단축키도 없다"
        );
        std::fs::remove_file(&path).ok();
    }

    /// codex P2 회귀(4차): FocusComposer를 전송 코드(⌘Enter)로 리바인드해도 **전송이
    /// 우선**한다 — 접힘이 먼저 소비하면 키보드 전송이 불가능해진다.
    #[test]
    fn kittest_접힘_바인딩이_전송_코드와_같으면_전송이_우선한다() {
        use egui_kittest::kittest::Queryable;
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let path = test_history_path("collapse-vs-send");
        let cmd_enter = Some(egui::KeyboardShortcut::new(
            egui::Modifiers::COMMAND,
            egui::Key::Enter,
        ));
        let mut harness = composer_harness_with_collapse(
            &catalog,
            ComposerSendKey::CmdEnter,
            path.clone(),
            cmd_enter,
        );
        focus_composer(&mut harness);
        harness
            .get_by_role(egui::accesskit::Role::MultilineTextInput)
            .type_text("hello");
        harness.run();
        harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Enter);
        harness.run();
        assert_single_send(&harness.state().1, "hello");
        assert!(
            harness.state().0.expanded,
            "전송이 우선했으므로 접히지 않아야 한다"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn kittest_히스토리_위아래_탐색() {
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let path = test_history_path("history-nav");
        save_history(&path, &[arc_str("one"), arc_str("two")]);
        let mut harness = composer_harness(&catalog, ComposerSendKey::Enter, path.clone());
        focus_composer(&mut harness);
        // 빈 버퍼에서 ↑ → 최신("two") → ↑ → 이전("one").
        harness.key_press(egui::Key::ArrowUp);
        harness.run();
        assert_eq!(buffer_of(&harness), "two");
        harness.key_press(egui::Key::ArrowUp);
        harness.run();
        assert_eq!(buffer_of(&harness), "one");
        // ↓ → 최신으로 → 한 번 더 ↓ → 빈 드래프트 복귀(탐색 종료).
        harness.key_press(egui::Key::ArrowDown);
        harness.run();
        assert_eq!(buffer_of(&harness), "two");
        harness.key_press(egui::Key::ArrowDown);
        harness.run();
        assert!(buffer_of(&harness).is_empty());
        assert!(harness.state().0.history_pos.is_none());
        std::fs::remove_file(&path).ok();
    }

    /// codex P2 회귀(7차): 토큰 삽입 캐럿도 즉시 저장 — 붙여넣기 제스처와 Text 이벤트가
    /// 한 입력 프레임에 배치되면 post-show 예약으로는 그 프레임의 텍스트가 옛 캐럿
    /// (토큰 앞)에 들어간다. begin 직후의 첫 TextEdit 패스가 토큰 뒤 캐럿을 봐야 한다.
    #[test]
    fn kittest_토큰_삽입과_같은_프레임의_텍스트는_토큰_뒤에_들어간다() {
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let path = test_history_path("attach-same-frame-text");
        let mut harness = composer_harness(&catalog, ComposerSendKey::Enter, path.clone());
        focus_composer(&mut harness);
        // 붙여넣기 제스처 시뮬레이션: begin_attach를 harness ctx로 직접 호출(macOS 테스트
        // 러너에선 native monitor/Ctrl+Shift+V 트리거를 이벤트로 주입할 수 없다) —
        // 토큰 삽입 + 캐럿 즉시 저장까지가 "그 프레임 show 전" 상태다.
        let harness_ctx = harness.ctx.clone();
        let request = {
            let ui = &mut harness.state_mut().0;
            let mut buffer = ui.buffers.remove(TEST_WS).unwrap_or_default();
            let request = ui
                .begin_attachment_request(&harness_ctx, TEST_WS, None, &mut buffer)
                .unwrap();
            ui.buffers.insert(TEST_WS.to_owned(), buffer);
            request
        };
        let token = request.target.token.clone();
        // 같은 입력 프레임에 배치된 Text 이벤트 — 정확히 1프레임만 돌린다.
        harness
            .input_mut()
            .events
            .push(egui::Event::Text("x".to_owned()));
        harness.step();
        assert_eq!(
            buffer_of(&harness),
            format!("{token}x"),
            "같은 프레임의 텍스트는 토큰 **뒤**에 들어가야 한다(토큰 무손상)"
        );
        std::fs::remove_file(&path).ok();
    }

    /// codex P2 회귀(5차): 드롭 삽입의 캐럿은 post-show 예약이 아니라 **즉시 저장** —
    /// 예약은 다음 프레임에나 적용돼 그 사이 타이핑이 낡은 캐럿을 쓰고 전환 시 유실된다.
    #[test]
    fn kittest_드롭_프레임에_캐럿이_즉시_삽입_끝이_된다() {
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let path = test_history_path("drop-caret");
        let mut harness = composer_harness(&catalog, ComposerSendKey::Enter, path.clone());
        // 포인터를 도크 위에 두고 OS 파일 드롭을 주입 — 정확히 1프레임만 돌린다.
        harness
            .input_mut()
            .events
            .push(egui::Event::PointerMoved(egui::pos2(50.0, 20.0)));
        harness
            .input_mut()
            .dropped_files
            .push(crate::test_dropped_file::handle(PathBuf::from("/x/a.png")));
        harness.step();
        assert_eq!(buffer_of(&harness), "/x/a.png");
        assert_eq!(
            cursor_char_index(&harness.ctx, ComposerUi::text_id(TEST_WS)),
            Some("/x/a.png".chars().count()),
            "드롭 프레임에 캐럿이 즉시 삽입 끝이어야 한다"
        );
        std::fs::remove_file(&path).ok();
    }

    /// 도크 밖에서 놓은 OS 드롭은 무시한다 — 컴포저·터미널·사이드바 중 실제 마우스
    /// 위치의 영역만 받는다는 라우팅 규칙의 컴포저 쪽 절반(2026-08-14).
    #[test]
    fn kittest_도크_밖_드롭은_무시된다() {
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let path = test_history_path("drop-outside-dock");
        let mut harness = composer_harness(&catalog, ComposerSendKey::Enter, path.clone());
        // 도크 캔버스 밖(멀리 떨어진 좌표) — 포인터를 여기 두고 OS 드롭 주입.
        harness
            .input_mut()
            .events
            .push(egui::Event::PointerMoved(egui::pos2(5000.0, 5000.0)));
        harness
            .input_mut()
            .dropped_files
            .push(crate::test_dropped_file::handle(PathBuf::from("/x/a.png")));
        harness.step();
        assert_eq!(
            buffer_of(&harness),
            "",
            "도크 밖 드롭은 버퍼에 아무것도 넣지 않아야 한다"
        );
        std::fs::remove_file(&path).ok();
    }

    /// 드롭 경로가 macOS NFD(자소분해)로 와도 컴포저 버퍼엔 NFC로 들어간다 —
    /// `mention_path_는_nfd_입력을_nfc로_합성한다`가 단위 검증한 걸 드롭 경로 전체로
    /// 확인한다(2026-08-14, 사용자: "한글 파일 드래그 드롭하면 아예 안들어가고있어").
    #[test]
    fn kittest_드롭된_nfd_파일명은_nfc로_삽입된다() {
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let path = test_history_path("drop-nfd-korean");
        let mut harness = composer_harness(&catalog, ComposerSendKey::Enter, path.clone());
        // "한" = ᄒ+ᅡ+ᆫ (NFD) — macOS 드래그&드롭이 넘기는 형태.
        let nfd_han = "\u{1112}\u{1161}\u{11AB}";
        let nfd_path = PathBuf::from(format!("/x/{nfd_han}.txt"));
        harness
            .input_mut()
            .events
            .push(egui::Event::PointerMoved(egui::pos2(50.0, 20.0)));
        harness
            .input_mut()
            .dropped_files
            .push(crate::test_dropped_file::handle(nfd_path));
        harness.step();
        let expected: String = format!("/x/{nfd_han}.txt").nfc().collect();
        assert_eq!(buffer_of(&harness), expected);
        std::fs::remove_file(&path).ok();
    }

    fn mcp_server(id: &str, name: &str, tool_count: usize) -> connector_contract::ServerSummary {
        connector_contract::ServerSummary {
            id: connector_contract::ServerId::new(id),
            name: name.to_owned(),
            transport: connector_contract::TransportKind::Stdio,
            enabled: true,
            connection: connector_contract::ConnectionState::Idle,
            tool_count,
            error_code: None,
        }
    }

    fn mcp_tool(index: usize, name: &str) -> connector_contract::ToolListItem {
        connector_contract::ToolListItem {
            id: connector_contract::ToolId::new(format!("tool-{index}")),
            name: name.to_owned(),
            description: Some(format!("description-{index}")),
            permission: connector_contract::PermissionRule::Ask,
        }
    }

    fn mcp_snapshot(
        servers: Vec<connector_contract::ServerSummary>,
        page: Option<connector_contract::ToolPage>,
    ) -> connector_contract::ConnectorSnapshot {
        connector_contract::ConnectorSnapshot {
            config_revision: connector_contract::Revision(1),
            servers: servers.into(),
            tool_page: page,
            ..connector_contract::ConnectorSnapshot::default()
        }
    }

    fn expand_composer(harness: &mut egui_kittest::Harness<'_, ComposerHarnessState>) {
        harness.state_mut().0.request_focus();
        harness.run();
        harness.run();
    }

    fn context_file_request(action: &ComposerAction) -> ContextFileRequest {
        match action {
            ComposerAction::RequestContextFile(request) => request.clone(),
            other => panic!("expected context-file request, got {other:?}"),
        }
    }

    #[test]
    fn kittest_context_file_click은_placeholder와_intent만_만든다() {
        use egui_kittest::kittest::Queryable;
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let path = test_history_path("context-file-intent");
        let mut harness = composer_harness(&catalog, ComposerSendKey::Enter, path.clone());
        harness.state_mut().3 = Some(PathBuf::from("/project"));
        expand_composer(&mut harness);
        harness.get_by_label("@").click();
        harness.run();

        assert_eq!(harness.state().1.len(), 1, "click당 action은 하나뿐이다");
        let request = context_file_request(&harness.state().1[0]);
        assert_eq!(request.workspace_id(), TEST_WS);
        assert_eq!(request.workspace_root(), Some(Path::new("/project")));
        assert_eq!(request.request_id(), 1);
        assert_eq!(buffer_of(&harness), request.token());
        assert_eq!(harness.state().0.pending_context_file, Some(request));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn kittest_context_file_completion은_그사이_타이핑을_보존한다() {
        use egui_kittest::kittest::Queryable;
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let path = test_history_path("context-file-typing");
        let mut harness = composer_harness(&catalog, ComposerSendKey::Enter, path.clone());
        harness.state_mut().3 = Some(PathBuf::from("/project"));
        expand_composer(&mut harness);
        harness.get_by_label("@").click();
        harness.run();
        let request = context_file_request(&harness.state().1[0]);

        harness
            .get_by_role(egui::accesskit::Role::MultilineTextInput)
            .type_text(" tail");
        harness.run();
        let egui_ctx = harness.ctx.clone();
        assert!(harness.state_mut().0.complete_context_file(
            &egui_ctx,
            request,
            Some(PathBuf::from("/project/src/main.rs")),
            TEST_WS,
        ));
        assert_eq!(buffer_of(&harness), "@src/main.rs tail");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn kittest_context_file_completion은_요청시점_workspace에_적용한다() {
        use egui_kittest::kittest::Queryable;
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let path = test_history_path("context-file-workspace-switch");
        let mut harness = composer_harness(&catalog, ComposerSendKey::Enter, path.clone());
        harness.state_mut().3 = Some(PathBuf::from("/project"));
        expand_composer(&mut harness);
        harness.get_by_label("@").click();
        harness.run();
        let request = context_file_request(&harness.state().1[0]);
        harness
            .state_mut()
            .0
            .buffers
            .insert("ws-b".to_owned(), "other draft".to_owned());

        let egui_ctx = harness.ctx.clone();
        assert!(harness.state_mut().0.complete_context_file(
            &egui_ctx,
            request,
            Some(PathBuf::from("/project/README.md")),
            "ws-b",
        ));
        assert_eq!(buffer_of(&harness), "@README.md");
        assert_eq!(harness.state().0.buffers["ws-b"], "other draft");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn kittest_context_file_cancel은_삽입_padding까지_정확히_제거한다() {
        use egui_kittest::kittest::Queryable;
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let path = test_history_path("context-file-cancel-padding");
        let mut harness = composer_harness(&catalog, ComposerSendKey::Enter, path.clone());
        expand_composer(&mut harness);
        harness
            .get_by_role(egui::accesskit::Role::MultilineTextInput)
            .type_text("left");
        harness.run();
        harness.get_by_label("@").click();
        harness.run();
        let request = context_file_request(&harness.state().1[0]);
        assert_eq!(buffer_of(&harness), format!("left {}", request.token()));

        let egui_ctx = harness.ctx.clone();
        assert!(
            harness
                .state_mut()
                .0
                .complete_context_file(&egui_ctx, request, None, TEST_WS,)
        );
        assert_eq!(buffer_of(&harness), "left");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn kittest_context_file_new_request가_old를_대체하고_stale_completion은_무시한다() {
        use egui_kittest::kittest::Queryable;
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let path = test_history_path("context-file-stale");
        let mut harness = composer_harness(&catalog, ComposerSendKey::Enter, path.clone());
        expand_composer(&mut harness);
        harness.get_by_label("@").click();
        harness.run();
        let old = context_file_request(&harness.state().1[0]);
        harness.get_by_label("@").click();
        harness.run();
        let new = context_file_request(&harness.state().1[1]);
        assert_ne!(old.request_id(), new.request_id());
        assert!(!buffer_of(&harness).contains(old.token()));
        assert_eq!(buffer_of(&harness), new.token());

        let before = buffer_of(&harness);
        let egui_ctx = harness.ctx.clone();
        assert!(!harness.state_mut().0.complete_context_file(
            &egui_ctx,
            old,
            Some(PathBuf::from("/stale")),
            TEST_WS,
        ));
        assert_eq!(buffer_of(&harness), before);
        assert!(
            harness
                .state_mut()
                .0
                .complete_context_file(&egui_ctx, new, None, TEST_WS,)
        );
        assert!(buffer_of(&harness).is_empty());
        std::fs::remove_file(&path).ok();
    }

    /// 2026-07-18 사용자 회귀: 서버는 등록됐지만 도구가 아직 발견되지 않은 상태가 흔하다.
    /// overview의 tool_count가 0이면 저장소 요청 없이 명시적 빈 상태를 보여준다.
    #[test]
    fn kittest_mcp_도구가_0개인_서버만_있으면_안내문구가_뜬다() {
        use egui_kittest::kittest::Queryable;
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let path = test_history_path("mcp-empty-servers");
        let mut harness = composer_harness(&catalog, ComposerSendKey::Enter, path.clone());
        harness.state_mut().2 = mcp_snapshot(
            vec![mcp_server("111", "111", 0), mcp_server("github", "깃헙", 0)],
            None,
        );
        expand_composer(&mut harness);
        harness.get_by_label("MCP").click();
        harness.run();
        assert!(
            harness
                .query_by_label(&catalog.t("composer.tools_empty", &[]))
                .is_some(),
            "도구 0개 서버만 있으면 안내 문구가 보여야 한다"
        );
        assert!(
            harness.state().1.is_empty(),
            "빈 서버는 page를 요청하지 않는다"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn kittest_mcp는_상호작용_전에는_page를_요청하지_않는다() {
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let path = test_history_path("mcp-lazy");
        let mut harness = composer_harness(&catalog, ComposerSendKey::Enter, path.clone());
        harness.state_mut().2 = mcp_snapshot(vec![mcp_server("a", "server-a", 1)], None);
        expand_composer(&mut harness);
        for _ in 0..300 {
            harness.run();
        }
        assert!(harness.state().1.is_empty());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn kittest_mcp_popup과_서버선택은_각각_page를_한번만_요청한다() {
        use egui_kittest::kittest::Queryable;
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let path = test_history_path("mcp-page-intent");
        let mut harness = composer_harness(&catalog, ComposerSendKey::Enter, path.clone());
        harness.state_mut().2 = mcp_snapshot(
            vec![
                mcp_server("a", "server-a", 1),
                mcp_server("b", "server-b", 1),
            ],
            None,
        );
        expand_composer(&mut harness);
        harness.get_by_label("MCP").click();
        harness.run();
        assert_eq!(
            harness.state().1,
            vec![ComposerAction::RequestMcpToolPage {
                server_id: connector_contract::ServerId::new("a"),
                offset: 0,
            }]
        );
        for _ in 0..3 {
            harness.run();
        }
        assert_eq!(
            harness.state().1.len(),
            1,
            "stable frame은 요청을 반복하지 않는다"
        );

        harness.state_mut().1.clear();
        harness.get_by_label("server-b").click();
        harness.run();
        assert_eq!(
            harness.state().1,
            vec![ComposerAction::RequestMcpToolPage {
                server_id: connector_contract::ServerId::new("b"),
                offset: 0,
            }]
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn kittest_mcp_mismatched_page는_렌더하지_않는다() {
        use egui_kittest::kittest::Queryable;
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let path = test_history_path("mcp-mismatched-page");
        let mut harness = composer_harness(&catalog, ComposerSendKey::Enter, path.clone());
        harness.state_mut().2 = mcp_snapshot(
            vec![
                mcp_server("a", "server-a", 1),
                mcp_server("b", "server-b", 1),
            ],
            Some(connector_contract::ToolPage {
                server_id: connector_contract::ServerId::new("b"),
                offset: 0,
                total: 1,
                items: vec![mcp_tool(0, "foreign-tool")].into(),
            }),
        );
        expand_composer(&mut harness);
        harness.get_by_label("MCP").click();
        harness.run();
        assert!(harness.query_by_label("server-b · foreign-tool").is_none());
        assert_eq!(
            harness.state().1,
            vec![ComposerAction::RequestMcpToolPage {
                server_id: connector_contract::ServerId::new("a"),
                offset: 0,
            }]
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn kittest_mcp_4096_total은_256_page중_visible_row만_렌더한다() {
        use egui_kittest::kittest::Queryable;
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let path = test_history_path("mcp-virtualized-page");
        let mut harness = composer_harness(&catalog, ComposerSendKey::Enter, path.clone());
        let tools = (0..MCP_TOOL_PAGE_LIMIT)
            .map(|index| mcp_tool(index, &format!("tool-{index}")))
            .collect::<Vec<_>>();
        harness.state_mut().2 = mcp_snapshot(
            vec![mcp_server("a", "server-a", 4_096)],
            Some(connector_contract::ToolPage {
                server_id: connector_contract::ServerId::new("a"),
                offset: 0,
                total: 4_096,
                items: tools.into(),
            }),
        );
        expand_composer(&mut harness);
        MCP_TOOL_ROWS_RENDERED.store(0, std::sync::atomic::Ordering::Relaxed);
        harness.get_by_label("MCP").click();
        harness.run();
        let rendered = MCP_TOOL_ROWS_RENDERED.load(std::sync::atomic::Ordering::Relaxed);
        assert!(rendered > 0);
        assert!(
            rendered < MCP_TOOL_PAGE_LIMIT,
            "virtualized popup rendered {rendered}/{} rows",
            MCP_TOOL_PAGE_LIMIT
        );
        harness.get_by_label("›").click();
        harness.run();
        assert_eq!(
            harness.state().1,
            vec![ComposerAction::RequestMcpToolPage {
                server_id: connector_contract::ServerId::new("a"),
                offset: MCP_TOOL_PAGE_LIMIT,
            }]
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn kittest_mcp_셀렉터가_실행_지시_문장을_버퍼에_삽입한다() {
        use egui_kittest::kittest::Queryable;
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let path = test_history_path("mcp-insert");
        let mut harness = composer_harness(&catalog, ComposerSendKey::Enter, path.clone());
        let mut tool = mcp_tool(0, "mytool");
        tool.description = Some("설명 텍스트".to_owned());
        harness.state_mut().2 = mcp_snapshot(
            vec![mcp_server("srv", "srv", 1)],
            Some(connector_contract::ToolPage {
                server_id: connector_contract::ServerId::new("srv"),
                offset: 0,
                total: 1,
                items: vec![tool].into(),
            }),
        );
        // 펼침(툴바 노출) — ⌘J 경로와 동일.
        expand_composer(&mut harness);
        harness.get_by_label("MCP").click();
        harness.run();
        harness.get_by_label("srv · mytool").click();
        harness.run();
        // 도구 이름만 맨몸으로 삽입하면 에이전트가 못 알아듣는다(2026-07-17 사용자
        // 실사용 확인, "directory_tree"만 보내니 에이전트가 반문) — 자연어 지시
        // 문장으로 감싸 삽입해야 한다.
        let expected = format!(
            "{} ",
            catalog.t("composer.mcp_insert_template", &[("tool", "mytool")])
        );
        assert_eq!(buffer_of(&harness), expected);
        // 삽입 후 커서는 삽입 끝이어야 한다 — 옛 위치(0)에 남으면 다음 타이핑이
        // 문장 앞/안에 끼어 깨진다(codex P2, 기존 검증 유지).
        let state =
            egui::text_edit::TextEditState::load(&harness.ctx, ComposerUi::text_id(TEST_WS))
                .expect("TextEdit 상태가 저장돼 있어야 한다");
        let cursor: usize = state
            .cursor
            .char_range()
            .expect("커서가 있어야 한다")
            .primary
            .index
            .into();
        assert_eq!(cursor, expected.chars().count());
        std::fs::remove_file(&path).ok();
    }
}
