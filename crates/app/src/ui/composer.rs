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

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::config::ComposerSendKey;

/// 히스토리 상한 — 초과분은 오래된 것부터 버린다.
const MAX_HISTORY: usize = 100;
/// 펼침 상태 텍스트 영역 상한(줄) — 넘으면 내부 스크롤.
const MAX_TEXT_ROWS: usize = 8;
/// 펼침 상태 최소 줄 수 — 빈 버퍼여도 여러 줄 컴포저로 보이게.
const EXPANDED_MIN_ROWS: usize = 3;
/// 접힘↔펼침 높이 트윈 시간(초).
const ANIM_SECONDS: f32 = 0.16;

/// web-remote P6a `encode_input`과 동일한 붙여넣기 판정 임계값.
const INPUT_PASTE_THRESHOLD: usize = 512;

/// 모델 후보 — PTY 에이전트에는 외부 모델 제어 프로토콜이 없으므로 **슬래시 커맨드
/// 텍스트 삽입** 방식이다(사용자가 검토 후 전송). CLI가 받는 대표 이름의 하드코딩 목록.
const CLAUDE_MODELS: &[&str] = &["opus", "sonnet", "haiku"];
const CODEX_MODELS: &[&str] = &["gpt-5.5-codex", "gpt-5.5", "gpt-5.6-sol"];

/// 컴포저가 App에 돌려주는 액션. 실제 전송(인코딩 + WriteInput)은 App이 한다.
#[derive(Debug, Clone, PartialEq)]
pub enum ComposerAction {
    /// 전송 키/버튼 — 이 프롬프트를 포커스 세션에 주입해 달라는 요청.
    Send(String),
}

/// 렌더에 필요한 프레임 데이터 (App이 채워 넘긴다 — 경계상 평면 값만).
pub struct ComposerContext<'a> {
    pub workspace_id: &'a str,
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
}

/// 첨부(클립보드/드롭) 백그라운드 태스크 결과 채널.
type AttachReceiver = std::sync::mpsc::Receiver<anyhow::Result<Option<Vec<PathBuf>>>>;

/// 첨부 태스크 시작 시점의 대상 스냅샷 — 완료가 워크스페이스 전환 **뒤에** 와도
/// 시작 시점의 드래프트에 삽입하기 위해 캡처한다(codex P2 — 엉뚱한 드래프트 오염 방지).
struct AttachTarget {
    workspace_id: String,
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

/// 진행 중인 클립보드 첨부 태스크.
struct PendingAttach {
    rx: AttachReceiver,
    target: AttachTarget,
}

/// 하단 도크 컴포저 상태. App이 소유하고 매 프레임 `render`를 호출한다.
pub struct ComposerUi {
    /// 워크스페이스별 드래프트 — 전환해도 초안이 유지된다.
    buffers: HashMap<String, String>,
    /// 펼침 상태 — 포커스/⌘J로 열리고, ⌘J/바깥 클릭으로 접힌다.
    expanded: bool,
    /// ⌘J(전역 단축키) → 다음 렌더에서 펼침 + 포커스 요청.
    focus_requested: bool,
    /// 보낸 프롬프트(오래된 것 → 최신). 시작 시 파일에서 1회 로드, 전송 시 저장.
    history: Vec<String>,
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
    /// 히스토리 영속 파일 (jsonl — 한 줄 = JSON 문자열 하나).
    history_path: PathBuf,
    /// 클립보드 이미지/파일 첨부 백그라운드 태스크 (터미널과 동일 인프라 재사용).
    attach_task: Option<PendingAttach>,
    /// 첨부 플레이스홀더 토큰 단조 카운터 — 대체된 옛 태스크의 토큰과 구별한다.
    attach_seq: u64,
    /// **비활성** 워크스페이스 드래프트의 토큰 치환이 예약한 캐럿/선택 보정(워크스페이스
    /// → 리베이스된 (primary, secondary) 문자 인덱스 — 선택 방향 보존, codex P2).
    /// TextEditState는 위젯 단위 상태라 전환으로 그 워크스페이스가 다시 활성이 되는
    /// 시점(sync_workspace)에 적용하는 것이 안전하다(codex P1 — 치환이 길이를 바꿔
    /// 캐럿이 경로 안에 박히는 문제).
    pending_caret: HashMap<String, (usize, usize)>,
    /// MCP 서버별 도구 이름 (서버명, 도구들) — App이 펼침 시 1회 채운다(경계: 평면 값만).
    mcp_tools: Vec<(String, Vec<String>)>,
    /// 이번 펼침에서 MCP 목록을 이미 받았다 — 접히면 리셋해 다음 펼침에 재조회.
    mcp_loaded: bool,
}

impl ComposerUi {
    /// 시작 시 1회 히스토리를 로드한다 — UI 프레임 경로에서 파일 IO 금지 원칙.
    pub fn new(history_path: PathBuf) -> Self {
        Self {
            buffers: HashMap::new(),
            expanded: false,
            focus_requested: false,
            history: load_history(&history_path),
            history_pos: None,
            pending_cursor: None,
            pending_surrender: None,
            last_workspace: None,
            history_path,
            attach_task: None,
            attach_seq: 0,
            pending_caret: HashMap::new(),
            mcp_tools: Vec::new(),
            mcp_loaded: false,
        }
    }

    /// ⌘J 전역 단축키(컴포저 비포커스일 때만 발화) — 펼침 + 포커스 요청.
    pub fn request_focus(&mut self) {
        self.focus_requested = true;
    }

    /// App이 MCP 목록을 채워야 하는가 — 펼침 상태에서 아직 못 받았을 때만 true
    /// (프레임 경로 저장소 조회를 펼침당 1회로 제한).
    pub fn mcp_refresh_needed(&self) -> bool {
        self.expanded && !self.mcp_loaded
    }

    pub fn set_mcp_tools(&mut self, tools: Vec<(String, Vec<String>)>) {
        self.mcp_tools = tools;
        self.mcp_loaded = true;
    }

    /// 컴포저 TextEdit의 egui Id — 워크스페이스별 상태(커서 등) 분리.
    fn text_id(workspace_id: &str) -> egui::Id {
        egui::Id::new(("composer_text", workspace_id))
    }

    pub fn render(
        &mut self,
        ui: &mut egui::Ui,
        catalog: &i18n::Catalog,
        ctx: &ComposerContext<'_>,
    ) -> Option<ComposerAction> {
        // 버퍼를 잠시 꺼내 self 차용 충돌 없이 다룬다 — 렌더 끝에 반드시 되돌린다.
        let mut buffer = self.buffers.remove(ctx.workspace_id).unwrap_or_default();
        let action = self.render_inner(ui, catalog, ctx, &mut buffer);
        self.buffers.insert(ctx.workspace_id.to_owned(), buffer);
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
        let text_id = Self::text_id(ctx.workspace_id);
        // 워크스페이스 전환 감지 — 히스토리 탐색 위치/커서 예약은 이전 워크스페이스
        // 문맥이므로 리셋한다(드래프트는 워크스페이스별 맵이 이미 분리 — codex P3).
        self.sync_workspace(&egui_ctx, ctx.workspace_id);
        // 지연된 포커스 반납(접힘 단축키 — pending_surrender 필드 주석 참조). 새 입력
        // 프레임(time 증가) **이면서 접힘 키가 릴리스된 뒤**에만 반납한다 — 그때의
        // raw 이벤트에는 접힘 키(초타·repeat 모두)가 이미 없으므로 터미널로 새지 않는다.
        if let Some((id, collapsed_at, key)) = self.pending_surrender
            && egui_ctx.input(|input| input.time > collapsed_at && !input.keys_down.contains(&key))
        {
            self.pending_surrender = None;
            egui_ctx.memory_mut(|memory| memory.surrender_focus(id));
        }
        // 직전 프레임의 포커스 — 이번 프레임 TextEdit이 그려지기 전이라 memory가 그 값이다.
        let had_focus = egui_ctx.memory(|memory| memory.has_focus(text_id));
        let focus_requested = std::mem::take(&mut self.focus_requested);
        if focus_requested {
            self.expanded = true;
        }

        // ── 키 가로채기 (TextEdit이 그려지기 전에 소비 여부를 판단) ──
        let mut send_requested = false;
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
            let collapse_shortcut = ctx
                .collapse_shortcut
                .filter(|shortcut| !shadows_send_chord(shortcut, send_modifiers));
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
            self.start_attach_if_requested(&egui_ctx, ctx, buffer);
        }
        self.poll_attach_task(&egui_ctx, ctx.workspace_id, buffer);

        // ── 전송 판정 (빈 내용/세션 없음이면 무시 — Enter는 이미 소비돼 개행도 안 된다) ──
        // 키 경유 전송은 TextEdit을 그리기 전에 처리해 비워진 버퍼가 이번 프레임에 보인다.
        let mut action = None;
        if send_requested {
            action = self.try_submit(buffer, ctx.can_send, ctx.workspace_id);
            send_requested = false;
        }

        // ── 도크 카드 (ChatGPT 컴포저 언어: 둥근 모서리 + 1px 테두리 + 은은한 그림자) ──
        let dark = ui.visuals().dark_mode;
        let card_fill = ui.visuals().extreme_bg_color;
        let card_stroke = ui.visuals().widgets.noninteractive.bg_stroke;
        let shadow = egui::epaint::Shadow {
            offset: [0, 2],
            blur: 10,
            spread: 0,
            color: egui::Color32::from_black_alpha(if dark { 72 } else { 28 }),
        };
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

        let card = egui::Frame::new()
            .fill(card_fill)
            .stroke(card_stroke)
            .corner_radius(11)
            .inner_margin(egui::Margin::symmetric(12, 10))
            .shadow(shadow);
        let card_response = card.show(ui, |ui| {
            let output = egui::ScrollArea::vertical()
                .id_salt(text_id.with("scroll"))
                .max_height(text_h)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    egui::TextEdit::multiline(buffer)
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
            if self.expanded {
                ui.add_space(6.0);
                if self.toolbar(ui, catalog, ctx, &egui_ctx, text_id, buffer) {
                    send_requested = true;
                }
            }
            output
        });
        let output = card_response.inner;

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
            action = self.try_submit(buffer, ctx.can_send, ctx.workspace_id);
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
            self.mcp_loaded = false;
            if output.response.has_focus() {
                egui_ctx.memory_mut(|memory| memory.surrender_focus(text_id));
            }
        }
        if !self.expanded {
            self.mcp_loaded = false;
        }
        // 애니메이션 중에는 매 프레임 다시 그린다.
        if (text_h - row_h * target_rows as f32).abs() > 0.5 {
            egui_ctx.request_repaint();
        }
        action
    }

    /// 전송 시도 — 빈 내용/세션 없음은 무시. 성공 시 버퍼를 비우고 히스토리에 기록한다.
    fn try_submit(
        &mut self,
        buffer: &mut String,
        can_send: bool,
        workspace_id: &str,
    ) -> Option<ComposerAction> {
        if !can_send || buffer.trim().is_empty() || self.attach_pending_in(workspace_id, buffer) {
            return None;
        }
        let prompt = std::mem::take(buffer);
        push_history(&mut self.history, prompt.clone());
        save_history(&self.history_path, &self.history);
        self.history_pos = None;
        Some(ComposerAction::Send(prompt))
    }

    /// 이 버퍼에 **진행 중 태스크의 토큰**이 남아 있는가 — 이 상태로 전송하면 리터럴
    /// `⟦attach-N⟧`이 PTY로 가고 완료된 이미지는 버려진다(codex P1 — 전송 차단 조건).
    /// 정확한 판정(현재 태스크의 토큰 포함 여부)이라 사용자가 토큰을 지웠거나(취소)
    /// 히스토리 recall로 토큰 없는 버퍼가 되면 전송이 자연 허용된다.
    fn attach_pending_in(&self, workspace_id: &str, buffer: &str) -> bool {
        self.attach_task.as_ref().is_some_and(|task| {
            task.target.workspace_id == workspace_id && buffer.contains(&task.target.token)
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
            *buffer = self.history[next].clone();
            self.pending_cursor = Some(buffer.chars().count());
        }
        if browsing
            && on_last_line
            && consume_key_exact(egui_ctx, egui::Modifiers::NONE, egui::Key::ArrowDown)
        {
            match self.history_pos {
                Some(pos) if pos + 1 < self.history.len() => {
                    self.history_pos = Some(pos + 1);
                    *buffer = self.history[pos + 1].clone();
                    self.pending_cursor = Some(buffer.chars().count());
                }
                _ => {
                    // 최신을 지나면 빈 드래프트로 복귀 — 탐색 종료.
                    self.history_pos = None;
                    buffer.clear();
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

    /// 이미지-only 클립보드 붙여넣기 감지 → 첨부 태스크 기동.
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
    ) {
        let native_paste = crate::native_key_monitor::drain().clipboard_paste;
        let shortcut_paste = egui_ctx.input(|i| i.events.iter().any(is_attach_paste_shortcut));
        if !(native_paste || shortcut_paste) {
            return;
        }
        // 텍스트 paste(Event::Paste)가 같은 프레임에 있으면 TextEdit 기본 붙여넣기가
        // 처리한다 — 스크린샷 등 이미지/파일 클립보드만 백그라운드로 경로화한다.
        let has_text_paste =
            egui_ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Paste(_))));
        if has_text_paste {
            return;
        }
        self.begin_attach(
            egui_ctx,
            crate::ui::clipboard_image::paste_clipboard_paths_or_image_background(
                egui_ctx.clone(),
                false,
            ),
            ctx.workspace_id,
            ctx.workspace_root,
            buffer,
        );
    }

    /// 첨부 태스크 등록 — 그 시점 커서에 플레이스홀더 토큰을 **동기로 삽입**하고
    /// 태스크에는 (워크스페이스, 토큰)을 캡처한다. 완료는 토큰 문자열 치환이므로
    /// 변환 중 타이핑/전환에도 위치가 낡지 않는다(codex P2 — AttachTarget 주석).
    ///
    /// 진행 중이던 태스크는 **최신 것으로 대체**한다. 터미널 paste 경로와 동일 관례
    /// (ui/workspace.rs PendingPaste: "연타 ⌘V는 최신 것으로 대체") — 조용히 버리면
    /// 워커가 느릴 때 두 번째 ⌘V가 유실된다(codex P2). 옛 토큰 제거를 **먼저** 하고
    /// 그 다음(제거로 시프트된) 캐럿을 읽어 새 토큰을 삽입한다 — 제거 전에 캡처한
    /// 커서 인덱스는 제거로 낡는다(codex P2).
    fn begin_attach(
        &mut self,
        egui_ctx: &egui::Context,
        rx: AttachReceiver,
        workspace_id: &str,
        workspace_root: Option<&Path>,
        buffer: &mut String,
    ) {
        if let Some(old) = self.attach_task.take() {
            // 결과가 채널에 **이미 도착**했는데 아직 poll 전이면 그 결과부터 반영한다 —
            // 무조건 빈 치환으로 버리면 완성된 첨부가 유실된다(codex P2, 터미널 paste
            // 경로의 "처리 후 새 제스처" 순서 관례 미러). 진행 중(Empty)/워커 사망
            // (Disconnected)이면 기존 관례대로 토큰만 걷고 최신 제스처로 대체한다.
            // 캐럿 리베이스 포함(resolve_attach) — 활성이면 즉시 반영된다.
            let replacement = match old.rx.try_recv() {
                Ok(result) => attach_replacement(&old.target, result),
                Err(_) => String::new(),
            };
            self.resolve_attach(egui_ctx, &old.target, &replacement, workspace_id, buffer);
        }
        self.attach_seq += 1;
        let token = attach_token(self.attach_seq);
        let cursor = self
            .pending_cursor
            .or_else(|| cursor_char_index(egui_ctx, Self::text_id(workspace_id)));
        let inserted = insert_snippet(buffer, cursor, &token);
        self.pending_cursor = Some(inserted.cursor);
        self.attach_task = Some(PendingAttach {
            rx,
            target: AttachTarget {
                workspace_id: workspace_id.to_owned(),
                workspace_root: workspace_root.map(Path::to_path_buf),
                token,
                padding: (inserted.leading_space, inserted.trailing_space),
            },
        });
    }

    /// 첨부 태스크 완료 폴링 — 결과(@멘션)로 플레이스홀더 토큰을 치환한다.
    /// 실패/이미지 아님/워커 사망도 토큰은 반드시 제거한다(빈 치환).
    fn poll_attach_task(
        &mut self,
        egui_ctx: &egui::Context,
        active_workspace: &str,
        active_buffer: &mut String,
    ) {
        let Some(task) = &self.attach_task else {
            return;
        };
        let result = match task.rx.try_recv() {
            Ok(result) => Some(result),
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => None,
        };
        let task = self.attach_task.take().expect("위에서 Some 확인");
        let replacement = match result {
            Some(result) => attach_replacement(&task.target, result),
            None => String::new(),
        };
        self.resolve_attach(
            egui_ctx,
            &task.target,
            &replacement,
            active_workspace,
            active_buffer,
        );
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
    ) {
        let is_active = target.workspace_id == active_workspace;
        let replaced = {
            let draft = if is_active {
                &mut *active_buffer
            } else if let Some(draft) = self.buffers.get_mut(&target.workspace_id) {
                draft
            } else {
                return; // 드래프트 자체가 사라짐(워크스페이스 소멸 등) — 폐기
            };
            let Some((pattern, byte_start)) =
                find_token_pattern(draft, &target.token, replacement, target.padding)
            else {
                return; // 사용자가 토큰을 지움 — 취소
            };
            let replace_start = draft[..byte_start].chars().count();
            let pattern_chars = pattern.chars().count();
            *draft = draft.replacen(&pattern, replacement, 1);
            (replace_start, pattern_chars)
        };
        let (replace_start, pattern_chars) = replaced;
        let replacement_chars = replacement.chars().count();
        let rebase = |endpoint: usize| {
            rebase_caret(endpoint, replace_start, pattern_chars, replacement_chars)
        };
        // 캐럿/선택 리베이스 — 대상 워크스페이스의 현재 상태를 읽어 델타 적용.
        let ws_text_id = Self::text_id(&target.workspace_id);
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
        } else if let Some((primary, secondary)) = stored_char_range(egui_ctx, ws_text_id) {
            self.pending_caret.insert(
                target.workspace_id.clone(),
                (rebase(primary), rebase(secondary)),
            );
        }
    }

    /// OS 파일 드롭 — 포인터가 도크 위에 있을 때만 받아 @멘션으로 삽입한다.
    fn accept_dropped_files(
        &mut self,
        egui_ctx: &egui::Context,
        text_id: egui::Id,
        dock_rect: egui::Rect,
        workspace_root: Option<&Path>,
        buffer: &mut String,
    ) {
        let dropped: Vec<PathBuf> = egui_ctx.input(|i| {
            if i.raw.dropped_files.is_empty() {
                return Vec::new();
            }
            let over_dock = i
                .pointer
                .latest_pos()
                .is_some_and(|pos| dock_rect.contains(pos));
            if !over_dock {
                return Vec::new();
            }
            i.raw
                .dropped_files
                .iter()
                .filter_map(|file| file.path.clone())
                .collect()
        });
        if dropped.is_empty() {
            return;
        }
        // 여러 파일은 합쳐 1회 삽입(순서 보존 — 첨부 완료 치환과 동일 규칙).
        let inserted = insert_snippet(buffer, None, &joined_mentions(workspace_root, &dropped));
        // 드롭 처리는 post-show 캐럿 반영 **이후**에 돈다 — pending_cursor 예약은 다음
        // 프레임에나 적용돼 그 사이 타이핑이 낡은 캐럿을 쓰고 전환 시 유실된다(codex P2).
        // 첨부 완료(resolve_attach)와 같은 원칙으로 즉시 저장한다.
        store_caret_now(egui_ctx, text_id, inserted.cursor);
        self.expanded = true;
    }

    /// 컴포저 하단 툴바(펼침 상태 전용) — 셀렉터 3종 + 전송. 셀렉터는 전부 "검토 가능한
    /// 텍스트 삽입"이다: PTY 에이전트에는 외부 제어 프로토콜이 없어 선택이 상태를 직접
    /// 바꿀 수 없고, 사용자가 삽입된 텍스트를 보고 전송한다. 반환: 전송 버튼 클릭.
    fn toolbar(
        &mut self,
        ui: &mut egui::Ui,
        catalog: &i18n::Catalog,
        ctx: &ComposerContext<'_>,
        egui_ctx: &egui::Context,
        text_id: egui::Id,
        buffer: &mut String,
    ) -> bool {
        let mut send_clicked = false;
        // 삽입 후 커서 예약 — 팝업 클로저가 &self.mcp_tools를 빌리는 동안 self를 다시
        // 빌릴 수 없어 로컬에 모았다가 마지막에 반영한다.
        let mut inserted_cursor: Option<usize> = None;
        ui.horizontal(|ui| {
            // ① 컨텍스트 파일 — 네이티브 파일 피커(rfd, 프로젝트 폴더 선택과 동일 관례)
            //    → 워크스페이스 루트 기준 @상대경로(claude @멘션 규약).
            if ui
                .small_button("@")
                .on_hover_text(catalog.t("composer.context_hint", &[]))
                .clicked()
                && let Some(path) = rfd::FileDialog::new().pick_file()
            {
                let cursor = cursor_char_index(egui_ctx, text_id);
                inserted_cursor = Some(
                    insert_snippet(buffer, cursor, &mention_path(ctx.workspace_root, &path)).cursor,
                );
            }
            // ② 모델 — 감지된 에이전트의 후보를 `/model <이름>`으로 버퍼 맨 앞에 삽입.
            if let Some(agent) = ctx.agent {
                let models = match agent {
                    crate::agent_surface::AgentProvider::Claude => CLAUDE_MODELS,
                    crate::agent_surface::AgentProvider::Codex => CODEX_MODELS,
                };
                let resp = ui
                    .small_button("/model")
                    .on_hover_text(catalog.t("composer.model_hint", &[]));
                egui::Popup::menu(&resp).show(|ui| {
                    for model in models {
                        if ui.button(*model).clicked() {
                            inserted_cursor = Some(prepend_model_command(buffer, model));
                        }
                    }
                });
            }
            // ③ MCP 도구 — App이 넘긴 서버/도구 이름을 커서 위치에 삽입.
            let resp = ui
                .small_button("MCP")
                .on_hover_text(catalog.t("composer.tools_hint", &[]));
            egui::Popup::menu(&resp).show(|ui| {
                if self.mcp_tools.is_empty() {
                    ui.weak(catalog.t("composer.tools_empty", &[]));
                }
                for (server, tools) in &self.mcp_tools {
                    for tool in tools {
                        if ui.button(format!("{server} · {tool}")).clicked() {
                            let cursor = cursor_char_index(egui_ctx, text_id);
                            inserted_cursor = Some(insert_snippet(buffer, cursor, tool).cursor);
                        }
                    }
                }
            });
            // 우측: 전송 버튼 + 전송 키 힌트. 첨부 변환 중(토큰 pending)엔 전송을 막고
            // 사유를 힌트로 보인다 — try_submit과 같은 조건(codex P1).
            let attach_pending = self.attach_pending_in(ctx.workspace_id, buffer);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let enabled = ctx.can_send && !buffer.trim().is_empty() && !attach_pending;
                if ui
                    .add_enabled(enabled, egui::Button::new("↑").corner_radius(8))
                    .on_hover_text(catalog.t("composer.send", &[]))
                    .clicked()
                {
                    send_clicked = true;
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
        send_clicked
    }
}

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

/// 워커 결과 → 토큰 치환 문자열. 실패/이미지 아님은 빈 문자열(= 토큰 제거).
/// poll(정규 완료)과 begin(대체 직전 drain — codex P2)이 공유한다.
fn attach_replacement(
    target: &AttachTarget,
    result: anyhow::Result<Option<Vec<PathBuf>>>,
) -> String {
    match result {
        Ok(Some(paths)) => joined_mentions(target.workspace_root.as_deref(), &paths),
        Ok(None) => String::new(),
        Err(e) => {
            tracing::warn!("컴포저 클립보드 첨부 실패: {e:#}");
            String::new()
        }
    }
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
/// 구조(Eq) 비교도, matches_exact 양방향 OR도 부족하다: 비-macOS 물리 Ctrl 이벤트는
/// ctrl|command를 **둘 다** 켜서, `Command+Enter` 바인딩과 `Ctrl+Enter` 전송이 서로는
/// 매치되지 않아도 실제 이벤트는 양쪽을 만족한다(codex P2, 6차). 별칭을 접은 뒤
/// 비교하면 그 겹침이 그대로 드러난다 — macOS에서 Cmd/Ctrl 교차 코드까지 가림으로
/// 보는 보수적 판정이지만, 실패 방향이 "전송 우선"이라 안전하다.
fn shadows_send_chord(collapse: &egui::KeyboardShortcut, send_modifiers: egui::Modifiers) -> bool {
    collapse.logical_key == egui::Key::Enter
        && fold_command_into_ctrl(collapse.modifiers) == fold_command_into_ctrl(send_modifiers)
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
    paths
        .iter()
        .map(|path| mention_path(root, path))
        .collect::<Vec<_>>()
        .join(" ")
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
/// 밖이면 절대경로 그대로.
fn mention_path(root: Option<&Path>, path: &Path) -> String {
    match root.and_then(|root| path.strip_prefix(root).ok()) {
        Some(rel) if !rel.as_os_str().is_empty() => format!("@{}", rel.display()),
        _ => path.display().to_string(),
    }
}

/// (커서가 첫 줄인가, 마지막 줄인가) — 문자 인덱스 기준. 빈 버퍼는 (true, true).
fn cursor_line_info(text: &str, cursor_chars: usize) -> (bool, bool) {
    let on_first = !text.chars().take(cursor_chars).any(|c| c == '\n');
    let on_last = !text.chars().skip(cursor_chars).any(|c| c == '\n');
    (on_first, on_last)
}

/// 히스토리에 추가 — **연속 중복은 접고** 상한(100)을 넘으면 오래된 것부터 버린다.
fn push_history(history: &mut Vec<String>, entry: String) {
    if history.last() == Some(&entry) {
        return;
    }
    history.push(entry);
    if history.len() > MAX_HISTORY {
        let overflow = history.len() - MAX_HISTORY;
        history.drain(..overflow);
    }
}

/// 히스토리 로드(시작 시 1회) — jsonl 한 줄 = JSON 문자열 하나. 깨진 줄은 건너뛴다.
fn load_history(path: &Path) -> Vec<String> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut history = Vec::new();
    for line in content.lines() {
        if let Ok(entry) = serde_json::from_str::<String>(line) {
            push_history(&mut history, entry);
        }
    }
    history
}

/// 히스토리 저장(전송 시 1회) — 100건 상한의 작은 파일이라 동기 write 허용(계획 확정).
/// 실패는 경고만 — 프롬프트 전송 자체를 막지 않는다.
fn save_history(path: &Path, history: &[String]) {
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn push_history_는_연속_중복을_접고_상한을_지킨다() {
        let mut history = Vec::new();
        push_history(&mut history, "a".to_owned());
        push_history(&mut history, "a".to_owned()); // 연속 중복 — 접힘
        push_history(&mut history, "b".to_owned());
        push_history(&mut history, "a".to_owned()); // 떨어진 중복 — 유지
        assert_eq!(history, vec!["a", "b", "a"]);

        let mut full = Vec::new();
        for i in 0..150 {
            push_history(&mut full, format!("prompt-{i}"));
        }
        assert_eq!(full.len(), MAX_HISTORY);
        assert_eq!(full.first().map(String::as_str), Some("prompt-50"));
        assert_eq!(full.last().map(String::as_str), Some("prompt-149"));
    }

    #[test]
    fn history_파일_저장_후_재로드_라운드트립() {
        let path = std::env::temp_dir().join(format!(
            "deppy-composer-history-{}-roundtrip.jsonl",
            std::process::id()
        ));
        // 여러 줄 프롬프트도 jsonl(JSON 문자열 이스케이프)로 한 줄에 보존된다.
        let history = vec![
            "한 줄 프롬프트".to_owned(),
            "여러 줄\n프롬프트\n\t탭 포함".to_owned(),
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
        assert_eq!(load_history(&path), vec!["ok", "also ok"]);
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

    /// codex P2 회귀: 변환이 도는 동안 사용자가 타이핑해도 완료 결과는 **토큰 자리**에
    /// 들어간다 — 커서 인덱스 앵커였다면 옛 위치에 삽입돼 입력 순서가 섞였다.
    #[test]
    fn 첨부_pending_중_타이핑해도_결과는_토큰_자리에_들어간다() {
        let egui_ctx = egui::Context::default();
        let path = test_history_path("attach-token-typing");
        let mut ui = ComposerUi::new(path.clone());
        let mut active = String::new();
        let (tx, rx) = std::sync::mpsc::channel();
        ui.begin_attach(&egui_ctx, rx, TEST_WS, None, &mut active);
        let token = ui.attach_task.as_ref().unwrap().target.token.clone();
        assert_eq!(active, token, "토큰이 동기로 삽입돼야 한다");
        assert_eq!(
            ui.pending_cursor,
            Some(token.chars().count()),
            "동기 삽입은 기존 커서 예약 경로로 캐럿을 토큰 뒤에 둔다"
        );
        // 변환 중 사용자 입력 — 토큰 앞뒤로 타이핑.
        active = format!("before {active} after");
        tx.send(Ok(Some(vec![PathBuf::from("/x/shot.png")])))
            .unwrap();
        ui.poll_attach_task(&egui_ctx, TEST_WS, &mut active);
        assert_eq!(active, "before /x/shot.png after");
        assert!(ui.attach_task.is_none());
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
        let (tx, rx) = std::sync::mpsc::channel();
        ui.begin_attach(&egui_ctx, rx, TEST_WS, None, &mut active);
        // 사용자가 토큰 **뒤에** 타이핑, 캐럿은 끝.
        active.push_str(" tail");
        ui.pending_cursor = Some(active.chars().count());
        tx.send(Ok(Some(vec![PathBuf::from("/x/shot.png")])))
            .unwrap();
        ui.poll_attach_task(&egui_ctx, TEST_WS, &mut active);
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

    /// codex P2 회귀(4차): 첫 워커의 결과가 채널에 도착했지만 아직 poll 전일 때 두 번째
    /// 제스처가 오면, 완료된 결과를 먼저 반영하고 나서 새 첨부를 시작한다 — 무조건
    /// 대체하면 완성된 첨부가 유실된다.
    #[test]
    fn 완료된_첨부는_교체_직전에_먼저_반영된다() {
        let egui_ctx = egui::Context::default();
        let path = test_history_path("attach-drain-before-replace");
        let mut ui = ComposerUi::new(path.clone());
        let mut active = String::new();
        let (tx1, rx1) = std::sync::mpsc::channel();
        ui.begin_attach(&egui_ctx, rx1, TEST_WS, None, &mut active);
        // 첫 워커 완료 — 아직 poll 전.
        tx1.send(Ok(Some(vec![PathBuf::from("/x/first.png")])))
            .unwrap();
        // 두 번째 제스처 → drain 후 새 토큰.
        let (_tx2, rx2) = std::sync::mpsc::channel();
        ui.begin_attach(&egui_ctx, rx2, TEST_WS, None, &mut active);
        let new_token = ui.attach_task.as_ref().unwrap().target.token.clone();
        assert_eq!(
            active,
            format!("/x/first.png {new_token}"),
            "완료된 첫 첨부는 유실되지 않고 새 토큰이 그 뒤(캐럿)에 삽입돼야 한다"
        );
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
        let mut draft_a = "a-draft".to_owned();
        let (tx, rx) = std::sync::mpsc::channel();
        ui.begin_attach(&egui_ctx, rx, "ws-a", None, &mut draft_a);
        let token = ui.attach_task.as_ref().unwrap().target.token.clone();
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
        let mut active_b = "b-draft".to_owned();
        tx.send(Ok(Some(vec![PathBuf::from("/x/shot.png")])))
            .unwrap();
        ui.poll_attach_task(&egui_ctx, "ws-b", &mut active_b);
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
        let (tx, rx) = std::sync::mpsc::channel();
        ui.begin_attach(&egui_ctx, rx, TEST_WS, None, &mut active);
        active.clear(); // 사용자가 토큰을 지움
        tx.send(Ok(Some(vec![PathBuf::from("/x/shot.png")])))
            .unwrap();
        ui.poll_attach_task(&egui_ctx, TEST_WS, &mut active);
        assert!(active.is_empty(), "토큰이 없으면 결과를 조용히 버린다");
        // 실패(이미지/파일 아님): 토큰 제거.
        let (tx, rx) = std::sync::mpsc::channel();
        ui.begin_attach(&egui_ctx, rx, TEST_WS, None, &mut active);
        assert!(!active.is_empty());
        tx.send(Ok(None)).unwrap();
        ui.poll_attach_task(&egui_ctx, TEST_WS, &mut active);
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
        let (tx, rx) = std::sync::mpsc::channel();
        ui.begin_attach(&egui_ctx, rx, TEST_WS, None, &mut active);
        assert_eq!(
            ui.attach_task.as_ref().unwrap().target.padding,
            (false, false),
            "기존 공백 옆 삽입은 패딩을 기록하지 않는다"
        );
        tx.send(Ok(None)).unwrap(); // 이미지/파일 아님 — 실패 제거
        ui.poll_attach_task(&egui_ctx, TEST_WS, &mut active);
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
        assert!(
            shadows_send_chord(&chord(ctrl_and_command), egui::Modifiers::CTRL),
            "비-macOS 캡처(CTRL|COMMAND)도 Ctrl+Enter 전송을 가린다"
        );
        assert!(shadows_send_chord(
            &chord(egui::Modifiers::COMMAND),
            egui::Modifiers::COMMAND
        ));
        // codex P2(6차): 비-macOS 물리 Ctrl 이벤트는 ctrl|command를 둘 다 켜 —
        // Command+Enter 바인딩과 Ctrl+Enter 전송이 서로는 매치 안 돼도 같은 이벤트를
        // 만족한다. 별칭 접기 후 비교로 가림 판정이 참이어야 한다.
        assert!(
            shadows_send_chord(&chord(egui::Modifiers::COMMAND), egui::Modifiers::CTRL),
            "Command+Enter 바인딩은 Ctrl+Enter 전송을 가린다(별칭 접기)"
        );
        assert!(shadows_send_chord(
            &chord(egui::Modifiers::CTRL),
            egui::Modifiers::COMMAND
        ));
        assert!(
            !shadows_send_chord(
                &chord(egui::Modifiers::CTRL | egui::Modifiers::SHIFT),
                egui::Modifiers::CTRL
            ),
            "Shift가 다르면 다른 코드다"
        );
        assert!(
            !shadows_send_chord(
                &egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::J),
                egui::Modifiers::COMMAND
            ),
            "Enter가 아니면 가리지 않는다"
        );
    }

    /// codex P2 회귀(5차): 첨부 완료 치환이 사용자 선택을 무너뜨리면 안 된다 —
    /// 양 끝점을 각각 리베이스해 범위와 방향(primary/secondary)을 보존한다.
    #[test]
    fn 첨부_완료가_선택_범위와_방향을_보존한다() {
        let egui_ctx = egui::Context::default();
        let path = test_history_path("attach-selection");
        let mut ui = ComposerUi::new(path.clone());
        let mut active = String::new();
        let (tx, rx) = std::sync::mpsc::channel();
        ui.begin_attach(&egui_ctx, rx, TEST_WS, None, &mut active);
        // 사용자가 앞에 타이핑 + "ello"를 역방향 선택(primary=1 < secondary=5).
        active = format!("hello {active}");
        ui.pending_cursor = None; // 예약은 전 프레임에 이미 적용된 상태를 시뮬레이션
        let text_id = ComposerUi::text_id(TEST_WS);
        store_caret_range_now(&egui_ctx, text_id, 1, 5);
        tx.send(Ok(Some(vec![PathBuf::from("/x/a.png")]))).unwrap();
        ui.poll_attach_task(&egui_ctx, TEST_WS, &mut active);
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
        let (_tx_old, rx_old) = std::sync::mpsc::channel();
        ui.begin_attach(&egui_ctx, rx_old, TEST_WS, None, &mut active);
        let old_token = ui.attach_task.as_ref().unwrap().target.token.clone();
        assert_eq!(active, format!("fix {old_token} bug"));
        // 워커가 느린 동안의 두 번째 ⌘V — 조용히 버리지 않고 최신 것으로 대체
        // (터미널 PendingPaste 관례, codex P2).
        let (_tx_new, rx_new) = std::sync::mpsc::channel();
        ui.begin_attach(&egui_ctx, rx_new, TEST_WS, None, &mut active);
        let new_token = ui.attach_task.as_ref().unwrap().target.token.clone();
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

    fn composer_harness<'a>(
        catalog: &'a i18n::Catalog,
        send_key: ComposerSendKey,
        history_path: PathBuf,
    ) -> egui_kittest::Harness<'a, (ComposerUi, Vec<ComposerAction>)> {
        composer_harness_with_collapse(catalog, send_key, history_path, cmd_j())
    }

    fn composer_harness_with_collapse<'a>(
        catalog: &'a i18n::Catalog,
        send_key: ComposerSendKey,
        history_path: PathBuf,
        collapse_shortcut: Option<egui::KeyboardShortcut>,
    ) -> egui_kittest::Harness<'a, (ComposerUi, Vec<ComposerAction>)> {
        egui_kittest::Harness::new_ui_state(
            move |ui, (widget, captured): &mut (ComposerUi, Vec<ComposerAction>)| {
                let ctx = ComposerContext {
                    workspace_id: TEST_WS,
                    send_key,
                    can_send: true,
                    agent: None,
                    workspace_root: None,
                    collapse_shortcut,
                };
                if let Some(action) = widget.render(ui, catalog, &ctx) {
                    captured.push(action);
                }
            },
            (ComposerUi::new(history_path), Vec::new()),
        )
    }

    fn buffer_of(harness: &egui_kittest::Harness<'_, (ComposerUi, Vec<ComposerAction>)>) -> String {
        harness
            .state()
            .0
            .buffers
            .get(TEST_WS)
            .cloned()
            .unwrap_or_default()
    }

    fn focus_composer(harness: &mut egui_kittest::Harness<'_, (ComposerUi, Vec<ComposerAction>)>) {
        use egui_kittest::kittest::Queryable;
        harness
            .get_by_role(egui::accesskit::Role::MultilineTextInput)
            .click();
        harness.run();
    }

    #[test]
    fn kittest_enter_전송_시_send와_버퍼_클리어() {
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
        assert_eq!(
            harness.state().1,
            vec![ComposerAction::Send("hello agent".to_owned())]
        );
        assert!(
            buffer_of(&harness).is_empty(),
            "전송 후 버퍼가 비워져야 한다"
        );
        // 전송된 프롬프트는 히스토리에 영속된다.
        assert_eq!(load_history(&path), vec!["hello agent".to_owned()]);
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
        // pending 태스크 구성 — 클립보드 IO 없이 채널을 직접 쥔다.
        let (tx, rx) = std::sync::mpsc::channel();
        {
            let side_ctx = egui::Context::default();
            let ui = &mut harness.state_mut().0;
            let mut buffer = ui.buffers.remove(TEST_WS).unwrap_or_default();
            ui.begin_attach(&side_ctx, rx, TEST_WS, None, &mut buffer);
            ui.buffers.insert(TEST_WS.to_owned(), buffer);
        }
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
        tx.send(Ok(Some(vec![PathBuf::from("/x/shot.png")])))
            .unwrap();
        harness.run();
        assert_eq!(buffer_of(&harness), "/x/shot.png");
        harness.key_press(egui::Key::Enter);
        harness.run();
        assert_eq!(
            harness.state().1,
            vec![ComposerAction::Send("/x/shot.png".to_owned())]
        );
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
        assert_eq!(
            harness.state().1,
            vec![ComposerAction::Send("a\n".to_owned())]
        );
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
        assert_eq!(
            harness.state().1,
            vec![ComposerAction::Send("hello".to_owned())],
            "⌘Enter는 접힘이 아니라 전송이어야 한다"
        );
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
        save_history(&path, &["one".to_owned(), "two".to_owned()]);
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
        harness.input_mut().dropped_files.push(egui::DroppedFile {
            path: Some(PathBuf::from("/x/a.png")),
            ..Default::default()
        });
        harness.step();
        assert_eq!(buffer_of(&harness), "/x/a.png");
        assert_eq!(
            cursor_char_index(&harness.ctx, ComposerUi::text_id(TEST_WS)),
            Some("/x/a.png".chars().count()),
            "드롭 프레임에 캐럿이 즉시 삽입 끝이어야 한다"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn kittest_mcp_셀렉터가_도구_이름을_버퍼에_삽입한다() {
        use egui_kittest::kittest::Queryable;
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let path = test_history_path("mcp-insert");
        let mut harness = composer_harness(&catalog, ComposerSendKey::Enter, path.clone());
        harness
            .state_mut()
            .0
            .set_mcp_tools(vec![("srv".to_owned(), vec!["mytool".to_owned()])]);
        // 펼침(툴바 노출) — ⌘J 경로와 동일.
        harness.state_mut().0.request_focus();
        harness.run();
        harness.run();
        harness.get_by_label("MCP").click();
        harness.run();
        harness.get_by_label("srv · mytool").click();
        harness.run();
        assert_eq!(buffer_of(&harness), "mytool");
        // 삽입 후 커서는 삽입 끝이어야 한다 — 옛 위치(0)에 남으면 다음 타이핑이
        // 도구 이름 앞/안에 끼어 깨진다(codex P2).
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
        assert_eq!(cursor, "mytool".chars().count());
        std::fs::remove_file(&path).ok();
    }
}
