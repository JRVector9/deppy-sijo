use std::path::Path;

use anyhow::Context;
use runtime::{RuntimeClient, RuntimeCommand, RuntimeEvent, SpawnKind};

use crate::config::TerminalConfig;
use crate::env::EnvValue;
use crate::storage::{AgentConfigRow, Db, EnvProfileRow};

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
    /// 등록 시 "deppy 권한계층 경유(MCP proxy)" 체크 상태
    mcp_proxy_enabled: bool,
    /// 등록 시 프론트할 backend mcp_server id (체크 시에만 의미)
    mcp_proxy_server_id: Option<String>,
    /// 등록 시 MCP config 주입 플래그 커스텀 (체크 시에만 의미). 비우면 기본 --mcp-config.
    mcp_config_flag: String,
    /// 실행 시 적용할 env profile (None = profile 없이)
    run_profile: Option<String>,
    /// production profile 실행은 경고 표시만으로는 부족하므로 spawn 직전 확인을 요구한다.
    pending_production_run: Option<PendingProductionRun>,
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
            mcp_proxy_enabled: false,
            mcp_proxy_server_id: None,
            mcp_config_flag: String::new(),
            run_profile: None,
            pending_production_run: None,
            pending_launches: 0,
            error: None,
            cached: None,
        }
    }

    /// pending 카운트를 회수하며 비운다 — workspace 전환 시 물러나는 workspace의
    /// WorkspaceRuntime으로 이관해 "agent spawn 응답 대기 = live" 판정에 쓴다
    /// (codex: agent spawn 직후 전환 race에서 suspend가 새 PTY를 죽이는 창 봉합).
    pub fn take_pending(&mut self) -> u32 {
        std::mem::take(&mut self.pending_launches)
    }

    /// 창이 열려 있는가 (툴바 선택 하이라이트용).
    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn toggle(&mut self) {
        self.open = !self.open;
        if !self.open {
            self.error = None;
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        db: &Db,
        workspace_id: &str,
        config: &TerminalConfig,
        client: &dyn RuntimeClient,
        events: &[RuntimeEvent],
        db_path: &Path,
        catalog: &i18n::Catalog,
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
                    self.error = Some(crate::ui::render_message(catalog, message));
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
        egui::Window::new(catalog.t("agents.title", &[]))
            .open(&mut open)
            .resizable(false)
            .show(ctx, |ui| {
                self.contents(ui, db, workspace_id, config, client, db_path, catalog)
            });
        if !open {
            self.open = false;
            self.error = None;
            self.pending_production_run = None;
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn contents(
        &mut self,
        ui: &mut egui::Ui,
        db: &Db,
        workspace_id: &str,
        config: &TerminalConfig,
        client: &dyn RuntimeClient,
        db_path: &Path,
        catalog: &i18n::Catalog,
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
                        catalog.t("common.list_failed", &[("message", &format!("{e:#}"))]),
                    );
                    return;
                }
            },
        };

        if list.is_empty() {
            ui.label(catalog.t("agents.empty", &[]));
        }
        // 실행 profile 선택 (설계문서 PR-09: env profile 선택)
        let profiles = db.list_env_profiles(workspace_id).unwrap_or_default();
        ui.horizontal(|ui| {
            ui.label(catalog.t("agents.run_profile", &[]));
            let current = self
                .run_profile
                .as_ref()
                .and_then(|id| profiles.iter().find(|p| &p.id == id))
                .map(|p| p.name.clone())
                .unwrap_or_else(|| catalog.t("common.none", &[]));
            egui::ComboBox::from_id_salt("agent_run_profile")
                .selected_text(current)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.run_profile, None, catalog.t("common.none", &[]));
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
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    catalog.t("agents.production_profile", &[]),
                );
            }
        });

        let mut delete_id = None;
        let mut run_config: Option<(AgentConfigRow, Option<String>)> = None;
        for config in &list {
            ui.horizontal(|ui| {
                ui.label(format!(
                    "{} — {} {}",
                    config.name,
                    config.command,
                    agent_args_for_display(&config.args)
                ));
                if ui.button(catalog.t("action.run", &[])).clicked() {
                    if let Some((profile_id, profile_name)) =
                        production_profile_to_confirm(self.run_profile.as_deref(), &profiles)
                    {
                        self.pending_production_run = Some(PendingProductionRun {
                            agent: config.clone(),
                            profile_id,
                            profile_name,
                        });
                    } else {
                        run_config = Some((config.clone(), self.run_profile.clone()));
                    }
                }
                if ui.button(catalog.t("action.delete", &[])).clicked() {
                    delete_id = Some(config.id.clone());
                }
            });
        }
        if let Some((agent, profile_id)) = run_config {
            self.launch_agent(
                ui.ctx(),
                db,
                config,
                client,
                &agent,
                workspace_id,
                db_path,
                profile_id.as_deref(),
            );
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
        ui.heading(catalog.t("agents.register", &[]));
        ui.horizontal(|ui| {
            ui.label(catalog.t("common.name", &[]));
            ui.text_edit_singleline(&mut self.name);
        });
        ui.horizontal(|ui| {
            ui.label(catalog.t("common.command", &[]));
            ui.text_edit_singleline(&mut self.command);
        });
        ui.label(catalog.t("agents.args_one_per_line", &[]));
        ui.add(
            egui::TextEdit::multiline(&mut self.args_input)
                .desired_rows(3)
                .font(egui::TextStyle::Monospace),
        );
        ui.collapsing(catalog.t("agents.status_regex", &[]), |ui| {
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
        // deppy 권한계층 경유 (MCP proxy) — 체크 시 spawn마다 .mcp.json을 생성해
        // 에이전트의 MCP tool 호출을 deppy-mcp-proxy로 라우팅한다 (backend 선택 필수).
        ui.checkbox(
            &mut self.mcp_proxy_enabled,
            catalog.t("agents.mcp_proxy", &[]),
        );
        let servers = if self.mcp_proxy_enabled {
            db.list_mcp_servers().unwrap_or_default()
        } else {
            Vec::new()
        };
        if self.mcp_proxy_enabled {
            ui.horizontal(|ui| {
                ui.label(catalog.t("common.backend", &[]));
                let current = self
                    .mcp_proxy_server_id
                    .as_ref()
                    .and_then(|id| servers.iter().find(|s| &s.id == id))
                    .map(|s| s.name.clone())
                    .unwrap_or_else(|| catalog.t("common.select", &[]));
                egui::ComboBox::from_id_salt("agent_mcp_proxy_backend")
                    .selected_text(current)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.mcp_proxy_server_id,
                            None,
                            catalog.t("common.select", &[]),
                        );
                        for server in &servers {
                            ui.selectable_value(
                                &mut self.mcp_proxy_server_id,
                                Some(server.id.clone()),
                                &server.name,
                            );
                        }
                    });
            });
            // 주입 플래그 커스텀 (고급): 에이전트마다 규약이 달라(--mcp-config 외) 이름만 바꾼다.
            // 비우면 기본 --mcp-config. 경로는 항상 다음 arg로 붙는다(=path 규약은 후속 과제).
            ui.horizontal(|ui| {
                ui.label(catalog.t("agents.config_flag", &[]));
                ui.add(
                    egui::TextEdit::singleline(&mut self.mcp_config_flag)
                        .font(egui::TextStyle::Monospace),
                );
            });
        }
        // proxy 미선택 상태 정합성: 체크 해제 시 선택 초기화, 저장된 backend가 목록에서
        // 사라졌으면(삭제) 선택을 비워 stale id 저장을 막는다.
        if !self.mcp_proxy_enabled {
            self.mcp_proxy_server_id = None;
            self.mcp_config_flag.clear();
        } else if let Some(id) = &self.mcp_proxy_server_id
            && !servers.iter().any(|s| &s.id == id)
        {
            self.mcp_proxy_server_id = None;
        }

        // 등록 가능 조건: 이름·command 필수. proxy 경유를 켰으면 backend 선택도 필수
        // (backend 없이 저장하면 spawn 시 라우팅할 대상이 없어 권한계층이 조용히 무력화됨).
        // 설정 플래그도 켰을 때만 검사 — 비었으면 기본, 값이 있으면 '-'로 시작해야 한다.
        let proxy_ok = !self.mcp_proxy_enabled || self.mcp_proxy_server_id.is_some();
        let flag_ok =
            !self.mcp_proxy_enabled || validate_mcp_config_flag(&self.mcp_config_flag).is_ok();
        let filled =
            !self.name.trim().is_empty() && !self.command.trim().is_empty() && proxy_ok && flag_ok;
        if self.mcp_proxy_enabled && self.mcp_proxy_server_id.is_none() {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                catalog.t("agents.select_backend", &[]),
            );
        }
        if self.mcp_proxy_enabled
            && let Err(hint) = validate_mcp_config_flag(&self.mcp_config_flag)
        {
            ui.colored_label(ui.visuals().warn_fg_color, hint);
        }
        if ui
            .add_enabled(filled, egui::Button::new(catalog.t("agents.register", &[])))
            .clicked()
        {
            let args: Vec<String> = self
                .args_input
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .collect();
            if let Err(e) = Db::validate_agent_args_for_persistence(&args) {
                self.error = Some(format!("{e:#}"));
                return;
            }
            let opt = |s: &str| {
                let t = s.trim();
                (!t.is_empty()).then(|| t.to_owned())
            };
            // regex는 저장 전에 컴파일 검증 — 잘못된 패턴이 영속돼 매 spawn마다
            // 조용히 무시되는 것 방지 (codex 리뷰. detector는 무시+warn이 계약)
            for (kind, pattern) in [
                ("waiting", &self.waiting_regex),
                ("approval", &self.approval_regex),
                ("error", &self.error_regex),
                ("done", &self.done_regex),
            ] {
                let trimmed = pattern.trim();
                if !trimmed.is_empty()
                    && let Err(e) = regex::Regex::new(trimmed)
                {
                    self.error = Some(format!("{kind} regex 오류: {e}"));
                    return;
                }
            }
            // 플래그는 proxy 경유를 켰을 때만 저장 — trim 후 None/Some (빈값은 DB에서 None 정규화).
            // filled 조건에서 이미 유효성(-로 시작)을 강제했으므로 여기선 Ok만 취한다.
            let mcp_config_flag = if self.mcp_proxy_enabled {
                validate_mcp_config_flag(&self.mcp_config_flag)
                    .ok()
                    .flatten()
            } else {
                None
            };
            match db.insert_agent_config(
                self.name.trim(),
                self.command.trim(),
                &args,
                opt(&self.waiting_regex).as_deref(),
                opt(&self.approval_regex).as_deref(),
                opt(&self.error_regex).as_deref(),
                opt(&self.done_regex).as_deref(),
                self.mcp_proxy_enabled,
                self.mcp_proxy_server_id.as_deref(),
                mcp_config_flag.as_deref(),
            ) {
                Ok(_) => {
                    self.name.clear();
                    self.command.clear();
                    self.args_input.clear();
                    self.waiting_regex.clear();
                    self.approval_regex.clear();
                    self.error_regex.clear();
                    self.done_regex.clear();
                    self.mcp_proxy_enabled = false;
                    self.mcp_proxy_server_id = None;
                    self.mcp_config_flag.clear();
                    self.cached = None;
                    self.error = None;
                }
                Err(e) => self.error = Some(format!("{e:#}")),
            }
        }

        if let Some(error) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
        self.production_confirm_window(
            ui.ctx(),
            db,
            config,
            client,
            workspace_id,
            db_path,
            catalog,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn production_confirm_window(
        &mut self,
        ctx: &egui::Context,
        db: &Db,
        config: &TerminalConfig,
        client: &dyn RuntimeClient,
        workspace_id: &str,
        db_path: &Path,
        catalog: &i18n::Catalog,
    ) {
        let Some(pending) = self.pending_production_run.clone() else {
            return;
        };
        let mut action = ProductionConfirmAction::None;
        egui::Window::new(catalog.t("agents.production_confirm_title", &[]))
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.label(catalog.t(
                    "agents.production_confirm_message",
                    &[
                        ("agent", pending.agent.name.as_str()),
                        ("profile", pending.profile_name.as_str()),
                    ],
                ));
                ui.weak(catalog.t("agents.production_confirm_secret_hint", &[]));
                ui.horizontal(|ui| {
                    if ui.button(catalog.t("action.cancel", &[])).clicked() {
                        action = ProductionConfirmAction::Cancel;
                    }
                    if ui.button(catalog.t("action.run", &[])).clicked() {
                        action = ProductionConfirmAction::Run;
                    }
                });
            });
        match action {
            ProductionConfirmAction::None => {}
            ProductionConfirmAction::Cancel => {
                self.pending_production_run = None;
            }
            ProductionConfirmAction::Run => {
                self.pending_production_run = None;
                self.launch_agent(
                    ctx,
                    db,
                    config,
                    client,
                    &pending.agent,
                    workspace_id,
                    db_path,
                    Some(pending.profile_id.as_str()),
                );
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn launch_agent(
        &mut self,
        ctx: &egui::Context,
        db: &Db,
        config: &TerminalConfig,
        client: &dyn RuntimeClient,
        agent: &AgentConfigRow,
        workspace_id: &str,
        db_path: &Path,
        profile_id: Option<&str>,
    ) {
        if let Err(e) = self.run(db, config, client, agent, workspace_id, db_path, profile_id) {
            self.error = Some(format!("{e:#}"));
        } else {
            self.error = None;
            self.pending_launches += 1;
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
    }

    /// 선택된 profile의 env를 plain/secret(credential_id)으로 나눠 SpawnAgent를 보낸다.
    /// secret 값은 여기서 절대 다루지 않는다 (resolve는 worker에서 — 2.1/6.3).
    #[allow(clippy::too_many_arguments)]
    fn run(
        &mut self,
        db: &Db,
        config: &TerminalConfig,
        client: &dyn RuntimeClient,
        agent: &AgentConfigRow,
        workspace_id: &str,
        db_path: &Path,
        profile_id: Option<&str>,
    ) -> anyhow::Result<()> {
        let mut env_plain = Vec::new();
        let mut env_secrets = Vec::new();
        if let Some(profile_id) = profile_id {
            // 선택된 profile이 **현재 workspace** 것인지 실행 직전에 검증한다 —
            // workspace 전환 후 남은 이전 선택으로 다른 프로젝트의 credential이
            // 주입되는 것 방지 (§6.1, codex P1)
            let owned = db
                .list_env_profiles(workspace_id)?
                .iter()
                .any(|p| p.id == profile_id);
            if !owned {
                if self.run_profile.as_deref() == Some(profile_id) {
                    self.run_profile = None;
                }
                anyhow::bail!("선택된 profile이 현재 workspace에 없습니다 — 다시 선택하세요");
            }
            for var in db.list_env_vars(profile_id)? {
                match var.value {
                    EnvValue::Plain(value) => env_plain.push((var.key, value)),
                    EnvValue::Secret { credential_id } => {
                        env_secrets.push((var.key, credential_id));
                    }
                }
            }
        }
        // deppy 권한계층 경유가 켜져 있고 backend가 선택돼 있으면, spawn 직전 .mcp.json을
        // 생성해 --mcp-config로 붙인다 — 에이전트의 MCP tool 호출이 deppy-mcp-proxy를 거친다.
        // 사용자가 명시적으로 권한계층을 켰으므로, 파일 생성 실패 시 조용히 우회하지 않고
        // 에러로 중단한다(권한계층 무력화 방지).
        let mut args = agent.args.clone();
        if agent.mcp_proxy_enabled {
            if let Some(server_id) = &agent.mcp_proxy_server_id {
                // 저장된 backend가 그새 삭제됐을 수 있다 — 스폰 후 프록시가 "server not found"로
                // 죽어 권한계층이 무력화되느니, 스폰 전에 존재를 확인하고 명확히 중단한다(codex).
                if !db.list_mcp_servers()?.iter().any(|s| &s.id == server_id) {
                    anyhow::bail!(
                        "권한계층 backend '{server_id}'가 더는 존재하지 않습니다 — 에이전트 설정에서 다시 선택하세요"
                    );
                }
                let proxy_bin = mcp_proxy_bin()?;
                let path = write_mcp_proxy_config(&proxy_bin, db_path, &agent.id, server_id)?;
                // 에이전트별 커스텀 플래그(없으면 기본 --mcp-config). 플래그 이름만 바꾸며,
                // 경로는 항상 다음 arg로 붙는다 — `--flag=path`처럼 등호로 합치는 규약은 후속 과제.
                let flag = agent.mcp_config_flag.as_deref().unwrap_or("--mcp-config");
                args.push(flag.to_owned());
                args.push(path.to_string_lossy().into_owned());
            } else {
                anyhow::bail!("권한계층 경유가 켜졌지만 backend가 선택되지 않았습니다");
            }
        }
        client.send_command(RuntimeCommand::SpawnAgent {
            // 세션 영속(§11.1 sessions.agent_id)에 어느 agent 설정으로 spawn했는지 기록
            agent_config_id: Some(agent.id.clone()),
            cols: 80,
            rows: 24,
            scrollback_lines: config.scrollback_lines as usize,
            command: agent.command.clone(),
            args,
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

#[derive(Clone)]
struct PendingProductionRun {
    agent: AgentConfigRow,
    profile_id: String,
    profile_name: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProductionConfirmAction {
    None,
    Cancel,
    Run,
}

fn production_profile_to_confirm(
    run_profile: Option<&str>,
    profiles: &[EnvProfileRow],
) -> Option<(String, String)> {
    let id = run_profile?;
    profiles
        .iter()
        .find(|profile| profile.id == id && profile.is_production)
        .map(|profile| (profile.id.clone(), profile.name.clone()))
}

fn agent_args_for_display(args: &[String]) -> String {
    if Db::validate_agent_args_for_persistence(args).is_err() {
        "[REDACTED_ARGS]".to_owned()
    } else {
        args.join(" ")
    }
}

/// MCP config 주입 플래그 유효성 검사 (순수 함수로 분리해 단위 테스트 가능하게).
/// 비었으면 Ok(None) — 기본 `--mcp-config`를 쓴다. 값이 있으면 앞뒤 공백을 trim한 뒤
/// 반드시 `-`로 시작해야 Ok(Some(...)); 아니면 힌트 문자열을 담은 Err.
fn validate_mcp_config_flag(raw: &str) -> Result<Option<String>, &'static str> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if !trimmed.starts_with('-') {
        return Err("설정 플래그는 '-'로 시작해야 합니다 (예: --mcp-config)");
    }
    // 단일 argv로 push되므로 공백 포함 값("--a --b")은 에이전트가 인식 못 하는 한 덩어리
    // 플래그가 된다 — 플래그 '이름 하나'만 허용 (codex P2).
    if trimmed.contains(char::is_whitespace) {
        return Err(
            "설정 플래그에 공백을 넣을 수 없습니다 — 플래그 이름 하나만 (예: --mcp-config)",
        );
    }
    Ok(Some(trimmed.to_owned()))
}

/// deppy-mcp-proxy 바이너리 경로를 해석한다: 현재 실행 파일과 같은 디렉터리에 있으면
/// 그 절대경로를(개발/번들 배치), 없으면 PATH에 있다고 보고 이름만 반환한다.
/// FLAG: 릴리스 번들에 proxy 바이너리를 함께 담는 패키징은 후속 과제.
/// deppy-mcp-proxy 바이너리 경로. 앱 실행 파일 **옆**에 함께 배포되는 것을 계약으로 한다
/// (dev: target/*/에 함께 빌드, release: 번들에 함께 복사). 옆에 없으면 PATH를 막연히
/// 믿지 않고 에러 — 권한계층 경유를 켠 스폰이 프록시 없이 조용히 진행되지 않게 한다(codex).
fn mcp_proxy_bin() -> anyhow::Result<String> {
    let exe = std::env::current_exe().context("현재 실행 파일 경로 조회 실패")?;
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow::anyhow!("실행 파일의 디렉터리를 알 수 없음: {}", exe.display()))?;
    // 플랫폼 실행 파일 접미사 사용 — Windows는 deppy-mcp-proxy.exe (codex).
    let candidate = dir.join(format!("deppy-mcp-proxy{}", std::env::consts::EXE_SUFFIX));
    if candidate.is_file() {
        Ok(candidate.to_string_lossy().into_owned())
    } else {
        anyhow::bail!(
            "deppy-mcp-proxy 바이너리를 앱 옆({})에서 찾을 수 없습니다 — 패키징/설치를 확인하세요",
            dir.display()
        )
    }
}

/// deppy-mcp-proxy를 프론트하는 .mcp.json(MCP client 포맷) 내용을 만든다. secret은 절대
/// 담지 않는다 — db 경로와 backend server id만. (순수 함수로 분리해 단위 테스트 가능하게.)
fn build_mcp_proxy_config_json(proxy_bin: &str, db_path: &str, server_id: &str) -> String {
    let value = serde_json::json!({
        "mcpServers": {
            "deppy-proxy": {
                "command": proxy_bin,
                "args": ["--db", db_path, "--server", server_id],
            }
        }
    });
    // json! 값의 직렬화는 실패하지 않는다.
    serde_json::to_string_pretty(&value).expect("mcp config JSON 직렬화")
}

/// agent별 .mcp.json을 `<data_dir>/mcp-proxy-configs/<agent_id>.mcp.json`에 쓴다.
/// data_dir은 db_path의 상위 디렉터리(= 앱 data dir). db 경로가 바뀔 수 있으므로 매 spawn마다
/// 덮어쓴다. 파일에는 secret이 없다(db 경로 + server id만). 쓴 경로를 반환.
fn write_mcp_proxy_config(
    proxy_bin: &str,
    db_path: &Path,
    agent_id: &str,
    server_id: &str,
) -> anyhow::Result<std::path::PathBuf> {
    let data_dir = db_path.parent().ok_or_else(|| {
        anyhow::anyhow!("db 경로에 상위 디렉터리가 없습니다: {}", db_path.display())
    })?;
    let dir = data_dir.join("mcp-proxy-configs");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("mcp-proxy-configs 디렉터리 생성 실패: {}", dir.display()))?;
    let path = dir.join(format!("{agent_id}.mcp.json"));
    let content = build_mcp_proxy_config_json(proxy_bin, &db_path.to_string_lossy(), server_id);
    std::fs::write(&path, content)
        .with_context(|| format!(".mcp.json 쓰기 실패: {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_mcp_config_flag_규칙() {
        // 내부 공백("--a --b" 주입류)은 거부 — 단일 argv라 한 덩어리 플래그가 됨 (codex P2)
        assert!(validate_mcp_config_flag("--a --b").is_err());
        assert!(validate_mcp_config_flag("--mcp-config /tmp/x").is_err());
        // 빈값/공백 → None (기본 --mcp-config 사용)
        assert_eq!(validate_mcp_config_flag(""), Ok(None));
        assert_eq!(validate_mcp_config_flag("   "), Ok(None));
        // 앞뒤 공백은 trim되고 '-'로 시작하면 통과
        assert_eq!(
            validate_mcp_config_flag("  --mcp-config-file  "),
            Ok(Some("--mcp-config-file".to_owned()))
        );
        assert_eq!(validate_mcp_config_flag("-c"), Ok(Some("-c".to_owned())));
        // '-'로 시작하지 않으면 거부
        assert!(validate_mcp_config_flag("mcp-config").is_err());
        assert!(validate_mcp_config_flag("config=path").is_err());
    }

    #[test]
    fn agent_args_display는_secret_like_payload를_숨긴다() {
        let rendered = agent_args_for_display(&[
            "--api-key".to_owned(),
            "sk-ui-agent-secret-never-rendered".to_owned(),
        ]);
        assert_eq!(rendered, "[REDACTED_ARGS]");
        assert!(!rendered.contains("sk-ui-agent-secret"));
        assert_eq!(
            agent_args_for_display(&["build".to_owned(), "--release".to_owned()]),
            "build --release"
        );
    }

    #[test]
    fn production_profile은_실행전_confirm_대상이다() {
        let profiles = vec![
            EnvProfileRow {
                id: "dev".to_owned(),
                name: "Development".to_owned(),
                kind: "local".to_owned(),
                is_production: false,
            },
            EnvProfileRow {
                id: "prod".to_owned(),
                name: "Production".to_owned(),
                kind: "production".to_owned(),
                is_production: true,
            },
        ];
        assert_eq!(
            production_profile_to_confirm(Some("prod"), &profiles),
            Some(("prod".to_owned(), "Production".to_owned()))
        );
        assert_eq!(production_profile_to_confirm(Some("dev"), &profiles), None);
        assert_eq!(production_profile_to_confirm(None, &profiles), None);
        assert_eq!(
            production_profile_to_confirm(Some("missing"), &profiles),
            None
        );
    }

    #[test]
    fn 커스텀_플래그가_args에_반영된다() {
        // run() 스폰 배선의 핵심: 커스텀 플래그가 있으면 그 이름을, 없으면 기본을 쓰고
        // 경로는 항상 다음 arg로 붙는다.
        let with_custom: Option<String> = Some("--mcp-config-file".to_owned());
        assert_eq!(
            with_custom.as_deref().unwrap_or("--mcp-config"),
            "--mcp-config-file"
        );
        let default: Option<String> = None;
        assert_eq!(default.as_deref().unwrap_or("--mcp-config"), "--mcp-config");
    }

    #[test]
    fn mcp_proxy_config_json에_db경로와_server가_담긴다() {
        let json =
            build_mcp_proxy_config_json("deppy-mcp-proxy", "/data/metadata.sqlite3", "srv-1");
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let server = &value["mcpServers"]["deppy-proxy"];
        assert_eq!(server["command"], "deppy-mcp-proxy");
        // args는 정확히 --db <path> --server <id> 순서
        assert_eq!(
            server["args"],
            serde_json::json!(["--db", "/data/metadata.sqlite3", "--server", "srv-1"])
        );
    }

    #[test]
    fn write_mcp_proxy_config는_config를_data_dir하위에_쓴다() {
        let dir = std::env::temp_dir().join(format!("deppy-mcpcfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("metadata.sqlite3");
        let path =
            write_mcp_proxy_config("deppy-mcp-proxy", &db_path, "agent-42", "srv-backend").unwrap();
        assert_eq!(
            path,
            dir.join("mcp-proxy-configs").join("agent-42.mcp.json")
        );
        let written = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&written).unwrap();
        assert_eq!(
            value["mcpServers"]["deppy-proxy"]["args"][1],
            db_path.to_string_lossy().as_ref()
        );
        assert_eq!(value["mcpServers"]["deppy-proxy"]["args"][3], "srv-backend");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
