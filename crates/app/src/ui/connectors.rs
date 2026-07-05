//! Connector Center (설계문서 §3 ConnectorCenter, PR-17/PR-18).
//! local MCP 서버를 카드로 나열하고 쉽게 추가 + 연결 상태를 표시한다.
//! 연결 테스트(discover_tools)는 subprocess 왕복이라 백그라운드 스레드에서 돌리고,
//! 결과는 채널로 받아 UI에 반영 + mcp_tools를 DB에 교체 저장한다.
//! OAuth 커넥터(PR-18): 브라우저 승인 대기가 길어 flow 전체를 백그라운드로 돌리고,
//! 획득한 토큰은 UI 스레드에서 keyring 저장 + credentials 등록 + redaction 시드.

use std::collections::HashMap;
use std::sync::mpsc;
use std::time::Duration;

use mcp::{LocalMcpManager, McpServerConfig, McpTool};
use mcp_store::{McpServerRow, McpToolRow};
use secret::RedactionService;

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

/// 진행 중인 도구 실행 (한 번에 하나). 정책 평가 → (필요 시) 승인 → tools/call → 감사.
struct ToolInvoke {
    server_id: String,
    server_name: String,
    command: String,
    args: Vec<String>,
    tool_name: String,
    schema_hash: String,
    /// tool 인자 JSON draft (사용자 편집)
    input: String,
    phase: InvokePhase,
    /// 실행 세대 — 백그라운드 결과가 이 값과 일치할 때만 반영(stale 결과 무시)
    generation: u64,
    /// Preparing이 가져온 현재 schema hash (패널이 소비해 정책 평가에 반영)
    prepared_hash: Option<String>,
}

enum InvokePhase {
    Editing,
    /// 스키마 재확인 중 (서버에서 현재 schema를 다시 가져와 재승인 판정)
    Preparing,
    Approval(audit::ApprovalReason),
    Running,
    Done(String),
    Failed(String),
}

/// 백그라운드 메시지.
enum InvokeMsg {
    /// 스키마 재확인 완료 — 현재(호출 시점) schema hash. 재승인 판정에 이걸 쓴다.
    Prepared(String),
    /// tools/call 결과 (성공 pretty JSON | 실패 에러)
    Result(Result<String, String>),
}

/// 백그라운드 결과: (실행 세대, 메시지). 세대가 일치할 때만 반영.
type InvokeResult = (u64, InvokeMsg);

/// tool 인자 JSON 최대 크기 — stdin pipe buffer(대체로 ≥64KB)보다 작게 잡아
/// write_all이 서버 미독취 시에도 블록되지 않게 한다 (transport write hang 방지).
const MAX_TOOL_INPUT: usize = 32 * 1024;

pub struct StoredOAuthCredential {
    pub id: String,
    pub masked_hint: String,
}

pub trait OAuthCredentialStore {
    fn store_oauth_token(&self, token: &auth::OAuthToken) -> anyhow::Result<StoredOAuthCredential>;
    fn delete_oauth_token(&self, id: &str) -> anyhow::Result<()>;
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
    // 도구 실행/승인 (PR-16): 규칙은 인메모리(세션 범위 — 재시작 시 초기화)
    policy: audit::PermissionPolicy,
    invoke: Option<ToolInvoke>,
    invoke_tx: mpsc::Sender<InvokeResult>,
    invoke_rx: mpsc::Receiver<InvokeResult>,
    invoke_gen: u64,
    /// 저장된 권한 규칙을 policy로 1회 로드했는지 (contents 최초 진입 시)
    rules_loaded: bool,
}

impl ConnectorsUi {
    pub fn new(redaction: RedactionService) -> Self {
        let (result_tx, result_rx) = mpsc::channel();
        let (oauth_tx, oauth_rx) = mpsc::channel();
        let (invoke_tx, invoke_rx) = mpsc::channel();
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
            policy: audit::PermissionPolicy::new(),
            invoke: None,
            invoke_tx,
            invoke_rx,
            invoke_gen: 0,
            rules_loaded: false,
        }
    }

    pub fn toggle(&mut self) {
        self.open = !self.open;
        self.error = None;
    }

    /// 진행 중인 도구 실행 상태를 비운다 (workspace 전환 시 — A에서 연 invoke가 B에서
    /// 실행/감사되지 않도록). 백그라운드 스레드는 계속 돌지만 결과는 세대 불일치로 무시된다.
    pub fn clear_invoke(&mut self) {
        self.invoke = None;
    }

    /// 새 credential이 등록됐으면 true (호출측이 자격증명 창 캐시를 무효화).
    /// raw/encrypted audit input 보존은 명시 opt-in 전까지 기본 비활성이다.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        db: &mut Db,
        workspace_id: &str,
        oauth_store: &dyn OAuthCredentialStore,
    ) -> bool {
        // 백그라운드 결과는 창이 닫혀 있어도 소화한다 (다시 열 때 최신 상태)
        self.drain_results(db);
        self.drain_invoke();
        let credential_added = self.drain_oauth(db, oauth_store);
        if !self.open {
            return credential_added;
        }
        let mut open = true;
        egui::Window::new("연결")
            .open(&mut open)
            .resizable(false)
            .show(ctx, |ui| self.contents(ui, ctx, db, workspace_id));
        self.open = open;
        credential_added
    }

    /// 백그라운드 tools/call 결과를 현재 invoke 상태에 반영.
    fn drain_invoke(&mut self) {
        while let Ok((generation, msg)) = self.invoke_rx.try_recv() {
            // 세대 일치할 때만 반영 — 다른 tool을 새로 시작했으면 이전 백그라운드
            // 결과는 무시한다 (stale 결과 race). Prepared는 패널이 정책 평가에 소비한다.
            if let Some(inv) = &mut self.invoke
                && inv.generation == generation
            {
                match msg {
                    InvokeMsg::Prepared(hash) => inv.prepared_hash = Some(hash),
                    InvokeMsg::Result(Ok(output)) => inv.phase = InvokePhase::Done(output),
                    InvokeMsg::Result(Err(err)) => inv.phase = InvokePhase::Failed(err),
                }
            }
        }
    }

    /// 저장된 권한 규칙을 PermissionPolicy로 로드 (재시작해도 Allow/Deny always 유지).
    fn load_permission_rules(&mut self, db: &Db) {
        match db.list_permission_rules() {
            Ok(rows) => {
                for row in rows {
                    if let Some(rule) = audit::PermissionRule::from_persisted(&row.rule) {
                        self.policy.load_rule(
                            &row.server_id,
                            &row.tool_name,
                            rule,
                            row.approved_schema_hash,
                        );
                    }
                }
            }
            Err(e) => tracing::warn!("권한 규칙 로드 실패: {e:#}"),
        }
    }

    fn contents(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        db: &mut Db,
        workspace_id: &str,
    ) {
        if !self.rules_loaded {
            self.load_permission_rules(db);
            self.rules_loaded = true;
        }
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
            self.server_card(ui, ctx, db, server);
        }

        // 도구 실행 패널 (선택된 tool이 있을 때) — 정책 평가·승인·실행·감사
        if self.invoke.is_some() {
            self.tool_invoke_panel(ui, ctx, db, workspace_id);
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
    fn drain_oauth(&mut self, db: &Db, oauth_store: &dyn OAuthCredentialStore) -> bool {
        let mut added = false;
        while let Ok((label, result)) = self.oauth_rx.try_recv() {
            let token = match result {
                Ok(token) => token,
                Err(msg) => {
                    self.oauth_status = Some(OAuthStatus::Failed(msg));
                    continue;
                }
            };
            let stored = match oauth_store.store_oauth_token(&token) {
                Ok(stored) => stored,
                Err(e) => {
                    self.oauth_status =
                        Some(OAuthStatus::Failed(format!("keyring 저장 실패: {e:#}")));
                    continue;
                }
            };
            let meta = CredentialMeta {
                id: stored.id.clone(),
                provider: "oauth".to_owned(),
                label: label.clone(),
                credential_kind: "oauth_token".to_owned(),
                masked_hint: Some(stored.masked_hint),
            };
            if let Err(e) = db.insert_credential(&meta) {
                // 고아 토큰 정리 (access + refresh)
                if let Err(rollback) = oauth_store.delete_oauth_token(&stored.id) {
                    tracing::warn!("OAuth token rollback 실패: {rollback:#}");
                }
                self.oauth_status =
                    Some(OAuthStatus::Failed(format!("credential 등록 실패: {e:#}")));
                continue;
            }
            self.oauth_status = Some(OAuthStatus::Done(label));
            added = true;
        }
        added
    }

    fn server_card(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        db: &mut Db,
        server: &McpServerRow,
    ) {
        ui.group(|ui| {
            ui.horizontal(|ui| {
                ui.strong(&server.name);
                let command = server.command.as_deref().unwrap_or("");
                ui.weak(format!(
                    "{} {}",
                    command,
                    mcp_args_for_display(&server.args)
                ));
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
            // 저장된 tool 목록 + 실행 버튼 + 현재 권한 규칙 (PR-16)
            if let Ok(tools) = db.list_mcp_tools(&server.id) {
                for tool in tools {
                    ui.horizontal(|ui| {
                        ui.monospace(&tool.name);
                        // 실행 중인 invoke가 있으면 새로 시작 금지 (동시 실행/덮어쓰기 방지)
                        if ui
                            .add_enabled(self.invoke.is_none(), egui::Button::new("실행").small())
                            .clicked()
                        {
                            self.begin_invoke(server, &tool);
                        }
                        // 현재 규칙 표시 + Ask 아니면 해제 버튼 (잘못 always한 것 되돌리기)
                        let rule = self.policy.rule(&server.id, &tool.name);
                        match rule {
                            audit::PermissionRule::Allow => {
                                ui.colored_label(
                                    egui::Color32::from_rgb(0x2e, 0xa0, 0x43),
                                    "규칙: 항상 허용",
                                );
                            }
                            audit::PermissionRule::Deny => {
                                ui.colored_label(egui::Color32::RED, "규칙: 항상 거부");
                            }
                            audit::PermissionRule::Ask => {}
                        }
                        if rule != audit::PermissionRule::Ask
                            && ui.small_button("규칙 해제").clicked()
                        {
                            self.policy.set_rule(
                                &server.id,
                                &tool.name,
                                audit::PermissionRule::Ask,
                            );
                            if let Err(e) = db.delete_permission_rule(&server.id, &tool.name) {
                                tracing::warn!("권한 규칙 삭제 실패: {e:#}");
                            }
                        }
                    });
                }
            }
        });
    }

    /// 도구 실행 시작 — Editing 상태로 invoke 패널을 연다.
    fn begin_invoke(&mut self, server: &McpServerRow, tool: &McpToolRow) {
        self.invoke_gen += 1;
        // schema_hash는 저장분 우선, 없으면 스키마에서 재계산 (재승인 판정용)
        let schema_hash = tool.schema_hash.clone().unwrap_or_else(|| {
            audit::schema_hash(tool.input_schema_json.as_deref().unwrap_or("{}"))
        });
        self.invoke = Some(ToolInvoke {
            server_id: server.id.clone(),
            server_name: server.name.clone(),
            command: server.command.clone().unwrap_or_default(),
            args: server.args.clone(),
            tool_name: tool.name.clone(),
            schema_hash,
            input: "{}".to_owned(),
            phase: InvokePhase::Editing,
            generation: self.invoke_gen,
            prepared_hash: None,
        });
    }

    /// 도구 실행 패널: 편집 → 정책 평가 → (승인) → tools/call → 결과. 감사는 결정 시점에.
    fn tool_invoke_panel(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        db: &mut Db,
        workspace_id: &str,
    ) {
        ui.separator();
        let Some(mut inv) = self.invoke.take() else {
            return;
        };
        // 스키마 재확인이 끝났으면(현재 hash 확보) 그걸로 정책을 평가한다 —
        // 저장된 stale hash가 아니라 호출 시점 schema로 재승인 판정 (codex 리뷰 P1).
        if matches!(inv.phase, InvokePhase::Preparing)
            && let Some(hash) = inv.prepared_hash.take()
        {
            inv.schema_hash = hash;
            inv.phase = match parse_tool_arguments(&inv.input) {
                Err(message) => InvokePhase::Failed(message),
                Ok(_) => {
                    let request = request_of(&inv);
                    match self.policy.evaluate(&request) {
                        audit::PolicyEvaluation::Decided(decision) => {
                            self.run_tool(&inv, db, workspace_id, ctx, decision)
                        }
                        audit::PolicyEvaluation::NeedsApproval(reason) => {
                            InvokePhase::Approval(reason)
                        }
                    }
                }
            };
        }
        enum Act {
            None,
            Submit,
            Decide(audit::ToolDecision),
            Close,
        }
        let mut act = Act::None;
        ui.group(|ui| {
            ui.strong(format!(
                "도구 실행 — {} · {}",
                inv.server_name, inv.tool_name
            ));
            match &inv.phase {
                InvokePhase::Editing => {
                    ui.label("인자 (JSON object)");
                    ui.add(
                        egui::TextEdit::multiline(&mut inv.input)
                            .code_editor()
                            .desired_rows(3),
                    );
                    ui.horizontal(|ui| {
                        if ui.button("호출").clicked() {
                            act = Act::Submit;
                        }
                        if ui.button("취소").clicked() {
                            act = Act::Close;
                        }
                    });
                }
                InvokePhase::Preparing => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("스키마 확인 중…");
                    });
                }
                InvokePhase::Approval(reason) => {
                    let why = match reason {
                        audit::ApprovalReason::AskRule => "정책: 매번 확인",
                        audit::ApprovalReason::FirstUse => "첫 사용 — 승인 필요",
                        audit::ApprovalReason::SchemaChanged => "스키마 변경 — 재승인 필요",
                    };
                    ui.colored_label(
                        egui::Color32::from_rgb(0xd0, 0x8a, 0x00),
                        format!("승인 필요: {why}"),
                    );
                    ui.horizontal(|ui| {
                        if ui.button("이번만 허용").clicked() {
                            act = Act::Decide(audit::ToolDecision::AllowOnce);
                        }
                        if ui.button("항상 허용").clicked() {
                            act = Act::Decide(audit::ToolDecision::AllowAlways);
                        }
                        if ui.button("이번만 거부").clicked() {
                            act = Act::Decide(audit::ToolDecision::DenyOnce);
                        }
                        if ui.button("항상 거부").clicked() {
                            act = Act::Decide(audit::ToolDecision::DenyAlways);
                        }
                    });
                }
                InvokePhase::Running => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("실행 중…");
                    });
                }
                InvokePhase::Done(output) => {
                    ui.label("결과");
                    let mut shown = output.clone();
                    ui.add(
                        egui::TextEdit::multiline(&mut shown)
                            .code_editor()
                            .desired_rows(6)
                            .interactive(false),
                    );
                    if ui.button("닫기").clicked() {
                        act = Act::Close;
                    }
                }
                InvokePhase::Failed(msg) => {
                    ui.colored_label(egui::Color32::RED, msg.clone());
                    if ui.button("닫기").clicked() {
                        act = Act::Close;
                    }
                }
            }
        });

        // 액션은 closure 밖에서 처리 (self를 자유롭게 빌림)
        match act {
            Act::None => self.invoke = Some(inv),
            Act::Close => {} // inv drop
            Act::Submit => {
                if let Err(message) = parse_tool_arguments(&inv.input) {
                    inv.phase = InvokePhase::Failed(message);
                    self.invoke = Some(inv);
                    return;
                }
                let request = request_of(&inv);
                // Deny 규칙은 스키마와 무관 — prepare(서버 spawn) 없이 즉시 거부+감사.
                // 그 외(Allow/Ask)는 stale hash로 우회되지 않도록 현재 스키마를 재확인한다.
                inv.phase = if let audit::PolicyEvaluation::Decided(
                    decision @ audit::ToolDecision::PolicyDeny,
                ) = self.policy.evaluate(&request)
                {
                    self.run_tool(&inv, db, workspace_id, ctx, decision)
                } else {
                    self.start_prepare(&inv, ctx);
                    inv.prepared_hash = None;
                    InvokePhase::Preparing
                };
                self.invoke = Some(inv);
            }
            Act::Decide(decision) => {
                if let Err(message) = parse_tool_arguments(&inv.input) {
                    inv.phase = InvokePhase::Failed(message);
                    self.invoke = Some(inv);
                    return;
                }
                let request = request_of(&inv);
                self.policy.apply_decision(&request, decision);
                // Always 계열은 규칙이 바뀌므로 영속한다 (재시작해도 유지)
                if matches!(
                    decision,
                    audit::ToolDecision::AllowAlways | audit::ToolDecision::DenyAlways
                ) {
                    let rule = self.policy.rule(&inv.server_id, &inv.tool_name);
                    let hash = self.policy.approved_hash(&inv.server_id, &inv.tool_name);
                    if let Err(e) = db.upsert_permission_rule(
                        &inv.server_id,
                        &inv.tool_name,
                        rule.as_str(),
                        hash,
                    ) {
                        tracing::warn!("권한 규칙 저장 실패: {e:#}");
                    }
                }
                inv.phase = self.run_tool(&inv, db, workspace_id, ctx, decision);
                self.invoke = Some(inv);
            }
        }
    }

    /// 현재 tool 스키마를 서버에서 다시 가져와 schema hash를 확보한다 (백그라운드).
    /// 저장된 stale hash로 재승인을 우회하지 않도록 호출 직전에 재확인한다.
    fn start_prepare(&self, inv: &ToolInvoke, ctx: &egui::Context) {
        let config = McpServerConfig {
            name: inv.server_name.clone(),
            command: inv.command.clone(),
            args: inv.args.clone(),
        };
        let manager = LocalMcpManager::new(self.redaction.clone());
        let tx = self.invoke_tx.clone();
        let tool_name = inv.tool_name.clone();
        let generation = inv.generation;
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let msg = match manager.discover_tools(&config) {
                Ok(tools) => match tools.iter().find(|t| t.name == tool_name) {
                    Some(tool) => InvokeMsg::Prepared(audit::schema_hash(&tool.input_schema_json)),
                    None => InvokeMsg::Result(Err(format!("tool '{tool_name}'이 서버에 없습니다"))),
                },
                Err(e) => InvokeMsg::Result(Err(format!("스키마 확인 실패: {e:#}"))),
            };
            let _ = tx.send((generation, msg));
            ctx.request_repaint();
        });
    }

    /// 감사 기록(redacted only; encrypted raw input 기본 비활성) 후, 허용이면
    /// 백그라운드로 tools/call 실행.
    /// 반환은 다음 phase (Running 또는 Failed).
    fn run_tool(
        &self,
        inv: &ToolInvoke,
        db: &Db,
        workspace_id: &str,
        ctx: &egui::Context,
        decision: audit::ToolDecision,
    ) -> InvokePhase {
        let arguments = match parse_tool_arguments(&inv.input) {
            Ok(arguments) => arguments,
            Err(message) => return InvokePhase::Failed(message),
        };
        // 감사: 기본 경로는 redacted JSON만 저장하고 encrypted raw blob은 NULL로 둔다.
        // raw/encrypted input 보존은 명시 opt-in plumbing이 생긴 뒤에만 Some(encryptor)를 넘긴다.
        let record = audit::AuditRecord {
            workspace_id: Some(workspace_id),
            session_id: None,
            server_id: Some(&inv.server_id),
            tool_name: &inv.tool_name,
            input_json: &inv.input,
            decision,
        };
        if let Err(e) = db.record_tool_audit(&record, &self.redaction, None) {
            tracing::warn!("tool 감사 기록 실패: {e:#}");
        }
        if !decision.is_allowed() {
            return InvokePhase::Failed("정책상 거부됨".to_owned());
        }
        let config = McpServerConfig {
            name: inv.server_name.clone(),
            command: inv.command.clone(),
            args: inv.args.clone(),
        };
        let manager = LocalMcpManager::new(self.redaction.clone());
        let redaction = self.redaction.clone();
        let tx = self.invoke_tx.clone();
        let tool_name = inv.tool_name.clone();
        let generation = inv.generation;
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            // 결과/에러 문자열은 표시 전에 등록된 secret을 마스킹한다 (§7 유출 방지).
            // 성공 결과와 에러 메시지(MCP 서버가 secret을 echo할 수 있음) 둘 다 대상.
            let result = match manager.call_tool(&config, &tool_name, arguments) {
                Ok(value) => Ok(redact_display(
                    &redaction,
                    &serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string()),
                )),
                Err(e) => Err(redact_display(&redaction, &format!("{e:#}"))),
            };
            let _ = tx.send((generation, InvokeMsg::Result(result)));
            ctx.request_repaint();
        });
        InvokePhase::Running
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
        if let Err(e) = mcp_store::validate_server_args_for_persistence(&args) {
            self.error = Some(format!("추가 실패: {e:#}"));
            return;
        }
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

fn mcp_args_for_display(args: &[String]) -> String {
    if mcp_store::validate_server_args_for_persistence(args).is_err() {
        "[REDACTED_ARGS]".to_owned()
    } else {
        args.join(" ")
    }
}

/// 발견한 tool을 저장용 행으로 변환한다. schema_hash는 여기서 계산해 기록 —
/// PR-16 재승인 트리거(audit::PermissionPolicy)가 이 해시를 비교한다.
/// 등록된 secret을 마스킹한다 (도구 결과/에러 표시 전 — 감사 로그와 동일 방어선).
fn redact_display(redaction: &RedactionService, text: &str) -> String {
    let mut redactor = redaction.stream_redactor();
    let mut out = redactor.redact_chunk(text.as_bytes());
    out.extend(redactor.flush());
    String::from_utf8_lossy(&out).into_owned()
}

/// invoke 상태에서 정책 평가용 요청 model을 만든다.
fn request_of(inv: &ToolInvoke) -> audit::ToolApprovalRequest {
    audit::ToolApprovalRequest {
        server_id: inv.server_id.clone(),
        tool_name: inv.tool_name.clone(),
        input_json: inv.input.clone(),
        schema_hash: inv.schema_hash.clone(),
    }
}

fn parse_tool_arguments(input: &str) -> Result<serde_json::Value, String> {
    // 인자 크기 상한 — 큰 JSON이 stdin pipe buffer를 채우면 write_all이 영구 블록돼
    // UI가 Running에 갇힌다. policy/approval/audit 전에 막아 DB/crypto 자원도 쓰지 않는다.
    if input.len() > MAX_TOOL_INPUT {
        return Err(format!(
            "인자가 너무 큽니다 (최대 {}KB)",
            MAX_TOOL_INPUT / 1024
        ));
    }
    match serde_json::from_str(input) {
        Ok(value @ serde_json::Value::Object(_)) => Ok(value),
        Ok(_) => Err("인자는 JSON object여야 합니다".to_owned()),
        Err(e) => Err(format!("인자 JSON 파싱 실패: {e}")),
    }
}

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
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_db_path() -> PathBuf {
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("deppy-connectors-test-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("metadata.sqlite3")
    }

    fn audit_rows(path: &Path) -> Vec<(String, Option<Vec<u8>>)> {
        let conn = rusqlite::Connection::open(path).unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT input_redacted_json, input_encrypted_blob
                 FROM tool_audit_logs ORDER BY created_at, id",
            )
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    fn invoke_with_input(input: &str) -> ToolInvoke {
        ToolInvoke {
            server_id: "srv-1".to_owned(),
            server_name: "mock".to_owned(),
            command: "/nonexistent/deppy-connectors-test".to_owned(),
            args: Vec::new(),
            tool_name: "read_file".to_owned(),
            schema_hash: audit::schema_hash(r#"{"type":"object"}"#),
            input: input.to_owned(),
            phase: InvokePhase::Editing,
            generation: 1,
            prepared_hash: None,
        }
    }

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

    #[test]
    fn mcp_args_display는_secret_like_payload를_숨긴다() {
        let rendered = mcp_args_for_display(&[
            "-H".to_owned(),
            "Authorization: Bearer sk-ui-mcp-secret-never-rendered".to_owned(),
        ]);
        assert_eq!(rendered, "[REDACTED_ARGS]");
        assert!(!rendered.contains("sk-ui-mcp-secret"));
        assert_eq!(
            mcp_args_for_display(&["-y".to_owned(), "server-filesystem".to_owned()]),
            "-y server-filesystem"
        );
    }

    #[test]
    fn tool_arguments는_json_object만_허용한다() {
        assert!(parse_tool_arguments(r#"{"path":"/tmp/x"}"#).is_ok());
        assert!(
            parse_tool_arguments("{bad")
                .unwrap_err()
                .contains("파싱 실패")
        );
        assert_eq!(
            parse_tool_arguments("[1,2]").unwrap_err(),
            "인자는 JSON object여야 합니다"
        );
    }

    #[test]
    fn connector_audit_기본값은_redacted_only_blob_null() {
        let path = temp_db_path();
        let db = Db::open(&path).unwrap();
        let ui = ConnectorsUi::new(RedactionService::new());
        let ctx = egui::Context::default();
        let inv = invoke_with_input(r#"{"token":"sk-unregistered-secret","path":"/tmp/x"}"#);

        let phase = ui.run_tool(&inv, &db, "ws-1", &ctx, audit::ToolDecision::DenyOnce);

        assert!(matches!(phase, InvokePhase::Failed(_)));
        let rows = audit_rows(&path);
        assert_eq!(rows.len(), 1);
        assert!(
            !rows[0].0.contains("sk-unregistered-secret"),
            "{}",
            rows[0].0
        );
        assert!(rows[0].0.contains("[REDACTED]"), "{}", rows[0].0);
        assert!(rows[0].1.is_none(), "encrypted blob must be default-off");
    }

    #[test]
    fn connector_invalid_input은_audit_없이_local_error() {
        let path = temp_db_path();
        let db = Db::open(&path).unwrap();
        let ui = ConnectorsUi::new(RedactionService::new());
        let ctx = egui::Context::default();

        for input in ["{bad", "[1,2]"] {
            let inv = invoke_with_input(input);
            let phase = ui.run_tool(&inv, &db, "ws-1", &ctx, audit::ToolDecision::DenyOnce);
            assert!(matches!(phase, InvokePhase::Failed(_)));
        }

        assert!(audit_rows(&path).is_empty());
    }
}
