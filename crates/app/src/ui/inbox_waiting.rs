//! 벨 팝오버 「대기 중」 섹션의 PTY 입력 대기 카드 (v3.9 N3).
//!
//! hook이 보고한 needsInput(claude/codex의 y/n·메뉴 번호 선택 프롬프트)에 **그
//! 워크스페이스/세션으로 이동하지 않고** 응답한다. 이 파일은 immutable snapshot 렌더와
//! bounded intent 생성만 담당한다. 카드 데이터 조립, 로그 파일 읽기, 실제 명령 전송은
//! app.rs의 host 경계가 한다. 이 모듈은 leaf UI 경계(xtask check-boundary)를 지켜
//! DB/스토리지/파일/스레드/런타임 구체 구현을 직접 참조하지 않는다.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use runtime::{MuxSnapshot, SessionId};

/// 로그 tail 읽기 상한 — 화면 한 장(80×24 기준 ~2KB)을 여유 있게 담되 UI가 부담되지
/// 않는 선. 줄 수를 12로 늘리면서 함께 키웠다(4KB면 재그리기 반복 탓에 12줄이 안 찰 수 있다).
pub const LOG_PREVIEW_TAIL_BYTES: u64 = 16_384;
/// 미리보기 줄 수 — claude/codex는 화면을 통째로 다시 그려 로그에 남기므로, tail
/// 마지막 몇 줄은 **항상 상태줄**이다(2026-07-17 실측: 3줄일 때 `⏵⏵ auto mode on`만
/// 보이고 정작 질문이 안 보였다). 승인 프롬프트 박스는 화면 하단이라 12줄이면
/// 박스째 들어온다. 아래 last_lines가 빈 줄·연속 중복을 접어 실제 표시는 더 짧다.
pub const LOG_PREVIEW_LINES: usize = 12;
pub const LOG_PREVIEW_LINE_BYTES: usize = 4 * 1024;
pub const LOG_PREVIEW_PATH_BYTES: usize = 32 * 1024;
const LOG_PREVIEW_CACHE_ITEMS: usize = 64;
const LOG_PREVIEW_QUEUE_CAP: usize = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogPreviewOperation(u64);

/// App host 경계를 지나는 로그 경로. raw path는 Debug에 절대 노출하지 않는다. 경로는
/// 생성 시 32 KiB/NUL 상한을 통과하고 Arc 하나로만 intent에 전달되어 frame별 복제를 막는다.
pub struct LogPreviewSource {
    path: PathBuf,
    bytes: usize,
}

impl LogPreviewSource {
    pub fn try_new(path: PathBuf) -> Result<Self, LogPreviewErrorCode> {
        let display = path.to_string_lossy();
        let bytes = display.len();
        if display.as_bytes().contains(&0) {
            return Err(LogPreviewErrorCode::InvalidPath);
        }
        if bytes == 0 || bytes > LOG_PREVIEW_PATH_BYTES {
            return Err(LogPreviewErrorCode::PathTooLarge);
        }
        Ok(Self { path, bytes })
    }

    pub fn as_path(&self) -> &Path {
        &self.path
    }
}

impl PartialEq for LogPreviewSource {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
    }
}

impl Eq for LogPreviewSource {}

impl Hash for LogPreviewSource {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.path.hash(state);
    }
}

impl std::fmt::Debug for LogPreviewSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogPreviewSource")
            .field("path", &"REDACTED")
            .field("bytes", &self.bytes)
            .finish()
    }
}

/// redacted log tail에서 만든 immutable UI snapshot. Debug는 raw line을 출력하지 않는다.
#[derive(Clone, PartialEq, Eq)]
pub struct LogPreviewSnapshot {
    lines: Arc<[Arc<str>]>,
    bytes: usize,
}

impl LogPreviewSnapshot {
    /// Host가 파일 끝에서 `LOG_PREVIEW_TAIL_BYTES` 이하를 읽은 뒤 호출하는 순수 변환기.
    /// `truncated_prefix`는 seek가 0보다 컸음을 뜻하며, 그때 첫 불완전 줄을 버린다.
    pub fn try_from_tail_bytes(
        bytes: &[u8],
        truncated_prefix: bool,
    ) -> Result<Option<Self>, LogPreviewErrorCode> {
        if bytes.len() > LOG_PREVIEW_TAIL_BYTES as usize {
            return Err(LogPreviewErrorCode::PreviewTooLarge);
        }
        let text = String::from_utf8_lossy(bytes);
        let text = if truncated_prefix {
            text.split_once('\n').map(|(_, rest)| rest).unwrap_or("")
        } else {
            &text
        };
        let lines = bounded_last_lines(
            text,
            LOG_PREVIEW_LINES,
            LOG_PREVIEW_LINE_BYTES,
            LOG_PREVIEW_TAIL_BYTES as usize,
        );
        if lines.is_empty() {
            return Ok(None);
        }
        let bytes = lines.iter().map(|line| line.len()).sum();
        Ok(Some(Self {
            lines: lines.into_iter().map(Arc::<str>::from).collect(),
            bytes,
        }))
    }

    pub fn lines(&self) -> &[Arc<str>] {
        &self.lines
    }
}

impl std::fmt::Debug for LogPreviewSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogPreviewSnapshot")
            .field("lines", &self.lines.len())
            .field("bytes", &self.bytes)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogPreviewErrorCode {
    Busy,
    InvalidPath,
    PathTooLarge,
    PreviewTooLarge,
    NativeFailure,
}

/// Leaf가 반환하고 App host가 소비하는 단일 로그 읽기 intent. path/source와 raw log는
/// Clone/Serialize하지 않으며 Debug에는 계수와 상관 ID만 남는다.
pub struct LogPreviewIntent {
    pub operation: LogPreviewOperation,
    pub generation: u64,
    pub source: Arc<LogPreviewSource>,
}

impl std::fmt::Debug for LogPreviewIntent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogPreviewIntent")
            .field("operation", &self.operation)
            .field("generation", &self.generation)
            .field("source", &self.source)
            .finish()
    }
}

pub struct LogPreviewCompletion {
    pub operation: LogPreviewOperation,
    pub generation: u64,
    pub result: Result<Option<LogPreviewSnapshot>, LogPreviewErrorCode>,
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct LogPreviewKey {
    workspace_id: String,
    session: SessionId,
    source: Arc<LogPreviewSource>,
}

struct PendingLogPreview {
    key: LogPreviewKey,
    operation: LogPreviewOperation,
    generation: u64,
}

/// 카드 하나의 표시 데이터. app.rs가 활성/warm 워크스페이스의 여러 필드(session_titles,
/// mux 스냅샷, 메모리 summary 또는 로그 tail)에서 조립해 넘긴다.
#[derive(Debug, Clone)]
pub struct WaitingCard {
    pub workspace_id: String,
    pub session: SessionId,
    pub workspace_name: String,
    pub session_title: String,
    /// hook이 보고한 대기 사유 — claude Notification payload의 message
    /// ("Claude needs your permission to use Bash"). 에이전트가 직접 말한 "무엇을
    /// 묻는지"라 아래 tail보다 정확하다(tail은 TUI 재그리기라 상태줄이 섞인다).
    /// 문구를 안 싣는 에이전트는 None — 그 경우 tail만 보인다.
    pub headline: Option<String>,
    /// 미리보기를 펼쳤을 때만 host intent를 만들 수 있는 bounded source. UUID/경로를
    /// 만들 수 없으면 None이고 카드의 답장/이동 UX는 그대로 유지한다.
    pub preview_source: Option<Arc<LogPreviewSource>>,
}

/// 카드에서 사용자가 취한 액션. app.rs가 소비한다.
#[derive(Debug, Clone, PartialEq)]
pub enum WaitingAction {
    /// [y]/[n]/자유 입력 — 문자열(개행은 app.rs가 붙인다)을 그 세션에 주입해 달라는 요청.
    /// stale 재확인·실제 전송은 app.rs 몫이다(이 모듈은 런타임 클라이언트를 모른다).
    Answer {
        workspace_id: String,
        session: SessionId,
        reply: String,
    },
    /// [이동→] — 기존 알림 네비게이션 파이프라인(plan_agent_notification_navigation)에
    /// 그대로 태울 수 있게 AgentNotificationTarget::Pty를 실어 돌려준다.
    Goto(super::notifications::AgentNotificationTarget),
}

/// 팝오버 PTY 카드의 렌더 상태. 파일/스레드/channel/timer를 소유하지 않고, immutable
/// snapshot과 capacity-1 host intent만 보관한다.
pub struct InboxWaitingUi {
    previews: HashMap<LogPreviewKey, Arc<LogPreviewSnapshot>>,
    preview_intent: Option<LogPreviewIntent>,
    pending_preview: Option<PendingLogPreview>,
    preview_generation: u64,
    next_preview_operation: u64,
    /// 카드별 자유 입력 버퍼 — (workspace_id, session)으로 프레임 간 유지한다.
    inputs: HashMap<(String, SessionId), String>,
    /// 미리보기를 펼쳐 둔 카드들 — 기본은 접힘(카드가 화면을 덮지 않게).
    expanded: HashSet<(String, SessionId)>,
}

impl InboxWaitingUi {
    pub fn new() -> Self {
        Self {
            previews: HashMap::new(),
            preview_intent: None,
            pending_preview: None,
            preview_generation: 1,
            next_preview_operation: 1,
            inputs: HashMap::new(),
            expanded: HashSet::new(),
        }
    }

    /// App host가 실행할 단일 intent. host slot이 비어 있을 때만 take하고, admission이
    /// 실패하면 같은 operation/generation으로 오류 completion을 돌려줘야 한다.
    pub fn take_preview_intent(&mut self) -> Option<LogPreviewIntent> {
        self.preview_intent.take()
    }

    /// Host 결과를 exact operation/generation으로 적용한다. source가 바뀌거나 카드가
    /// 사라져 generation이 전진한 뒤 도착한 결과는 현재 snapshot을 건드리지 않는다.
    pub fn complete_preview(&mut self, completion: LogPreviewCompletion) -> bool {
        let Some(pending) = self.pending_preview.as_ref() else {
            return false;
        };
        if pending.operation != completion.operation
            || pending.generation != completion.generation
            || completion.generation != self.preview_generation
        {
            return false;
        }
        let pending = self.pending_preview.take().expect("exact pending checked");
        match completion.result {
            Ok(Some(snapshot)) => self.insert_preview(pending.key, Arc::new(snapshot)),
            Ok(None) | Err(_) => {
                self.previews.remove(&pending.key);
            }
        }
        true
    }

    fn insert_preview(&mut self, key: LogPreviewKey, snapshot: Arc<LogPreviewSnapshot>) {
        if !self.previews.contains_key(&key)
            && self.previews.len() >= LOG_PREVIEW_CACHE_ITEMS
            && let Some(evicted) = self.previews.keys().next().cloned()
        {
            self.previews.remove(&evicted);
        }
        self.previews.insert(key, snapshot);
    }

    fn queue_preview(&mut self, card: &WaitingCard) -> Result<(), LogPreviewErrorCode> {
        debug_assert_eq!(LOG_PREVIEW_QUEUE_CAP, 1);
        if self.preview_intent.is_some() || self.pending_preview.is_some() {
            return Err(LogPreviewErrorCode::Busy);
        }
        let source = Arc::clone(
            card.preview_source
                .as_ref()
                .ok_or(LogPreviewErrorCode::InvalidPath)?,
        );
        let operation = LogPreviewOperation(self.next_preview_operation);
        self.next_preview_operation = self.next_preview_operation.wrapping_add(1).max(1);
        let generation = self.preview_generation;
        let key = LogPreviewKey {
            workspace_id: card.workspace_id.clone(),
            session: card.session,
            source: Arc::clone(&source),
        };
        self.pending_preview = Some(PendingLogPreview {
            key,
            operation,
            generation,
        });
        self.preview_intent = Some(LogPreviewIntent {
            operation,
            generation,
            source,
        });
        Ok(())
    }

    /// 팝오버 「대기 중」 섹션의 PTY 카드들을 그린다. 카드가 비어 있으면 아무것도
    /// 그리지 않는다(빈 섹션 헤더를 보이지 않는다 — MCP 승인 카드만 있을 수 있다).
    pub fn render(
        &mut self,
        ui: &mut egui::Ui,
        catalog: &i18n::Catalog,
        cards: &[WaitingCard],
    ) -> Option<WaitingAction> {
        // 사라진 카드(대기 해소·워크스페이스 소멸)의 입력 버퍼를 정리한다 — SessionId는
        // 워커마다 1부터 재배정되므로 방치하면 다른 논리 세션이 과거 드래프트를 물려받는다
        // (2026-07-17 리뷰 P2). 카드가 비어도 실행해 마지막 카드 해소 시의 잔존을 막는다.
        let alive = |workspace_id: &String, session: &SessionId| {
            cards
                .iter()
                .any(|card| card.workspace_id == *workspace_id && card.session == *session)
        };
        self.inputs
            .retain(|(workspace_id, session), _| alive(workspace_id, session));
        // 펼침 상태도 같은 규칙으로 정리 — 세션이 재배정되면 남의 카드가 펼쳐진 채 뜬다.
        self.expanded
            .retain(|(workspace_id, session)| alive(workspace_id, session));
        let preview_alive = |key: &LogPreviewKey| {
            cards.iter().any(|card| {
                card.workspace_id == key.workspace_id
                    && card.session == key.session
                    && card
                        .preview_source
                        .as_ref()
                        .is_some_and(|source| source == &key.source)
            })
        };
        self.previews.retain(|key, _| preview_alive(key));
        if self
            .pending_preview
            .as_ref()
            .is_some_and(|pending| !preview_alive(&pending.key))
        {
            self.pending_preview = None;
            self.preview_intent = None;
            self.preview_generation = self.preview_generation.wrapping_add(1).max(1);
        }
        if cards.is_empty() {
            return None;
        }
        super::notifications::section_label(ui, &catalog.t("inbox.waiting.section", &[]));
        let mut action = None;
        for card in cards {
            ui.add_space(4.0);
            self.render_card(ui, catalog, card, &mut action);
        }
        action
    }

    fn render_card(
        &mut self,
        ui: &mut egui::Ui,
        catalog: &i18n::Catalog,
        card: &WaitingCard,
        action: &mut Option<WaitingAction>,
    ) {
        // 승인 카드(inbox_approvals)와 같은 박스로 감싼다 — 카드가 여러 장일 때
        // 경계가 없으면 어느 미리보기가 어느 세션 것인지 뭉쳐 보인다(2026-07-17 사용자).
        egui::Frame::group(ui.style()).show(ui, |ui| {
            self.render_card_body(ui, catalog, card, action);
        });
    }

    fn render_card_body(
        &mut self,
        ui: &mut egui::Ui,
        catalog: &i18n::Catalog,
        card: &WaitingCard,
        action: &mut Option<WaitingAction>,
    ) {
        ui.horizontal(|ui| {
            ui.label("⏳");
            ui.label(
                egui::RichText::new(format!("{} · {}", card.workspace_name, card.session_title))
                    .size(12.0)
                    .strong(),
            );
        });
        // 헤드라인(hook이 말한 대기 사유)이 있으면 tail보다 위에, 눈에 띄게 — 이게
        // "무엇을 승인/응답하는지"의 답이다. tail은 펼쳤을 때 선택지 번호를 보여준다.
        if let Some(headline) = &card.headline {
            ui.add(egui::Label::new(egui::RichText::new(headline).size(12.0)).wrap());
        }
        // 미리보기는 **기본 접힘** — 12줄이라 펼쳐두면 카드 하나가 화면 절반을 먹는다
        // (2026-07-17 사용자). 헤드라인이 주 정보이고, tail은 선택지 번호를 확인할 때만
        // 필요하다. 접힘 상태는 세션별로 프레임 간 유지한다.
        if let Some(source) = &card.preview_source {
            let key = (card.workspace_id.clone(), card.session);
            let expanded = self.expanded.contains(&key);
            let label = if expanded {
                catalog.t("inbox.waiting.hide_screen", &[])
            } else {
                catalog.t("inbox.waiting.show_screen", &[])
            };
            if ui
                .add(egui::Button::new(egui::RichText::new(label).size(11.0)).frame(false))
                .clicked()
            {
                if expanded {
                    self.expanded.remove(&key);
                } else {
                    self.expanded.insert(key);
                    // 명시적 펼치기만 refresh를 요청한다. snapshot이 있으면 완료 전까지
                    // stale 값으로 UX를 유지하며, host가 바쁘면 자동 polling/retry하지 않는다.
                    let _ = self.queue_preview(card);
                }
            }
            if expanded {
                let snapshot = self
                    .previews
                    .iter()
                    .find(|(key, _)| {
                        key.workspace_id == card.workspace_id
                            && key.session == card.session
                            && &key.source == source
                    })
                    .map(|(_, snapshot)| snapshot.as_ref());
                render_preview(ui, catalog, snapshot);
            }
        }
        ui.horizontal(|ui| {
            if ui.small_button("y").clicked() {
                *action = Some(WaitingAction::Answer {
                    workspace_id: card.workspace_id.clone(),
                    session: card.session,
                    reply: "y".to_owned(),
                });
            }
            if ui.small_button("n").clicked() {
                *action = Some(WaitingAction::Answer {
                    workspace_id: card.workspace_id.clone(),
                    session: card.session,
                    reply: "n".to_owned(),
                });
            }
            let input_key = (card.workspace_id.clone(), card.session);
            let buf = self.inputs.entry(input_key.clone()).or_default();
            let resp = ui.add(
                egui::TextEdit::singleline(buf)
                    // auto-Id는 위치 기반이라 위쪽 카드가 해소되면 포커스가 다음 카드
                    // 입력칸으로 밀린다(다른 세션 오입력, 2026-07-17 리뷰 P2) —
                    // 세션 고유 Id로 고정한다.
                    .id_salt(("inbox_waiting_input", &card.workspace_id, card.session.0))
                    // 남는 폭을 [이동→] 몫만 남기고 입력칸에 준다 — 72px 고정일 때
                    // hint("답장 후 Enter")조차 잘렸다(2026-07-17 사용자).
                    .desired_width((ui.available_width() - 72.0).max(96.0))
                    .hint_text(catalog.t("inbox.waiting.answer_hint", &[])),
            );
            let submit = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            // 빈 Enter는 무시한다 — 빈 reply는 PTY에 "\n"만 주입해 프롬프트의 기본
            // 항목을 실행할 수 있다(의도치 않은 승인 효과, 리뷰 P3). 기본값 수락이
            // 필요하면 y/n 버튼이나 [이동→]을 쓴다.
            if submit && !buf.trim().is_empty() {
                let reply = buf.clone();
                self.inputs.remove(&input_key);
                *action = Some(WaitingAction::Answer {
                    workspace_id: card.workspace_id.clone(),
                    session: card.session,
                    reply,
                });
            }
            if ui
                .small_button(catalog.t("inbox.waiting.goto", &[]))
                .clicked()
            {
                *action = Some(WaitingAction::Goto(
                    super::notifications::AgentNotificationTarget::Pty {
                        workspace_id: card.workspace_id.clone(),
                        session: card.session,
                    },
                ));
            }
        });
    }
}

/// 미리보기 3줄 — 고정폭, 좌측 세로선(section_label과 같은 스타일의 accent bar).
/// 없으면 폴백 문구 하나만 보인다.
fn render_preview(
    ui: &mut egui::Ui,
    catalog: &i18n::Catalog,
    preview: Option<&LogPreviewSnapshot>,
) {
    let Some(lines) = preview
        .map(LogPreviewSnapshot::lines)
        .filter(|lines| !lines.is_empty())
    else {
        ui.label(
            egui::RichText::new(catalog.t("inbox.waiting.no_preview", &[]))
                .size(10.5)
                .weak(),
        );
        return;
    };
    ui.horizontal(|ui| {
        let row_h = 13.0;
        let (rect, _) = ui.allocate_exact_size(
            egui::vec2(2.0, row_h * lines.len() as f32),
            egui::Sense::hover(),
        );
        ui.painter().rect_filled(
            rect,
            1.0,
            ui.visuals().widgets.noninteractive.bg_stroke.color,
        );
        ui.vertical(|ui| {
            for line in lines {
                // truncate로 시각 1줄 고정 — wrap되면 좌측 bar 높이(줄 수 × row_h)와
                // 어긋나고, 4KB 단일 줄 tail이면 카드가 수십 행으로 폭발한다(리뷰 P3).
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(line.as_ref())
                            .monospace()
                            .size(10.0)
                            .weak(),
                    )
                    .truncate(),
                );
            }
        });
    });
}

/// 라이브 mux 스냅샷에서 세션의 영속 UUID(sessions.id)를 찾는다. PaneSnapshot에 이미
/// 실려 온다(v3.7 I1: "경계를 넘는 식별자는 이 UUID를 써야" — 알림 딥링크와 같은 계약이라
/// DB 조인 없이 얻을 수 있다). warm 워크스페이스는 mux 스냅샷이 최신이 아닐 수 있어
/// (§14.1 — Warm은 렌더 경로로만 갱신된다) 못 찾을 수 있다: 그 경우 호출측이 미리보기를
/// 생략한다(카드 자체는 정상 동작 — 계획서 §PR-N3 폴백 규약).
pub fn find_persistent_session_id(mux: &MuxSnapshot, session: SessionId) -> Option<String> {
    mux.tabs
        .iter()
        .flat_map(|tab| &tab.panes)
        .find(|pane| pane.session_id == Some(session))
        .and_then(|pane| pane.persistent_session_id.clone())
}

/// hook 세션 키(`{workspace_id}:{u64}`) 파싱 — 규칙과 문서는 `deppy_core`에 있다
/// (runtime이 만들고 app·web-remote가 소비하므로 최하층 공용). 소유 String이 필요한
/// 호출부(카드 조립)를 위해 얇게 감싼다.
pub fn parse_session_key(key: &str) -> Option<(String, SessionId)> {
    deppy_core::parse_session_key(key).map(|(ws, session)| (ws.to_owned(), session))
}

/// 에이전트가 화면 하단에 **늘** 그리는 장식 줄인가 — 승인/응답 판단에 아무 정보도
/// 주지 않으면서 tail을 통째로 차지한다(2026-07-17 사용자: "하단에 이것까진 안 나와도
/// 될 것 같다"). 걸러야 그 위의 실제 질문·선택지가 미리보기에 들어온다.
///
/// statusline은 사용자가 설정한 명령의 출력을 그대로 체인하므로(mcp-proxy의
/// chain_user_statusline) 형식을 특정할 수 없다 — 대신 claude가 제공하는 컨텍스트
/// 게이지(`[█░░] 23%`)라는 공통 특징으로 거른다. 못 걸러도 손해는 tail 한 줄뿐이다.
fn is_screen_chrome(line: &str) -> bool {
    let t = line.trim();
    // claude 모드 표시: "⏵⏵ auto mode on (shift+tab to cycle)", "⏸ ..."
    if t.starts_with('⏵') || t.starts_with('⏸') {
        return true;
    }
    // 컨텍스트 게이지가 있는 statusline.
    if (t.contains('█') || t.contains('░')) && t.contains('%') {
        return true;
    }
    t == "Checking for updates" || t.starts_with("? for shortcuts")
}

/// 텍스트의 마지막 n줄(비어있지 않은 줄만, 원래 순서 유지) — 로그 tail 미리보기 추출.
///
/// **연속 중복은 접는다**: TUI가 상태줄을 주기적으로 다시 그려 같은 줄이 로그에 연달아
/// 쌓인다(2026-07-17 실측 — `⏵⏵ auto mode on`이 반복되며 12줄을 다 먹었다). 접지 않으면
/// 줄 수를 늘려도 상태줄만 늘어난다.
/// 순수 함수(유닛 테스트 대상). 실제 파일 읽기는 App host만 수행한다.
#[cfg(test)]
fn last_lines(text: &str, n: usize) -> Vec<String> {
    bounded_last_lines(
        text,
        n,
        LOG_PREVIEW_LINE_BYTES,
        LOG_PREVIEW_TAIL_BYTES as usize,
    )
}

fn truncate_utf8(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn bounded_last_lines(
    text: &str,
    n: usize,
    max_line_bytes: usize,
    max_total_bytes: usize,
) -> Vec<String> {
    let mut lines: Vec<String> = Vec::with_capacity(n);
    let mut total_bytes = 0usize;
    for line in text.lines().rev() {
        let trimmed = line.trim_end();
        if trimmed.trim().is_empty() || is_screen_chrome(trimmed) {
            continue;
        }
        let remaining = max_total_bytes.saturating_sub(total_bytes);
        let trimmed = truncate_utf8(trimmed, max_line_bytes.min(remaining));
        if trimmed.is_empty() {
            break;
        }
        // 역순 순회라 "직전에 담은 것"이 로그상 바로 다음 줄 — 연속 중복 판정에 맞다.
        if lines.last().map(String::as_str) == Some(trimmed) {
            continue;
        }
        total_bytes += trimmed.len();
        lines.push(trimmed.to_owned());
        if lines.len() == n {
            break;
        }
    }
    lines.reverse();
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use runtime::{LayoutNode, MuxPaneId, MuxTabId, PaneSnapshot, TabSnapshot};

    #[test]
    fn inbox_waiting_production_source는_host_io와_polling이_없다() {
        let source = include_str!("inbox_waiting.rs");
        let production = source.split("#[cfg(test)]").next().unwrap();
        for forbidden in [
            "std::fs",
            "File::open",
            ".metadata(",
            ".read_to_end(",
            ".seek(",
            "std::thread",
            "mpsc",
            "request_repaint_after",
            "request_repaint(",
            "storage::",
        ] {
            assert!(
                !production.contains(forbidden),
                "production inbox leaf contains forbidden host edge: {forbidden}"
            );
        }
    }

    #[test]
    fn parse_session_key_parses_workspace_and_session() {
        assert_eq!(
            parse_session_key("ws-abc:42"),
            Some(("ws-abc".to_owned(), SessionId(42)))
        );
    }

    #[test]
    fn parse_session_key_rejects_malformed_input() {
        assert_eq!(parse_session_key("no-colon"), None);
        assert_eq!(parse_session_key("ws:not-a-number"), None);
        assert_eq!(parse_session_key(":42"), None); // 빈 workspace id
        assert_eq!(parse_session_key("ws:"), None); // 빈 session id
    }

    #[test]
    fn last_lines_returns_last_n_non_empty_in_order() {
        let text = "a\nb\n\nc\nd\ne\n";
        assert_eq!(
            last_lines(text, 3),
            vec!["c".to_owned(), "d".to_owned(), "e".to_owned()]
        );
    }

    #[test]
    fn last_lines_returns_all_when_fewer_than_n() {
        assert_eq!(last_lines("only\n", 3), vec!["only".to_owned()]);
    }

    #[test]
    fn last_lines_empty_or_blank_text_returns_empty() {
        assert!(last_lines("", 3).is_empty());
        assert!(last_lines("\n\n\n", 3).is_empty());
    }

    fn snapshot_with_pane(pane_id: &str, session: SessionId, uuid: Option<&str>) -> MuxSnapshot {
        MuxSnapshot {
            tabs: vec![TabSnapshot {
                id: MuxTabId("t1".to_owned()),
                title: "tab".to_owned(),
                layout: LayoutNode::Pane(MuxPaneId(pane_id.to_owned())),
                panes: vec![PaneSnapshot {
                    id: MuxPaneId(pane_id.to_owned()),
                    session_id: Some(session),
                    title: "shell".to_owned(),
                    persistent_session_id: uuid.map(str::to_owned),
                }],
            }],
            active_tab: Some(MuxTabId("t1".to_owned())),
            focused_pane: None,
        }
    }

    #[test]
    fn find_persistent_session_id_matches_by_live_session() {
        let mux = snapshot_with_pane("p1", SessionId(1), Some("uuid-1"));
        assert_eq!(
            find_persistent_session_id(&mux, SessionId(1)),
            Some("uuid-1".to_owned())
        );
        assert_eq!(find_persistent_session_id(&mux, SessionId(2)), None);
    }

    #[test]
    fn find_persistent_session_id_none_when_pane_has_no_persisted_uuid() {
        let mux = snapshot_with_pane("p1", SessionId(1), None);
        assert_eq!(find_persistent_session_id(&mux, SessionId(1)), None);
    }

    #[test]
    fn log_preview_snapshot은_item_line_byte_상한을_지킨다() {
        let line = "가".repeat(LOG_PREVIEW_LINE_BYTES);
        let raw = (0..32)
            .map(|index| format!("{index}:{line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let tail = &raw.as_bytes()[raw.len() - LOG_PREVIEW_TAIL_BYTES as usize..];
        let snapshot = LogPreviewSnapshot::try_from_tail_bytes(tail, true)
            .unwrap()
            .unwrap();
        assert!(snapshot.lines().len() <= LOG_PREVIEW_LINES);
        assert!(
            snapshot
                .lines()
                .iter()
                .all(|line| line.len() <= LOG_PREVIEW_LINE_BYTES)
        );
        assert!(snapshot.bytes <= LOG_PREVIEW_TAIL_BYTES as usize);
    }

    #[test]
    fn log_preview_path와_debug는_raw_내용을_노출하지_않는다() {
        let raw = "/private/token-like/path/redacted.plain.txt";
        let source = LogPreviewSource::try_new(PathBuf::from(raw)).unwrap();
        let source_debug = format!("{source:?}");
        assert!(source_debug.contains("REDACTED"));
        assert!(!source_debug.contains(raw));
        assert_eq!(
            LogPreviewSource::try_new(PathBuf::from("bad\0path")).unwrap_err(),
            LogPreviewErrorCode::InvalidPath
        );
        assert_eq!(
            LogPreviewSource::try_new(PathBuf::from("x".repeat(LOG_PREVIEW_PATH_BYTES + 1)))
                .unwrap_err(),
            LogPreviewErrorCode::PathTooLarge
        );

        let raw_log = "never-print-this-preview";
        let snapshot = LogPreviewSnapshot::try_from_tail_bytes(raw_log.as_bytes(), false)
            .unwrap()
            .unwrap();
        let snapshot_debug = format!("{snapshot:?}");
        assert!(!snapshot_debug.contains(raw_log));
        assert!(snapshot_debug.contains("lines"));
    }

    /// 2026-07-17 실측 회귀: claude가 상태줄을 주기적으로 다시 그려 같은 줄이 로그에
    /// 연달아 쌓인다 — 접지 않으면 줄 수를 늘려도 `⏵⏵ auto mode on`만 12줄 나온다.
    #[test]
    fn last_lines_연속_중복_상태줄을_접어_질문이_보이게_한다() {
        let log = "Do you want to proceed?\n\
                   1. Yes\n\
                   2. No\n\
                   auto mode on\n\
                   auto mode on\n\
                   auto mode on\n\
                   auto mode on\n";
        assert_eq!(
            last_lines(log, 4),
            vec![
                "Do you want to proceed?".to_owned(),
                "1. Yes".to_owned(),
                "2. No".to_owned(),
                "auto mode on".to_owned(),
            ],
            "반복 상태줄은 한 줄로 접혀 질문·선택지가 살아남아야 한다"
        );
    }

    /// 2026-07-17 사용자 회귀: 화면 하단 장식(모드 표시 + statusline)이 미리보기를
    /// 차지했다. 아래 3줄은 실제 화면에서 그대로 가져온 것.
    #[test]
    fn last_lines_화면하단_장식을_걸러_질문이_보이게_한다() {
        let log = "Do you want to proceed?\n\
                   ❯ 1. Yes\n\
                   ⏵⏵ auto mode on (shift+tab to cycle)\n\
                   SKRT  docs/selected-train-alert-p an  Opus 4.8 (1M context)  [█░░░░░░░░░] 23%\n\
                   ⏵⏵ auto mode on (shift+tab to cycle)\n";
        assert_eq!(
            last_lines(log, 4),
            vec!["Do you want to proceed?".to_owned(), "❯ 1. Yes".to_owned()],
            "모드 표시·statusline은 빠지고 질문·선택지만 남아야 한다"
        );
    }

    /// 게이지가 없는 평범한 출력은 걸러선 안 된다 — %가 있다는 이유만으로 버리면
    /// "coverage 23%" 같은 진짜 결과가 사라진다.
    #[test]
    fn is_screen_chrome_일반_출력은_걸러지지_않는다() {
        assert!(!is_screen_chrome("All tests passed: coverage 23%"));
        assert!(!is_screen_chrome("Do you want to proceed?"));
        assert!(!is_screen_chrome("  2. No"));
        assert!(is_screen_chrome("⏵⏵ auto mode on (shift+tab to cycle)"));
        assert!(is_screen_chrome("SKRT  Opus 4.8 (1M context)  [█░░░] 23%"));
    }

    /// 떨어져 있는 같은 줄은 접지 않는다 — 연속만 중복으로 본다(맥락 유지).
    #[test]
    fn last_lines_떨어진_같은_줄은_접지_않는다() {
        let log = "a\nb\na\n";
        assert_eq!(
            last_lines(log, 3),
            vec!["a".to_owned(), "b".to_owned(), "a".to_owned()]
        );
    }

    #[test]
    fn log_preview_snapshot은_잘린_첫_줄을_버린다() {
        let snapshot = LogPreviewSnapshot::try_from_tail_bytes(b"bb\ncccc\ndddd\n", true)
            .unwrap()
            .unwrap();
        assert_eq!(
            snapshot
                .lines()
                .iter()
                .map(|line| line.as_ref())
                .collect::<Vec<_>>(),
            vec!["cccc", "dddd"]
        );
    }

    // ── kittest 상호작용 테스트 (2026-07-17) ──
    // "그 세션에 가지 않고 y/n/번호로 응답"의 UI 절반을 실제 클릭·타이핑 시뮬레이션으로
    // 자동 검증한다(주입 절반 WriteInput은 runtime 테스트가 커버).

    fn card(session: u64) -> WaitingCard {
        WaitingCard {
            workspace_id: "ws-1".to_owned(),
            session: SessionId(session),
            workspace_name: "proj".to_owned(),
            session_title: "codex".to_owned(),
            headline: None,
            preview_source: None,
        }
    }

    fn card_with_preview(session: u64, path: &str) -> WaitingCard {
        WaitingCard {
            preview_source: Some(Arc::new(
                LogPreviewSource::try_new(PathBuf::from(path)).unwrap(),
            )),
            ..card(session)
        }
    }

    /// 액션들을 프레임 너머로 수집하는 하네스. InboxWaitingUi(입력 버퍼)도 상태에 넣어
    /// 프레임 간 유지한다 — 실제 App과 동일한 수명.
    fn waiting_harness<'a>(
        catalog: &'a i18n::Catalog,
        cards: &'a [WaitingCard],
    ) -> egui_kittest::Harness<'a, (InboxWaitingUi, Vec<WaitingAction>)> {
        egui_kittest::Harness::new_ui_state(
            move |ui, (widget, captured): &mut (InboxWaitingUi, Vec<WaitingAction>)| {
                if let Some(action) = widget.render(ui, catalog, cards) {
                    captured.push(action);
                }
            },
            (InboxWaitingUi::new(), Vec::new()),
        )
    }

    #[test]
    fn kittest_y_클릭이_answer_y를_만든다() {
        use egui_kittest::kittest::Queryable;
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let cards = vec![card(7)];
        let mut harness = waiting_harness(&catalog, &cards);
        harness.get_by_label("y").click();
        harness.run();
        assert_eq!(
            harness.state().1,
            vec![WaitingAction::Answer {
                workspace_id: "ws-1".to_owned(),
                session: SessionId(7),
                reply: "y".to_owned(),
            }]
        );
    }

    #[test]
    fn kittest_collapsed_300_frame은_preview_host_intent가_0이다() {
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let cards = vec![card_with_preview(7, "/bounded/redacted.plain.txt")];
        let mut harness = waiting_harness(&catalog, &cards);
        for _ in 0..300 {
            harness.run();
        }
        assert!(harness.state_mut().0.take_preview_intent().is_none());
        assert!(harness.state().0.pending_preview.is_none());
    }

    #[test]
    fn preview_intent는_capacity_one이고_completion은_exact_stale_safe다() {
        let first = card_with_preview(7, "/first/redacted.plain.txt");
        let second = card_with_preview(8, "/second/redacted.plain.txt");
        let mut ui = InboxWaitingUi::new();
        ui.queue_preview(&first).unwrap();
        assert_eq!(
            ui.queue_preview(&second).unwrap_err(),
            LogPreviewErrorCode::Busy
        );
        let intent = ui.take_preview_intent().unwrap();
        let snapshot = LogPreviewSnapshot::try_from_tail_bytes(b"question\n1. Yes\n", false)
            .unwrap()
            .unwrap();
        assert!(!ui.complete_preview(LogPreviewCompletion {
            operation: intent.operation,
            generation: intent.generation.wrapping_add(1),
            result: Ok(Some(snapshot.clone())),
        }));
        assert!(ui.complete_preview(LogPreviewCompletion {
            operation: intent.operation,
            generation: intent.generation,
            result: Ok(Some(snapshot)),
        }));
        assert_eq!(ui.previews.len(), 1);
        assert!(ui.pending_preview.is_none());
    }

    #[test]
    fn preview_source_change는_late_completion을_폐기한다() {
        let first = card_with_preview(7, "/first/redacted.plain.txt");
        let second = card_with_preview(7, "/second/redacted.plain.txt");
        let mut state = InboxWaitingUi::new();
        state.queue_preview(&first).unwrap();
        let intent = state.take_preview_intent().unwrap();
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, state: &mut InboxWaitingUi| {
                state.render(ui, &catalog, std::slice::from_ref(&second));
            },
            state,
        );
        harness.run();
        assert!(!harness.state_mut().complete_preview(LogPreviewCompletion {
            operation: intent.operation,
            generation: intent.generation,
            result: Ok(LogPreviewSnapshot::try_from_tail_bytes(b"stale\n", false).unwrap()),
        }));
        assert!(harness.state().previews.is_empty());
    }

    #[test]
    fn kittest_자유입력_타이핑_후_enter가_answer를_만들고_버퍼를_비운다() {
        use egui_kittest::kittest::Queryable;
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let cards = vec![card(7)];
        let mut harness = waiting_harness(&catalog, &cards);
        // 번호 응답("2↵") 시나리오 — claude/codex 메뉴 선택. 실제 사용자처럼 입력칸을
        // 먼저 클릭(포커스)한다 — kittest type_text는 Event::Text만 넣으므로 포커스가
        // 없으면 TextEdit이 무시한다.
        harness
            .get_by_role(egui::accesskit::Role::TextInput)
            .click();
        harness.run();
        harness
            .get_by_role(egui::accesskit::Role::TextInput)
            .type_text("2");
        harness.run();
        harness.key_combination(&[egui::Key::Enter]);
        harness.run();
        let answers = &harness.state().1;
        assert_eq!(
            answers,
            &vec![WaitingAction::Answer {
                workspace_id: "ws-1".to_owned(),
                session: SessionId(7),
                reply: "2".to_owned(),
            }],
            "Enter 시 입력 내용이 Answer로 나와야 한다"
        );
        // 제출 시 항목이 remove되지만 다음 프레임 렌더의 or_default()가 빈 항목을
        // 재생성한다 — 계약은 "내용이 비워짐"이다.
        assert!(
            harness.state().0.inputs.values().all(|buf| buf.is_empty()),
            "전송 후 입력 버퍼 내용이 비워져야 한다"
        );
    }

    #[test]
    fn kittest_빈_입력_enter는_아무것도_보내지_않는다() {
        use egui_kittest::kittest::Queryable;
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let cards = vec![card(7)];
        let mut harness = waiting_harness(&catalog, &cards);
        harness
            .get_by_role(egui::accesskit::Role::TextInput)
            .click();
        harness.run();
        harness.key_combination(&[egui::Key::Enter]);
        harness.run();
        assert!(
            harness.state().1.is_empty(),
            "빈 reply는 PTY에 개행만 주입해 기본 항목을 실행할 수 있어 무시해야 한다"
        );
    }

    #[test]
    fn kittest_이동_클릭이_goto_타깃을_만든다() {
        use egui_kittest::kittest::Queryable;
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let cards = vec![card(7)];
        let mut harness = waiting_harness(&catalog, &cards);
        harness.get_by_label("Go to →").click();
        harness.run();
        assert_eq!(
            harness.state().1,
            vec![WaitingAction::Goto(
                crate::ui::notifications::AgentNotificationTarget::Pty {
                    workspace_id: "ws-1".to_owned(),
                    session: SessionId(7),
                }
            )]
        );
    }
}
