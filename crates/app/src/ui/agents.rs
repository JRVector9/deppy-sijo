use std::path::Path;

use anyhow::Context;
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
    /// 등록 시 "deppy 권한계층 경유(MCP proxy)" 체크 상태
    mcp_proxy_enabled: bool,
    /// 등록 시 프론트할 backend mcp_server id (체크 시에만 의미)
    mcp_proxy_server_id: Option<String>,
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
            mcp_proxy_enabled: false,
            mcp_proxy_server_id: None,
            run_profile: None,
            pending_launches: 0,
            error: None,
            cached: None,
        }
    }

    /// 런타임에 묶인 pending 실행 상태를 비운다 (workspace 전환으로 워커가 바뀔 때).
    /// 안 하면 이전 워커의 AgentSpawned를 못 받아 pending_launches가 남아 50ms
    /// repaint가 무한 예약된다 (codex 리뷰).
    pub fn clear_pending(&mut self) {
        self.pending_launches = 0;
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
                self.contents(ui, db, workspace_id, config, client, db_path)
            });
        if !open {
            self.open = false;
            self.error = None;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn contents(
        &mut self,
        ui: &mut egui::Ui,
        db: &Db,
        workspace_id: &str,
        config: &TerminalConfig,
        client: &dyn RuntimeClient,
        db_path: &Path,
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
            if let Err(e) = self.run(db, config, client, &agent, workspace_id, db_path) {
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
        // deppy 권한계층 경유 (MCP proxy) — 체크 시 spawn마다 .mcp.json을 생성해
        // 에이전트의 MCP tool 호출을 deppy-mcp-proxy로 라우팅한다 (backend 선택 필수).
        ui.checkbox(
            &mut self.mcp_proxy_enabled,
            "deppy 권한계층 경유 (MCP proxy)",
        );
        let servers = if self.mcp_proxy_enabled {
            db.list_mcp_servers().unwrap_or_default()
        } else {
            Vec::new()
        };
        if self.mcp_proxy_enabled {
            ui.horizontal(|ui| {
                ui.label("backend");
                let current = self
                    .mcp_proxy_server_id
                    .as_ref()
                    .and_then(|id| servers.iter().find(|s| &s.id == id))
                    .map(|s| s.name.clone())
                    .unwrap_or_else(|| "(선택)".into());
                egui::ComboBox::from_id_salt("agent_mcp_proxy_backend")
                    .selected_text(current)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.mcp_proxy_server_id, None, "(선택)");
                        for server in &servers {
                            ui.selectable_value(
                                &mut self.mcp_proxy_server_id,
                                Some(server.id.clone()),
                                &server.name,
                            );
                        }
                    });
            });
        }
        // proxy 미선택 상태 정합성: 체크 해제 시 선택 초기화, 저장된 backend가 목록에서
        // 사라졌으면(삭제) 선택을 비워 stale id 저장을 막는다.
        if !self.mcp_proxy_enabled {
            self.mcp_proxy_server_id = None;
        } else if let Some(id) = &self.mcp_proxy_server_id
            && !servers.iter().any(|s| &s.id == id)
        {
            self.mcp_proxy_server_id = None;
        }

        // 등록 가능 조건: 이름·command 필수. proxy 경유를 켰으면 backend 선택도 필수
        // (backend 없이 저장하면 spawn 시 라우팅할 대상이 없어 권한계층이 조용히 무력화됨).
        let proxy_ok = !self.mcp_proxy_enabled || self.mcp_proxy_server_id.is_some();
        let filled = !self.name.trim().is_empty() && !self.command.trim().is_empty() && proxy_ok;
        if self.mcp_proxy_enabled && self.mcp_proxy_server_id.is_none() {
            ui.colored_label(ui.visuals().warn_fg_color, "backend를 선택하세요");
        }
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
    #[allow(clippy::too_many_arguments)]
    fn run(
        &mut self,
        db: &Db,
        config: &TerminalConfig,
        client: &dyn RuntimeClient,
        agent: &AgentConfigRow,
        workspace_id: &str,
        db_path: &Path,
    ) -> anyhow::Result<()> {
        let mut env_plain = Vec::new();
        let mut env_secrets = Vec::new();
        if let Some(profile_id) = &self.run_profile {
            // 선택된 profile이 **현재 workspace** 것인지 실행 직전에 검증한다 —
            // workspace 전환 후 남은 이전 선택으로 다른 프로젝트의 credential이
            // 주입되는 것 방지 (§6.1, codex P1)
            let owned = db
                .list_env_profiles(workspace_id)?
                .iter()
                .any(|p| &p.id == profile_id);
            if !owned {
                self.run_profile = None;
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
                args.push("--mcp-config".to_owned());
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
