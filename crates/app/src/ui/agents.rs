use runtime::{RuntimeClient, RuntimeCommand, RuntimeEvent, SpawnKind};

use crate::config::TerminalConfig;
use crate::env::EnvValue;
use crate::storage::{AgentConfigRow, Db};

/// agent command 등록·실행 창 (설계문서 PR-09/10).
/// 실행 시 env profile을 선택하면 plain은 값으로, secret은 credential_id로
/// runtime에 전달된다 — resolve는 spawn 직전 worker에서 (6.3).
pub struct AgentsUi {
    open: bool,
    name: String,
    command: String,
    /// 줄바꿈으로 구분해 args array로 저장한다 (셸 문자열 파싱 금지)
    args_input: String,
    /// status detector regex 입력 (PR-12) — 비우면 미사용
    waiting_regex: String,
    approval_regex: String,
    error_regex: String,
    done_regex: String,
    /// 실행 시 적용할 env profile (None = profile 없이)
    run_profile: Option<String>,
    /// 응답(AgentSpawned/SpawnFailed)을 아직 못 받은 실행 수 — 폴링 유지
    pending_launches: u32,
    error: Option<String>,
    cached: Option<Vec<AgentConfigRow>>,
}

impl AgentsUi {
    pub fn new() -> Self {
        Self {
            open: false,
            name: String::new(),
            command: String::new(),
            args_input: String::new(),
            waiting_regex: String::new(),
            approval_regex: String::new(),
            error_regex: String::new(),
            done_regex: String::new(),
            run_profile: None,
            pending_launches: 0,
            error: None,
            cached: None,
        }
    }

    pub fn toggle(&mut self) {
        self.open = !self.open;
        if !self.open {
            self.error = None;
        }
    }

    pub fn show(
        &mut self,
        ctx: &egui::Context,
        db: &Db,
        workspace_id: &str,
        config: &TerminalConfig,
        client: &dyn RuntimeClient,
        events: &[RuntimeEvent],
    ) {
        // 실행 응답 추적 (창이 닫혀 있어도)
        for event in events {
            match event {
                RuntimeEvent::AgentSpawned { .. } => {
                    self.pending_launches = self.pending_launches.saturating_sub(1);
                }
                RuntimeEvent::SpawnFailed {
                    kind: SpawnKind::Agent,
                    message,
                } => {
                    self.pending_launches = self.pending_launches.saturating_sub(1);
                    // 실행 주체인 이 창에도 실패를 표시한다 (workspace 에러바와 별개)
                    self.error = Some(format!("실행 실패: {message}"));
                }
                _ => {}
            }
        }
        if self.pending_launches > 0 {
            // 응답이 올 때까지 폴링 유지 (keyring resolve 등으로 늦어질 수 있다)
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
        if !self.open {
            return;
        }
        let mut open = true;
        egui::Window::new("에이전트")
            .open(&mut open)
            .resizable(false)
            .show(ctx, |ui| {
                self.contents(ui, db, workspace_id, config, client)
            });
        if !open {
            self.open = false;
            self.error = None;
        }
    }

    fn contents(
        &mut self,
        ui: &mut egui::Ui,
        db: &Db,
        workspace_id: &str,
        config: &TerminalConfig,
        client: &dyn RuntimeClient,
    ) {
        let list = match &self.cached {
            Some(list) => list.clone(),
            None => match db.list_agent_configs() {
                Ok(list) => {
                    self.cached = Some(list.clone());
                    list
                }
                Err(e) => {
                    ui.colored_label(
                        ui.visuals().error_fg_color,
                        format!("목록 조회 실패: {e:#}"),
                    );
                    return;
                }
            },
        };

        if list.is_empty() {
            ui.label("등록된 에이전트가 없습니다.");
        }
        // 실행 profile 선택 (설계문서 PR-09: env profile 선택)
        let profiles = db.list_env_profiles(workspace_id).unwrap_or_default();
        ui.horizontal(|ui| {
            ui.label("실행 profile");
            let current = self
                .run_profile
                .as_ref()
                .and_then(|id| profiles.iter().find(|p| &p.id == id))
                .map(|p| p.name.clone())
                .unwrap_or_else(|| "(없음)".into());
            egui::ComboBox::from_id_salt("agent_run_profile")
                .selected_text(current)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.run_profile, None, "(없음)");
                    for profile in &profiles {
                        let label = if profile.is_production {
                            format!("⚠ {}", profile.name)
                        } else {
                            profile.name.clone()
                        };
                        ui.selectable_value(&mut self.run_profile, Some(profile.id.clone()), label);
                    }
                });
            // production guard (6.4): 실행 전 경고
            if self
                .run_profile
                .as_ref()
                .and_then(|id| profiles.iter().find(|p| &p.id == id))
                .is_some_and(|p| p.is_production)
            {
                ui.colored_label(ui.visuals().warn_fg_color, "⚠ production profile");
            }
        });

        let mut delete_id = None;
        let mut run_config = None;
        for config in &list {
            ui.horizontal(|ui| {
                ui.label(format!(
                    "{} — {} {}",
                    config.name,
                    config.command,
                    config.args.join(" ")
                ));
                if ui.button("실행").clicked() {
                    run_config = Some(config.clone());
                }
                if ui.button("삭제").clicked() {
                    delete_id = Some(config.id.clone());
                }
            });
        }
        if let Some(agent) = run_config {
            if let Err(e) = self.run(db, config, client, &agent) {
                self.error = Some(format!("{e:#}"));
            } else {
                self.error = None;
                self.pending_launches += 1;
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(50));
            }
        }
        if let Some(id) = delete_id {
            if let Err(e) = db.delete_agent_config(&id) {
                self.error = Some(format!("{e:#}"));
            } else {
                self.cached = None;
                self.error = None;
            }
        }

        ui.separator();
        ui.heading("등록");
        ui.horizontal(|ui| {
            ui.label("이름");
            ui.text_edit_singleline(&mut self.name);
        });
        ui.horizontal(|ui| {
            ui.label("command");
            ui.text_edit_singleline(&mut self.command);
        });
        ui.label("args (한 줄에 하나)");
        ui.add(
            egui::TextEdit::multiline(&mut self.args_input)
                .desired_rows(3)
                .font(egui::TextStyle::Monospace),
        );
        ui.collapsing("상태 감지 regex (선택)", |ui| {
            for (label, field) in [
                ("waiting", &mut self.waiting_regex),
                ("approval", &mut self.approval_regex),
                ("error", &mut self.error_regex),
                ("done", &mut self.done_regex),
            ] {
                ui.horizontal(|ui| {
                    ui.label(label);
                    ui.add(egui::TextEdit::singleline(field).font(egui::TextStyle::Monospace));
                });
            }
        });
        let filled = !self.name.trim().is_empty() && !self.command.trim().is_empty();
        if ui.add_enabled(filled, egui::Button::new("등록")).clicked() {
            let args: Vec<String> = self
                .args_input
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .collect();
            let opt = |s: &str| {
                let t = s.trim();
                (!t.is_empty()).then(|| t.to_owned())
            };
            match db.insert_agent_config(
                self.name.trim(),
                self.command.trim(),
                &args,
                opt(&self.waiting_regex).as_deref(),
                opt(&self.approval_regex).as_deref(),
                opt(&self.error_regex).as_deref(),
                opt(&self.done_regex).as_deref(),
            ) {
                Ok(_) => {
                    self.name.clear();
                    self.command.clear();
                    self.args_input.clear();
                    self.waiting_regex.clear();
                    self.approval_regex.clear();
                    self.error_regex.clear();
                    self.done_regex.clear();
                    self.cached = None;
                    self.error = None;
                }
                Err(e) => self.error = Some(format!("{e:#}")),
            }
        }

        if let Some(error) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
    }

    /// 선택된 profile의 env를 plain/secret(credential_id)으로 나눠 SpawnAgent를 보낸다.
    /// secret 값은 여기서 절대 다루지 않는다 (resolve는 worker에서 — 2.1/6.3).
    fn run(
        &mut self,
        db: &Db,
        config: &TerminalConfig,
        client: &dyn RuntimeClient,
        agent: &AgentConfigRow,
    ) -> anyhow::Result<()> {
        let mut env_plain = Vec::new();
        let mut env_secrets = Vec::new();
        if let Some(profile_id) = &self.run_profile {
            for var in db.list_env_vars(profile_id)? {
                match var.value {
                    EnvValue::Plain(value) => env_plain.push((var.key, value)),
                    EnvValue::Secret { credential_id } => {
                        env_secrets.push((var.key, credential_id));
                    }
                }
            }
        }
        client.send_command(RuntimeCommand::SpawnAgent {
            // 세션 영속(§11.1 sessions.agent_id)에 어느 agent 설정으로 spawn했는지 기록
            agent_config_id: Some(agent.id.clone()),
            cols: 80,
            rows: 24,
            scrollback_lines: config.scrollback_lines as usize,
            command: agent.command.clone(),
            args: agent.args.clone(),
            env_plain,
            env_secrets,
            waiting_regex: agent.waiting_regex.clone(),
            approval_regex: agent.approval_regex.clone(),
            error_regex: agent.error_regex.clone(),
            done_regex: agent.done_regex.clone(),
        })?;
        Ok(())
    }
}
