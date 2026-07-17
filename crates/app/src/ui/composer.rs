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
    /// 히스토리 recall 직후 커서를 끝으로 — TextEdit 상태는 show 후에만 만질 수 있다.
    cursor_to_end: bool,
    /// 히스토리 영속 파일 (jsonl — 한 줄 = JSON 문자열 하나).
    history_path: PathBuf,
    /// 클립보드 이미지/파일 paste 백그라운드 태스크 (터미널과 동일 인프라 재사용).
    paste_rx: Option<std::sync::mpsc::Receiver<anyhow::Result<Option<Vec<PathBuf>>>>>,
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
            cursor_to_end: false,
            history_path,
            paste_rx: None,
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
        // 직전 프레임의 포커스 — 이번 프레임 TextEdit이 그려지기 전이라 memory가 그 값이다.
        let had_focus = egui_ctx.memory(|memory| memory.has_focus(text_id));
        let focus_requested = std::mem::take(&mut self.focus_requested);
        if focus_requested {
            self.expanded = true;
        }

        // ── 키 가로채기 (TextEdit이 그려지기 전에 소비 여부를 판단) ──
        let mut send_requested = false;
        if had_focus {
            // ⌘J 접기 — 포커스를 내려놓으면 터미널이 다음 프레임에 키보드를 자연 회수한다
            // (terminal_keyboard_input_allowed가 text_edit_focused=false를 본다).
            if consume_key_exact(&egui_ctx, egui::Modifiers::COMMAND, egui::Key::J) {
                self.expanded = false;
                egui_ctx.memory_mut(|memory| memory.surrender_focus(text_id));
            }
            // IME 가드: macOS 한글 IME의 조합 확정 Enter는 `Ime::Commit`으로 소비되고
            // "\n" commit은 egui가 무시한다(egui 0.35 + winit 백포트 — 리뷰로 검증).
            // 그래도 통합 밖 IME/이벤트 순서 변화로 Key(Enter)가 같은 프레임에 새어
            // 들어올 수 있으므로, 이번 프레임에 Ime 이벤트가 있으면 Enter를 전송으로
            // 치지 않는다 — 조합 확정이 프롬프트 오발사가 되지 않게.
            let ime_active =
                egui_ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Ime(_))));
            if !ime_active {
                let modifiers = match ctx.send_key {
                    ComposerSendKey::Enter => egui::Modifiers::NONE,
                    ComposerSendKey::CmdEnter => egui::Modifiers::COMMAND,
                    ComposerSendKey::CtrlEnter => egui::Modifiers::CTRL,
                };
                if consume_key_exact(&egui_ctx, modifiers, egui::Key::Enter) {
                    send_requested = true;
                }
            }
            self.intercept_history_keys(&egui_ctx, text_id, buffer);
            self.start_clipboard_paste_if_requested(&egui_ctx);
        }
        self.poll_clipboard_paste(&egui_ctx, text_id, ctx.workspace_root, buffer);

        // ── 전송 판정 (빈 내용/세션 없음이면 무시 — Enter는 이미 소비돼 개행도 안 된다) ──
        // 키 경유 전송은 TextEdit을 그리기 전에 처리해 비워진 버퍼가 이번 프레임에 보인다.
        let mut action = None;
        if send_requested {
            action = self.try_submit(buffer, ctx.can_send);
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

        // 포커스/펼침 전이 — 포커스가 생기면 펼치고, recall 직후 커서를 끝으로.
        if std::mem::take(&mut self.cursor_to_end) {
            let mut state = output.state.clone();
            state
                .cursor
                .set_char_range(Some(egui::text::CCursorRange::one(
                    egui::text::CCursor::new(buffer.chars().count()),
                )));
            state.store(&egui_ctx, text_id);
        }
        if focus_requested {
            output.response.request_focus();
        }
        if output.response.has_focus() {
            self.expanded = true;
        }
        if output.response.changed() {
            // 사용자가 직접 편집 — 히스토리 탐색 종료(recall 텍스트가 드래프트가 된다).
            self.history_pos = None;
        }

        // 전송 버튼 경유 요청 — 키 경유와 같은 규칙(빈 내용/세션 없음 무시).
        if send_requested && action.is_none() {
            action = self.try_submit(buffer, ctx.can_send);
        }
        if action.is_some() {
            // 전송 후에도 포커스 유지 — 연속 프롬프트 작성(ChatGPT 관례).
            output.response.request_focus();
        }

        // 파일 드래그&드롭 — 도크 위에 놓인 OS 파일 드롭을 @멘션으로 삽입.
        self.accept_dropped_files(&egui_ctx, ui.min_rect(), ctx.workspace_root, buffer);

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
    fn try_submit(&mut self, buffer: &mut String, can_send: bool) -> Option<ComposerAction> {
        if !can_send || buffer.trim().is_empty() {
            return None;
        }
        let prompt = std::mem::take(buffer);
        push_history(&mut self.history, prompt.clone());
        save_history(&self.history_path, &self.history);
        self.history_pos = None;
        Some(ComposerAction::Send(prompt))
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
            self.cursor_to_end = true;
        }
        if browsing
            && on_last_line
            && consume_key_exact(egui_ctx, egui::Modifiers::NONE, egui::Key::ArrowDown)
        {
            match self.history_pos {
                Some(pos) if pos + 1 < self.history.len() => {
                    self.history_pos = Some(pos + 1);
                    *buffer = self.history[pos + 1].clone();
                    self.cursor_to_end = true;
                }
                _ => {
                    // 최신을 지나면 빈 드래프트로 복귀 — 탐색 종료.
                    self.history_pos = None;
                    buffer.clear();
                }
            }
        }
    }

    /// 이미지-only 클립보드 ⌘V: egui-winit이 이벤트 없이 소비하므로(Event::Paste도
    /// Key도 없음) AppKit native monitor의 기록으로만 감지된다 — 터미널(workspace)과
    /// 같은 원천. 컴포저가 포커스일 때만 drain하므로 터미널 흐름을 훔치지 않는다
    /// (터미널은 text_edit_focused면 키보드 비활성이라 그 프레임 기록을 쓰지 않는다).
    fn start_clipboard_paste_if_requested(&mut self, egui_ctx: &egui::Context) {
        let native = crate::native_key_monitor::drain();
        if !native.clipboard_paste || self.paste_rx.is_some() {
            return;
        }
        // 텍스트 paste(Event::Paste)가 같은 프레임에 있으면 TextEdit 기본 붙여넣기가
        // 처리한다 — 스크린샷 등 이미지/파일 클립보드만 백그라운드로 경로화한다.
        let has_text_paste =
            egui_ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Paste(_))));
        if has_text_paste {
            return;
        }
        self.paste_rx = Some(
            crate::ui::clipboard_image::paste_clipboard_paths_or_image_background(
                egui_ctx.clone(),
                false,
            ),
        );
    }

    /// paste 태스크 완료 폴링 — 경로가 나오면 커서 위치에 @멘션으로 삽입.
    fn poll_clipboard_paste(
        &mut self,
        egui_ctx: &egui::Context,
        text_id: egui::Id,
        workspace_root: Option<&Path>,
        buffer: &mut String,
    ) {
        let Some(rx) = &self.paste_rx else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(Some(paths))) => {
                let cursor = cursor_char_index(egui_ctx, text_id);
                for path in &paths {
                    insert_snippet(buffer, cursor, &mention_path(workspace_root, path));
                }
                self.paste_rx = None;
            }
            Ok(Ok(None)) => self.paste_rx = None,
            Ok(Err(e)) => {
                tracing::warn!("컴포저 클립보드 paste 실패: {e:#}");
                self.paste_rx = None;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => self.paste_rx = None,
        }
    }

    /// OS 파일 드롭 — 포인터가 도크 위에 있을 때만 받아 @멘션으로 삽입한다.
    fn accept_dropped_files(
        &mut self,
        egui_ctx: &egui::Context,
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
        for path in &dropped {
            insert_snippet(buffer, None, &mention_path(workspace_root, path));
        }
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
                insert_snippet(buffer, cursor, &mention_path(ctx.workspace_root, &path));
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
                            prepend_model_command(buffer, model);
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
                            insert_snippet(buffer, cursor, tool);
                        }
                    }
                }
            });
            // 우측: 전송 버튼 + 전송 키 힌트.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let enabled = ctx.can_send && !buffer.trim().is_empty();
                if ui
                    .add_enabled(enabled, egui::Button::new("↑").corner_radius(8))
                    .on_hover_text(catalog.t("composer.send", &[]))
                    .clicked()
                {
                    send_clicked = true;
                }
                let hint_key = match ctx.send_key {
                    ComposerSendKey::Enter => "composer.hint.enter",
                    ComposerSendKey::CmdEnter => "composer.hint.cmd_enter",
                    ComposerSendKey::CtrlEnter => "composer.hint.ctrl_enter",
                };
                ui.add(egui::Label::new(
                    egui::RichText::new(catalog.t(hint_key, &[]))
                        .size(11.0)
                        .weak(),
                ));
            });
        });
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

/// 현재 TextEdit 커서의 문자 인덱스 (직전 프레임 상태 — 삽입 위치 결정용).
fn cursor_char_index(egui_ctx: &egui::Context, text_id: egui::Id) -> Option<usize> {
    egui::text_edit::TextEditState::load(egui_ctx, text_id)
        .and_then(|state| state.cursor.char_range())
        .map(|range| range.primary.index.into())
}

/// 커서(문자 인덱스) 위치에 스니펫을 삽입한다 — 단어에 눌어붙지 않게 앞뒤 공백을 보정.
/// None이면 끝에 붙인다.
fn insert_snippet(buffer: &mut String, cursor_chars: Option<usize>, snippet: &str) {
    let char_len = buffer.chars().count();
    let at = cursor_chars.unwrap_or(char_len).min(char_len);
    let byte = buffer
        .char_indices()
        .nth(at)
        .map(|(byte, _)| byte)
        .unwrap_or(buffer.len());
    let mut piece = String::new();
    if byte > 0 && !buffer[..byte].ends_with(char::is_whitespace) {
        piece.push(' ');
    }
    piece.push_str(snippet);
    if byte < buffer.len() && !buffer[byte..].starts_with(char::is_whitespace) {
        piece.push(' ');
    }
    buffer.insert_str(byte, &piece);
}

/// `/model <이름>`을 버퍼 맨 앞에 삽입 — claude/codex 모두 슬래시 커맨드는 줄 단위라
/// 개행으로 분리한다. 전송은 사용자가 검토 후 직접 한다.
fn prepend_model_command(buffer: &mut String, model: &str) {
    buffer.insert_str(0, &format!("/model {model}\n"));
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
    fn insert_snippet_은_경계_공백을_보정한다() {
        let mut buffer = String::new();
        insert_snippet(&mut buffer, None, "@a.txt");
        assert_eq!(buffer, "@a.txt");
        let mut buffer = "fix".to_owned();
        insert_snippet(&mut buffer, None, "@a.txt");
        assert_eq!(buffer, "fix @a.txt");
        let mut buffer = "fix bug".to_owned();
        insert_snippet(&mut buffer, Some(3), "@a.txt"); // "fix|" 커서 뒤가 공백
        assert_eq!(buffer, "fix @a.txt bug");
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
    fn prepend_model_command_는_버퍼_맨_앞에_삽입한다() {
        let mut buffer = "프롬프트".to_owned();
        prepend_model_command(&mut buffer, "opus");
        assert_eq!(buffer, "/model opus\n프롬프트");
    }

    // ── kittest 상호작용 테스트 (inbox_waiting.rs 관례 — type_text 전 click 포커스 필수) ──

    const TEST_WS: &str = "ws-test";

    fn test_history_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "deppy-composer-kittest-{}-{name}.jsonl",
            std::process::id()
        ))
    }

    fn composer_harness<'a>(
        catalog: &'a i18n::Catalog,
        send_key: ComposerSendKey,
        history_path: PathBuf,
    ) -> egui_kittest::Harness<'a, (ComposerUi, Vec<ComposerAction>)> {
        egui_kittest::Harness::new_ui_state(
            move |ui, (widget, captured): &mut (ComposerUi, Vec<ComposerAction>)| {
                let ctx = ComposerContext {
                    workspace_id: TEST_WS,
                    send_key,
                    can_send: true,
                    agent: None,
                    workspace_root: None,
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
        std::fs::remove_file(&path).ok();
    }
}
