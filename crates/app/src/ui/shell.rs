//! 과도기 코드 (설계문서 PR-04~05 순서 주의): UI가 PTY/terminal backend에 직결한다.
//! PR-06에서 RuntimeClient 경유로 전면 대체·삭제 예정.

use std::sync::mpsc::{Receiver, TryRecvError};

use pty::{PortablePtyBackend, PtyBackend, PtySession};
use terminal::{AlacrittyBackend, TerminalBackend, input_mapper, renderer_egui};

use crate::config::TerminalConfig;

/// 프레임당 드레인 상한 — 연속 대량 출력(yes 등)이 프레임을 독점하지 못하게 한다
const DRAIN_PER_FRAME_CAP: usize = 256 * 1024;

pub struct ShellUi {
    open: bool,
    session: Option<ShellSession>,
    /// IME 조합 중 텍스트 (Preedit) — commit 전 표시용
    preedit: String,
    error: Option<String>,
}

struct ShellSession {
    session: Box<dyn PtySession>,
    output: Receiver<Vec<u8>>,
    backend: AlacrittyBackend,
    cols: u16,
    rows: u16,
    exit_code: Option<u32>,
    /// 트랙패드 미세 스크롤 누적 (1행 미만 잔여분)
    scroll_residual: f32,
}

impl ShellUi {
    pub fn new() -> Self {
        Self {
            open: false,
            session: None,
            preedit: String::new(),
            error: None,
        }
    }

    pub fn toggle(&mut self) {
        self.open = !self.open;
        if !self.open {
            self.close_session();
        }
    }

    fn close_session(&mut self) {
        // 프로세스 정리는 PtySession Drop이 보장한다 (process group SIGHUP + reap)
        self.session = None;
        self.preedit.clear();
        self.error = None;
    }

    pub fn show(&mut self, ctx: &egui::Context, config: &TerminalConfig) {
        if !self.open {
            return;
        }
        let mut open = true;
        egui::Window::new("셸 (임시)")
            .open(&mut open)
            .default_size([720.0, 480.0])
            .show(ctx, |ui| self.contents(ui, config));
        if !open {
            self.open = false;
            self.close_session();
        } else if self.session.as_ref().is_some_and(|s| s.exit_code.is_none()) {
            // 과도기: reader thread 알림 대신 폴링으로 출력을 회수한다
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
    }

    fn contents(&mut self, ui: &mut egui::Ui, config: &TerminalConfig) {
        let Some(shell) = &mut self.session else {
            if ui.button("셸 시작").clicked() {
                self.start_shell(ui.ctx(), config);
            }
            if let Some(error) = &self.error {
                ui.colored_label(ui.visuals().error_fg_color, error);
            }
            return;
        };

        // 창 크기 → cols/rows (resize)
        let cell = renderer_egui::cell_size(ui.ctx(), config.font_size);
        let avail = ui.available_size();
        let cols = ((avail.x / cell.x) as u16).clamp(20, 500);
        let rows = (((avail.y - cell.y) / cell.y) as u16).clamp(5, 200);
        if (cols, rows) != (shell.cols, shell.rows) {
            let _ = shell.backend.resize(cols, rows);
            if let Err(e) = shell.session.resize(cols, rows) {
                tracing::warn!("PTY resize 실패: {e:#}");
            }
            (shell.cols, shell.rows) = (cols, rows);
        }

        // 출력 회수 (프레임당 상한 — 초과분은 다음 프레임에서)
        let mut drained = 0usize;
        loop {
            if drained >= DRAIN_PER_FRAME_CAP {
                ui.ctx().request_repaint();
                break;
            }
            match shell.output.try_recv() {
                Ok(chunk) => {
                    drained += chunk.len();
                    match shell.backend.feed(&chunk) {
                        Ok(changes) => {
                            // 터미널 질의(DA 등) 응답은 PTY로 되돌려 쓴다
                            if !changes.pty_responses.is_empty()
                                && let Err(e) = shell.session.write_input(&changes.pty_responses)
                            {
                                tracing::warn!("터미널 질의 응답 전송 실패: {e:#}");
                            }
                        }
                        Err(e) => tracing::warn!("terminal feed 실패: {e:#}"),
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if shell.exit_code.is_none() {
                        shell.exit_code = shell.session.try_exit_code().unwrap_or(None);
                    }
                    break;
                }
            }
        }

        // 렌더링
        let Some(snapshot) = shell.backend.viewport_snapshot() else {
            return;
        };
        let preedit = (!self.preedit.is_empty()).then_some(self.preedit.as_str());
        let output = renderer_egui::draw(ui, &snapshot, config.font_size, preedit);
        if output.response.clicked() {
            output.response.request_focus();
        }

        // 입력 (포커스 시) — 키/텍스트/IME/붙여넣기 → PTY
        if output.response.has_focus() {
            let bracketed = shell.backend.bracketed_paste();
            let mut pending: Vec<u8> = Vec::new();
            ui.input(|input| {
                let modifiers = input.modifiers;
                for event in &input.raw.events {
                    // IME 조합 중 텍스트는 표시 상태로만 유지
                    if let egui::Event::Ime(egui::ImeEvent::Preedit { text, .. }) = event {
                        self.preedit = text.clone();
                        continue;
                    }
                    if let egui::Event::Ime(egui::ImeEvent::Commit(_)) = event {
                        self.preedit.clear();
                    }
                    if let Some(bytes) = input_mapper::map_event(event, bracketed, &modifiers) {
                        pending.extend(bytes);
                    }
                }
            });
            if !pending.is_empty()
                && let Err(e) = shell.session.write_input(&pending)
            {
                self.error = Some(format!("{e:#}"));
            }
        }

        // 마우스 휠 → 스크롤백 (1행 미만 잔여분은 누적)
        if output.response.hovered() {
            let scroll_y = ui.input(|i| i.smooth_scroll_delta.y);
            shell.scroll_residual += scroll_y / cell.y;
            let whole_rows = shell.scroll_residual.trunc() as i32;
            if whole_rows != 0 {
                shell.backend.scroll(whole_rows);
                shell.scroll_residual -= whole_rows as f32;
                // snapshot은 이미 그려졌다 — 새 offset이 바로 보이게 재도장 요청
                ui.ctx().request_repaint();
            }
        }

        if let Some(code) = shell.exit_code {
            ui.label(format!("[프로세스 종료: exit code {code}]"));
            if ui.button("다시 시작").clicked() {
                let config = config.clone();
                self.close_session();
                self.start_shell(ui.ctx(), &config);
            }
        }
        if let Some(error) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
    }

    fn start_shell(&mut self, ctx: &egui::Context, config: &TerminalConfig) {
        let (cols, rows) = (80, 24); // 첫 프레임에서 창 크기에 맞춰 resize된다
        match PortablePtyBackend.spawn(&pty::default_shell(), cols, rows) {
            Ok(mut session) => {
                let output = session.take_output().expect("새 세션의 output 채널");
                self.session = Some(ShellSession {
                    session,
                    output,
                    backend: AlacrittyBackend::new(cols, rows, config.scrollback_lines as usize),
                    cols,
                    rows,
                    exit_code: None,
                    scroll_residual: 0.0,
                });
                self.error = None;
                ctx.request_repaint();
            }
            Err(e) => self.error = Some(format!("셸 시작 실패: {e:#}")),
        }
    }
}
