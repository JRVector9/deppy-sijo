//! 셸 창. Runtime Boundary(설계문서 2장) 준수 —
//! RuntimeCommand 전송 / RuntimeEvent 수신 / TerminalViewportSnapshot 렌더링만 한다.
//! PTY·terminal backend 직접 접근 없음 (PR-04~05 과도기 코드는 PR-06에서 제거됨).

use std::sync::Arc;

use runtime::{RuntimeClient, RuntimeCommand, RuntimeEvent, SessionId};
use terminal::{TerminalViewportSnapshot, input_mapper, renderer_egui};

use crate::config::TerminalConfig;

pub struct ShellUi {
    open: bool,
    /// SpawnShell 전송 후 ShellSpawned 대기 중
    spawn_pending: bool,
    view: Option<ShellView>,
    /// IME 조합 중 텍스트 (Preedit) — commit 전 표시용
    preedit: String,
    error: Option<String>,
}

/// runtime 이벤트로만 채워지는 화면 상태.
struct ShellView {
    session: SessionId,
    snapshot: Option<Arc<TerminalViewportSnapshot>>,
    bracketed_paste: bool,
    /// Some이면 종료됨 (내부는 exit code)
    exit_code: Option<Option<u32>>,
    cols: u16,
    rows: u16,
    /// 트랙패드 미세 스크롤 누적 (1행 미만 잔여분)
    scroll_residual: f32,
}

impl ShellUi {
    pub fn new() -> Self {
        Self {
            open: false,
            spawn_pending: false,
            view: None,
            preedit: String::new(),
            error: None,
        }
    }

    pub fn toggle(&mut self, client: &dyn RuntimeClient) {
        self.open = !self.open;
        if !self.open {
            self.close_session(client);
        }
    }

    fn close_session(&mut self, client: &dyn RuntimeClient) {
        // 종료된 세션도 KillSession — runtime이 scrollback용으로 유지 중인 backend 해제
        if let Some(view) = self.view.take()
            && let Err(e) = client.send_command(RuntimeCommand::KillSession {
                session: view.session,
            })
        {
            tracing::warn!("KillSession 전송 실패: {e:#}");
        }
        // spawn_pending은 유지 — 늦게 도착할 ShellSpawned를 계속 폴링해서 정리한다
        self.preedit.clear();
        self.error = None;
    }

    /// 창이 닫혀 있어도 이벤트는 소화한다 (상태 일관성).
    fn handle_events(&mut self, events: &[RuntimeEvent], client: &dyn RuntimeClient) {
        for event in events {
            match event {
                RuntimeEvent::ShellSpawned { session } => {
                    if !self.open {
                        // spawn 대기 중 창을 닫은 race — 뒤늦게 도착한 세션은 즉시 정리
                        self.spawn_pending = false;
                        if let Err(e) =
                            client.send_command(RuntimeCommand::KillSession { session: *session })
                        {
                            tracing::warn!("지연 spawn 정리 실패: {e:#}");
                        }
                        continue;
                    }
                    self.spawn_pending = false;
                    self.view = Some(ShellView {
                        session: *session,
                        snapshot: None,
                        bracketed_paste: false,
                        exit_code: None,
                        cols: 0,
                        rows: 0,
                        scroll_residual: 0.0,
                    });
                }
                RuntimeEvent::SpawnFailed { kind, message } => {
                    if *kind != runtime::SpawnKind::Shell {
                        continue; // agent 실패는 셸 창 소관이 아니다
                    }
                    self.spawn_pending = false;
                    if self.open {
                        self.error = Some(format!("셸 시작 실패: {message}"));
                    } else {
                        // 창을 닫은 뒤 도착한 실패는 다음 오픈에 표시하지 않는다
                        tracing::warn!("셸 시작 실패 (창 닫힘): {message}");
                    }
                }
                RuntimeEvent::Viewport {
                    session,
                    snapshot,
                    bracketed_paste,
                } => {
                    if let Some(view) = self.view.as_mut().filter(|v| v.session == *session) {
                        view.snapshot = Some(Arc::clone(snapshot));
                        view.bracketed_paste = *bracketed_paste;
                    }
                }
                // agent 세션은 셸 창 소유가 아니다 (PR-10에서 pane이 소유)
                RuntimeEvent::AgentSpawned { .. } => {}
                RuntimeEvent::SessionExited { session, exit_code } => {
                    if let Some(view) = self.view.as_mut().filter(|v| v.session == *session) {
                        view.exit_code = Some(*exit_code);
                    }
                }
            }
        }
    }

    pub fn show(
        &mut self,
        ctx: &egui::Context,
        config: &TerminalConfig,
        client: &dyn RuntimeClient,
        events: &[RuntimeEvent],
    ) {
        self.handle_events(events, client);
        if !self.open {
            if self.spawn_pending {
                // 닫힌 뒤에도 지연 ShellSpawned를 수신·정리할 때까지 폴링 유지
                ctx.request_repaint_after(std::time::Duration::from_millis(50));
            }
            return;
        }
        let mut open = true;
        egui::Window::new("셸")
            .open(&mut open)
            .default_size([720.0, 480.0])
            .show(ctx, |ui| self.contents(ui, config, client));
        if !open {
            self.open = false;
            self.close_session(client);
        } else if self.spawn_pending || self.view.as_ref().is_some_and(|v| v.exit_code.is_none()) {
            // Viewport 이벤트는 프레임 시작 시 폴링으로 수신한다
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
    }

    fn contents(&mut self, ui: &mut egui::Ui, config: &TerminalConfig, client: &dyn RuntimeClient) {
        let Some(view) = &mut self.view else {
            if self.spawn_pending {
                ui.label("셸 시작 중…");
            } else if ui.button("셸 시작").clicked() {
                self.send(
                    client,
                    RuntimeCommand::SpawnShell {
                        cols: 80,
                        rows: 24,
                        scrollback_lines: config.scrollback_lines as usize,
                    },
                );
                self.spawn_pending = true;
            }
            if let Some(error) = &self.error {
                ui.colored_label(ui.visuals().error_fg_color, error);
            }
            return;
        };

        // 창 크기 → cols/rows (resize 명령)
        let cell = renderer_egui::cell_size(ui.ctx(), config.font_size);
        let avail = ui.available_size();
        let cols = ((avail.x / cell.x) as u16).clamp(20, 500);
        let rows = (((avail.y - cell.y) / cell.y) as u16).clamp(5, 200);
        if (cols, rows) != (view.cols, view.rows) {
            (view.cols, view.rows) = (cols, rows);
            let session = view.session;
            self.send(
                client,
                RuntimeCommand::Resize {
                    session,
                    cols,
                    rows,
                },
            );
        }
        let Some(view) = &mut self.view else { return };

        // 렌더링 (최신 Viewport 이벤트 기준)
        let Some(snapshot) = view.snapshot.clone() else {
            ui.label("연결 중…");
            return;
        };
        let preedit = (!self.preedit.is_empty()).then_some(self.preedit.as_str());
        let output = renderer_egui::draw(ui, &snapshot, config.font_size, preedit);
        if output.response.clicked() {
            output.response.request_focus();
        }

        // 입력 (포커스 시) — 키/텍스트/IME/붙여넣기 → WriteInput
        if output.response.has_focus() {
            let bracketed = view.bracketed_paste;
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
            if !pending.is_empty() {
                let session = view.session;
                self.send(
                    client,
                    RuntimeCommand::WriteInput {
                        session,
                        bytes: pending,
                    },
                );
            }
        }
        let Some(view) = &mut self.view else { return };

        // 마우스 휠 → 스크롤백 (1행 미만 잔여분은 누적)
        if output.response.hovered() {
            let scroll_y = ui.input(|i| i.smooth_scroll_delta.y);
            view.scroll_residual += scroll_y / cell.y;
            let whole_rows = view.scroll_residual.trunc() as i32;
            if whole_rows != 0 {
                view.scroll_residual -= whole_rows as f32;
                let session = view.session;
                self.send(
                    client,
                    RuntimeCommand::Scroll {
                        session,
                        delta: whole_rows,
                    },
                );
                // 종료된 세션은 주기 폴링이 없다 — 응답 Viewport 수신을 보장
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(50));
            }
        }

        if let Some(view) = &self.view
            && let Some(code) = view.exit_code
        {
            ui.label(format!(
                "[프로세스 종료: exit code {}]",
                code.map_or("알 수 없음".into(), |c| c.to_string())
            ));
            if ui.button("다시 시작").clicked() {
                // 다중 세션 runtime — 종료된 세션도 명시적으로 정리해야 한다
                if let Some(view) = self.view.take() {
                    self.send(
                        client,
                        RuntimeCommand::KillSession {
                            session: view.session,
                        },
                    );
                }
                self.send(
                    client,
                    RuntimeCommand::SpawnShell {
                        cols: 80,
                        rows: 24,
                        scrollback_lines: config.scrollback_lines as usize,
                    },
                );
                self.spawn_pending = true;
            }
        }
        if let Some(error) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
    }

    fn send(&mut self, client: &dyn RuntimeClient, command: RuntimeCommand) {
        if let Err(e) = client.send_command(command) {
            self.error = Some(format!("{e:#}"));
        }
    }
}
