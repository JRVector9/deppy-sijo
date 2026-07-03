//! Connector Center (설계문서 §3 ConnectorCenter, PR-17/PR-18).
//! local MCP 서버를 카드로 나열하고 쉽게 추가 + 연결 상태를 표시한다.
//! 연결 테스트(discover_tools)는 subprocess 왕복이라 백그라운드 스레드에서 돌리고,
//! 결과는 채널로 받아 UI에 반영 + mcp_tools를 DB에 교체 저장한다.
//! OAuth 커넥터(PR-18): 브라우저 승인 대기가 길어 flow 전체를 백그라운드로 돌리고,
//! 획득한 토큰은 UI 스레드에서 keyring 저장 + credentials 등록 + redaction 시드.

use std::collections::HashMap;
use std::sync::mpsc;
use std::time::Duration;

use mcp::{LocalMcpManager, McpServerConfig, McpServerRow, McpTool, McpToolRow};
use secret::{KeyringSecretStore, RedactionService, SecretStore};

use crate::storage::{CredentialMeta, Db};

/// 서버별 연결 상태 (완료 기준: 연결 상태 표시).
enum ConnStatus {
    Checking,
    Connected { tools: usize },
    Failed(String),
}

/// 백그라운드 연결 테스트 결과: (server_id, 발견한 tools 또는 에러 문자열).
type DiscoverResult = (String, Result<Vec<McpTool>, String>);

/// OAuth flow 결과: 입력했던 label과 (토큰 또는 에러 문자열).
type OAuthResult = (String, Result<auth::OAuthToken, String>);

/// OAuth 연결 진행 상태.
enum OAuthStatus {
    Waiting,
    Done(String),
    Failed(String),
}

pub struct ConnectorsUi {
    redaction: RedactionService,
    open: bool,
    // 추가 폼
    name: String,
    command: String,
    /// 한 줄에 하나 — agents 등록과 같은 관례 (셸 문자열 파싱 금지)
    args_input: String,
    error: Option<String>,
    cached: Option<Vec<McpServerRow>>,
    status: HashMap<String, ConnStatus>,
    result_tx: mpsc::Sender<DiscoverResult>,
    result_rx: mpsc::Receiver<DiscoverResult>,
    // OAuth 폼 (PR-18)
    oauth_label: String,
    oauth_auth_url: String,
    oauth_token_url: String,
    oauth_client_id: String,
    /// 공백 구분
    oauth_scopes: String,
    oauth_status: Option<OAuthStatus>,
    oauth_tx: mpsc::Sender<OAuthResult>,
    oauth_rx: mpsc::Receiver<OAuthResult>,
}

impl ConnectorsUi {
    pub fn new(redaction: RedactionService) -> Self {
        let (result_tx, result_rx) = mpsc::channel();
        let (oauth_tx, oauth_rx) = mpsc::channel();
        Self {
            redaction,
            open: false,
            name: String::new(),
            command: String::new(),
            args_input: String::new(),
            error: None,
            cached: None,
            status: HashMap::new(),
            result_tx,
            result_rx,
            oauth_label: String::new(),
            oauth_auth_url: String::new(),
            oauth_token_url: String::new(),
            oauth_client_id: String::new(),
            oauth_scopes: String::new(),
            oauth_status: None,
            oauth_tx,
            oauth_rx,
        }
    }

    pub fn toggle(&mut self) {
        self.open = !self.open;
        self.error = None;
    }

    /// 새 credential이 등록됐으면 true (호출측이 자격증명 창 캐시를 무효화).
    pub fn show(&mut self, ctx: &egui::Context, db: &mut Db) -> bool {
        // 백그라운드 결과는 창이 닫혀 있어도 소화한다 (다시 열 때 최신 상태)
        self.drain_results(db);
        let credential_added = self.drain_oauth(db);
        if !self.open {
            return credential_added;
        }
        let mut open = true;
        egui::Window::new("연결")
            .open(&mut open)
            .resizable(false)
            .show(ctx, |ui| self.contents(ui, ctx, db));
        self.open = open;
        credential_added
    }

    fn contents(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, db: &mut Db) {
        let servers = match &self.cached {
            Some(list) => list.clone(),
            None => match db.list_mcp_servers() {
                Ok(list) => {
                    self.cached = Some(list.clone());
                    list
                }
                Err(e) => {
                    ui.colored_label(egui::Color32::RED, format!("목록 조회 실패: {e:#}"));
                    return;
                }
            },
        };

        ui.heading("Local MCP");
        if servers.is_empty() {
            ui.label("등록된 MCP 서버가 없습니다.");
        }
        for server in &servers {
            self.server_card(ui, ctx, server);
        }

        ui.separator();
        ui.label("MCP 서버 추가 (stdio)");
        ui.horizontal(|ui| {
            ui.label("이름");
            ui.text_edit_singleline(&mut self.name);
        });
        ui.horizontal(|ui| {
            ui.label("command");
            ui.text_edit_singleline(&mut self.command);
        });
        ui.label("args (한 줄에 하나) — secret은 args가 아니라 자격증명/환경으로");
        ui.add(
            egui::TextEdit::multiline(&mut self.args_input)
                .desired_rows(2)
                .hint_text("-y\nserver-filesystem"),
        );
        if ui.button("추가").clicked() {
            self.add_server(db);
        }
        if let Some(error) = &self.error {
            ui.colored_label(egui::Color32::RED, error);
        }

        // OAuth 커넥터 (PR-18): external browser + PKCE + localhost callback
        ui.separator();
        ui.heading("OAuth 커넥터");
        ui.horizontal(|ui| {
            ui.label("이름");
            ui.text_edit_singleline(&mut self.oauth_label);
        });
        ui.horizontal(|ui| {
            ui.label("authorize URL");
            ui.text_edit_singleline(&mut self.oauth_auth_url);
        });
        ui.horizontal(|ui| {
            ui.label("token URL");
            ui.text_edit_singleline(&mut self.oauth_token_url);
        });
        ui.horizontal(|ui| {
            ui.label("client id");
            ui.text_edit_singleline(&mut self.oauth_client_id);
        });
        ui.horizontal(|ui| {
            ui.label("scopes (공백 구분)");
            ui.text_edit_singleline(&mut self.oauth_scopes);
        });
        let waiting = matches!(self.oauth_status, Some(OAuthStatus::Waiting));
        if ui
            .add_enabled(!waiting, egui::Button::new("브라우저로 연결"))
            .clicked()
        {
            self.start_oauth(ctx);
        }
        match &self.oauth_status {
            None => {}
            Some(OAuthStatus::Waiting) => {
                ui.weak("브라우저에서 승인을 기다리는 중…");
            }
            Some(OAuthStatus::Done(label)) => {
                ui.colored_label(
                    egui::Color32::from_rgb(0x2e, 0xa0, 0x43),
                    format!("● 연결됨 — '{label}' 토큰이 keyring에 저장되었습니다"),
                );
            }
            Some(OAuthStatus::Failed(msg)) => {
                ui.colored_label(egui::Color32::RED, format!("● 실패: {msg}"));
            }
        }
    }

    /// OAuth flow를 백그라운드로 시작한다 (브라우저 승인 대기까지 블로킹이므로).
    fn start_oauth(&mut self, ctx: &egui::Context) {
        let label = self.oauth_label.trim().to_owned();
        if label.is_empty()
            || self.oauth_auth_url.trim().is_empty()
            || self.oauth_token_url.trim().is_empty()
            || self.oauth_client_id.trim().is_empty()
        {
            self.oauth_status = Some(OAuthStatus::Failed(
                "이름·authorize URL·token URL·client id는 필수입니다".to_owned(),
            ));
            return;
        }
        let config = auth::OAuthProviderConfig {
            auth_url: self.oauth_auth_url.trim().to_owned(),
            token_url: self.oauth_token_url.trim().to_owned(),
            client_id: self.oauth_client_id.trim().to_owned(),
            scopes: self
                .oauth_scopes
                .split_whitespace()
                .map(str::to_owned)
                .collect(),
        };
        self.oauth_status = Some(OAuthStatus::Waiting);
        let tx = self.oauth_tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result =
                auth::run_flow(&config, Duration::from_secs(180)).map_err(|e| format!("{e:#}"));
            let _ = tx.send((label, result));
            ctx.request_repaint();
        });
    }

    /// OAuth 결과 반영: keyring 저장 → credentials 등록 → redaction 시드.
    /// 중간 실패 시 keyring 고아 토큰을 지운다. credential을 추가했으면 true.
    fn drain_oauth(&mut self, db: &Db) -> bool {
        let mut added = false;
        while let Ok((label, result)) = self.oauth_rx.try_recv() {
            let token = match result {
                Ok(token) => token,
                Err(msg) => {
                    self.oauth_status = Some(OAuthStatus::Failed(msg));
                    continue;
                }
            };
            let id = uuid::Uuid::new_v4().to_string();
            let store = KeyringSecretStore;
            if let Err(e) = auth::store_token(&store, &id, &token) {
                self.oauth_status = Some(OAuthStatus::Failed(format!("keyring 저장 실패: {e:#}")));
                continue;
            }
            let meta = CredentialMeta {
                id: id.clone(),
                provider: "oauth".to_owned(),
                label: label.clone(),
                credential_kind: "oauth_token".to_owned(),
                masked_hint: Some(secret::masked_hint(token.access_token.expose())),
            };
            if let Err(e) = db.insert_credential(&meta) {
                // 고아 토큰 정리 (access + refresh)
                let _ = store.delete_secret(&id);
                let _ = store.delete_secret(&auth::refresh_entry_id(&id));
                self.oauth_status =
                    Some(OAuthStatus::Failed(format!("credential 등록 실패: {e:#}")));
                continue;
            }
            // 이후 세션 로그에 토큰이 찍히지 않도록 redaction에 등록 (§7)
            self.redaction.register(&token.access_token);
            if let Some(refresh) = &token.refresh_token {
                self.redaction.register(refresh);
            }
            self.oauth_status = Some(OAuthStatus::Done(label));
            added = true;
        }
        added
    }

    fn server_card(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, server: &McpServerRow) {
        ui.group(|ui| {
            ui.horizontal(|ui| {
                ui.strong(&server.name);
                let command = server.command.as_deref().unwrap_or("");
                ui.weak(format!("{} {}", command, server.args.join(" ")));
            });
            ui.horizontal(|ui| {
                match self.status.get(&server.id) {
                    None => ui.weak("미확인"),
                    Some(ConnStatus::Checking) => ui.weak("확인 중…"),
                    Some(ConnStatus::Connected { tools }) => ui.colored_label(
                        egui::Color32::from_rgb(0x2e, 0xa0, 0x43),
                        format!("● 연결됨 — tools {tools}개"),
                    ),
                    Some(ConnStatus::Failed(msg)) => {
                        ui.colored_label(egui::Color32::RED, format!("● 실패: {msg}"))
                    }
                };
                let checking = matches!(self.status.get(&server.id), Some(ConnStatus::Checking));
                if ui
                    .add_enabled(!checking, egui::Button::new("연결 테스트"))
                    .clicked()
                {
                    self.start_discover(ctx, server);
                }
            });
        });
    }

    /// 연결 테스트를 백그라운드로 시작한다 (UI 프레임을 막지 않는다).
    fn start_discover(&mut self, ctx: &egui::Context, server: &McpServerRow) {
        let Some(command) = server.command.clone() else {
            self.status.insert(
                server.id.clone(),
                ConnStatus::Failed("command가 비어 있습니다".to_owned()),
            );
            return;
        };
        self.status.insert(server.id.clone(), ConnStatus::Checking);
        let config = McpServerConfig {
            name: server.name.clone(),
            command,
            args: server.args.clone(),
        };
        let manager = LocalMcpManager::new(self.redaction.clone());
        let tx = self.result_tx.clone();
        let ctx = ctx.clone();
        let server_id = server.id.clone();
        std::thread::spawn(move || {
            let result = manager
                .discover_tools(&config)
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send((server_id, result));
            ctx.request_repaint();
        });
    }

    /// 백그라운드 결과 반영: 상태 갱신 + tools를 DB에 교체 저장 (schema_hash 포함).
    fn drain_results(&mut self, db: &mut Db) {
        while let Ok((server_id, result)) = self.result_rx.try_recv() {
            let status = match result {
                Ok(tools) => {
                    let rows = tool_rows(&server_id, &tools);
                    match db.replace_mcp_tools(&server_id, &rows) {
                        Ok(()) => ConnStatus::Connected { tools: rows.len() },
                        Err(e) => ConnStatus::Failed(format!("tools 저장 실패: {e:#}")),
                    }
                }
                Err(msg) => ConnStatus::Failed(msg),
            };
            self.status.insert(server_id, status);
        }
    }

    fn add_server(&mut self, db: &mut Db) {
        let name = self.name.trim();
        let command = self.command.trim();
        if name.is_empty() || command.is_empty() {
            self.error = Some("이름과 command는 필수입니다".to_owned());
            return;
        }
        let args: Vec<String> = self
            .args_input
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect();
        let row = McpServerRow {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.to_owned(),
            kind: "stdio".to_owned(), // v0는 stdio만 (§1.5)
            command: Some(command.to_owned()),
            args,
            url: None,
            enabled: true,
        };
        match db.insert_mcp_server(&row) {
            Ok(()) => {
                self.name.clear();
                self.command.clear();
                self.args_input.clear();
                self.error = None;
                self.cached = None; // 목록 재조회
            }
            Err(e) => self.error = Some(format!("추가 실패: {e:#}")),
        }
    }
}

/// 발견한 tool을 저장용 행으로 변환한다. schema_hash는 여기서 계산해 기록 —
/// PR-16 재승인 트리거(audit::PermissionPolicy)가 이 해시를 비교한다.
fn tool_rows(server_id: &str, tools: &[McpTool]) -> Vec<McpToolRow> {
    tools
        .iter()
        .map(|tool| McpToolRow {
            id: uuid::Uuid::new_v4().to_string(),
            server_id: server_id.to_owned(),
            name: tool.name.clone(),
            description: tool.description.clone(),
            input_schema_json: Some(tool.input_schema_json.clone()),
            trust_level: "unknown".to_owned(),
            schema_hash: Some(audit::schema_hash(&tool.input_schema_json)),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_rows는_schema_hash를_계산한다() {
        let tools = vec![McpTool {
            name: "read_file".to_owned(),
            description: None,
            input_schema_json: r#"{"type":"object"}"#.to_owned(),
        }];
        let rows = tool_rows("srv-1", &tools);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].schema_hash.as_deref(),
            Some(audit::schema_hash(r#"{"type":"object"}"#).as_str())
        );
        assert_eq!(rows[0].server_id, "srv-1");
        assert_eq!(rows[0].trust_level, "unknown");
    }
}
