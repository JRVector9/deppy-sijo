//! 과도기 코드 (설계문서 PR-04 순서 주의): UI가 PTY에 직결한다.
//! terminal emulation 없이 raw 출력을 표시만 한다.
//! PR-06에서 RuntimeClient 경유로 전면 대체·삭제 예정.

use std::sync::mpsc::{Receiver, TryRecvError};

use pty::{PortablePtyBackend, PtyBackend, PtySession};

/// 표시 버퍼 상한 (과도기 — scrollback 정책은 PR-05 terminal backend에서)
const RAW_BUFFER_CAP: usize = 200_000;
/// 프레임당 드레인 상한 — 연속 대량 출력(yes 등)이 프레임을 독점하지 못하게 한다
const DRAIN_PER_FRAME_CAP: usize = 256 * 1024;

pub struct ShellUi {
    open: bool,
    session: Option<ShellSession>,
    input: String,
    error: Option<String>,
}

struct ShellSession {
    session: Box<dyn PtySession>,
    output: Receiver<Vec<u8>>,
    raw: Vec<u8>,
    exit_code: Option<u32>,
}

impl ShellUi {
    pub fn new() -> Self {
        Self {
            open: false,
            session: None,
            input: String::new(),
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
        self.input.clear();
        self.error = None;
    }

    pub fn show(&mut self, ctx: &egui::Context) {
        if !self.open {
            return;
        }
        let mut open = true;
        egui::Window::new("셸 (임시)")
            .open(&mut open)
            .default_size([640.0, 420.0])
            .show(ctx, |ui| self.contents(ui));
        if !open {
            self.open = false;
            self.close_session();
        } else if self.session.as_ref().is_some_and(|s| s.exit_code.is_none()) {
            // 과도기: reader thread 알림 대신 폴링으로 출력을 회수한다
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
    }

    fn contents(&mut self, ui: &mut egui::Ui) {
        let Some(shell) = &mut self.session else {
            if ui.button("셸 시작").clicked() {
                self.start_shell();
            }
            if let Some(error) = &self.error {
                ui.colored_label(ui.visuals().error_fg_color, error);
            }
            return;
        };

        // 출력 회수 (프레임당 상한 — 초과분은 다음 프레임에서, 즉시 repaint 요청)
        let mut drained = 0usize;
        loop {
            if drained >= DRAIN_PER_FRAME_CAP {
                ui.ctx().request_repaint();
                break;
            }
            match shell.output.try_recv() {
                Ok(chunk) => {
                    drained += chunk.len();
                    shell.raw.extend(chunk);
                    if shell.raw.len() > RAW_BUFFER_CAP {
                        let cut = shell.raw.len() - RAW_BUFFER_CAP;
                        shell.raw.drain(..cut);
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    // EOF → 종료 코드 확인
                    if shell.exit_code.is_none() {
                        shell.exit_code = shell.session.try_exit_code().unwrap_or(None);
                    }
                    break;
                }
            }
        }

        let text = strip_ansi(&String::from_utf8_lossy(&shell.raw));
        egui::ScrollArea::vertical()
            .max_height(300.0)
            .stick_to_bottom(true)
            .show(ui, |ui| {
                ui.label(egui::RichText::new(text).monospace());
            });

        if let Some(code) = shell.exit_code {
            ui.label(format!("[프로세스 종료: exit code {code}]"));
            if ui.button("다시 시작").clicked() {
                self.close_session();
                self.start_shell();
            }
            return;
        }

        ui.horizontal(|ui| {
            let edit = ui.add(
                egui::TextEdit::singleline(&mut self.input)
                    .desired_width(400.0)
                    .font(egui::TextStyle::Monospace),
            );
            let submitted = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if submitted || ui.button("전송").clicked() {
                let line = std::mem::take(&mut self.input);
                // Enter는 CR로 전달 (셸 line editor 기준)
                if let Err(e) = shell.session.write_input(format!("{line}\r").as_bytes()) {
                    self.error = Some(format!("{e:#}"));
                }
                edit.request_focus();
            }
            if ui.button("Ctrl+C").clicked()
                && let Err(e) = shell.session.write_input(b"\x03")
            {
                self.error = Some(format!("{e:#}"));
            }
        });
        if let Some(error) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
    }

    fn start_shell(&mut self) {
        match PortablePtyBackend.spawn(&pty::default_shell(), 80, 24) {
            Ok(mut session) => {
                let output = session.take_output().expect("새 세션의 output 채널");
                self.session = Some(ShellSession {
                    session,
                    output,
                    raw: Vec::new(),
                    exit_code: None,
                });
                self.error = None;
            }
            Err(e) => self.error = Some(format!("셸 시작 실패: {e:#}")),
        }
    }
}

/// 과도기 표시용 ANSI escape 제거. 정식 파싱은 PR-05 alacritty backend가 담당한다.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.peek() {
                // CSI: ESC [ ... final byte(0x40..=0x7e)
                Some('[') => {
                    chars.next();
                    for n in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&n) {
                            break;
                        }
                    }
                }
                // OSC: ESC ] ... BEL 또는 ESC \
                Some(']') => {
                    chars.next();
                    while let Some(n) = chars.next() {
                        if n == '\u{7}' {
                            break;
                        }
                        if n == '\u{1b}' {
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                // 2문자 escape (ESC =, ESC > 등)
                _ => {
                    chars.next();
                }
            },
            '\n' | '\t' => out.push(c),
            c if c.is_control() => {} // CR, BS 등은 표시에서 제외
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csi_색상_코드_제거() {
        assert_eq!(strip_ansi("\u{1b}[31mred\u{1b}[0m"), "red");
    }

    #[test]
    fn osc_타이틀_제거() {
        assert_eq!(strip_ansi("\u{1b}]0;title\u{7}text"), "text");
        assert_eq!(strip_ansi("\u{1b}]0;title\u{1b}\\text"), "text");
    }

    #[test]
    fn 일반_텍스트와_개행_유지() {
        assert_eq!(strip_ansi("line1\r\nline2\tend"), "line1\nline2\tend");
    }
}
