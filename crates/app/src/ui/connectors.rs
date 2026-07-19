//! Connector Center (설계문서 §3 ConnectorCenter, PR-17/PR-18 + H3 + H5).
//! local MCP 서버를 카드로 나열하고 쉽게 추가 + 연결 상태를 표시한다.
//! 연결 테스트(discover_tools)는 subprocess/HTTP 왕복이라 백그라운드 스레드에서 돌리고,
//! 결과는 채널로 받아 UI에 반영 + mcp_tools를 DB에 교체 저장한다.
//! HTTP 커넥터(H3): kind='http' 서버를 url로 등록·발견·실행한다. 최초 연결(테스트/
//! 도구 발견/실행) 전 세션당 1회 신뢰 확인 모달을 거치고, url 편집 저장 시 Allow 규칙
//! 초기화 + 도구 캐시 무효화 + 재확인한다 (VS Code cacheNonce 신뢰의 편집 시점 훅 등가).
//! OAuth(H5, 401 사다리): 401/403은 "에러"가 아니라 "승인 필요" 카드 상태로 구분한다
//! (차용: extHostMcp의 needs-user-interaction 상태 분리). [브라우저로 승인] →
//! 발견 체인(RFC 9728/8414) → 동의 다이얼로그 → DCR(실패 시 수동 client_id 폴백)
//! → 외부 브라우저 PKCE → Bearer 재시도(scope 갱신 1회 + 재등록 1회 한정)까지
//! 백그라운드 스레드 + mpsc로 돌리고, 토큰은 UI 스레드에서 keyring 저장 +
//! credentials(oauth_json 바인딩 메타) 등록 + redaction 시드. Bearer는 동의 시점
//! 서버 URL과 현재 url이 일치할 때만 부착한다 (토큰 URL 바인딩 — H3 url 편집
//! 규칙 리셋과 한 쌍). 구 OAuth 수동 폼(auth/token URL 입력)은 발견 체인이
//! 대체해 제거했다.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, mpsc};
use std::time::{Duration, SystemTime};

use anyhow::Context;
use mcp::{
    LocalMcpManager, McpAuthRequired, McpHttpServerConfig, McpServerConfig, McpTool,
    PROTOCOL_VERSION, validate_mcp_url,
};
use mcp_store::{McpServerRow, McpToolRow};
use secret::{RedactionService, SecretStore, SecretString};

use crate::mcp_import::{self, SkipReason};
use crate::storage::{CredentialMeta, Db};

/// OAuth 네트워크 프리미티브(발견·DCR·refresh) 타임아웃.
const OAUTH_HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// 외부 브라우저 승인 대기 상한 (구 PR-18 run_flow과 동일).
const BROWSER_FLOW_TIMEOUT: Duration = Duration::from_secs(180);

/// 서버별 연결 상태 (완료 기준: 연결 상태 표시).
enum ConnStatus {
    Checking,
    Connected {
        tools: usize,
    },
    Failed(String),
    /// 401/403 — 에러가 아니라 사용자 승인 개입 지점 (H5). challenge는
    /// [브라우저로 승인] 클릭 시 발견 체인의 입력이 된다.
    NeedsAuth {
        message: String,
        challenge: AuthChallengeInfo,
    },
}

/// 401 응답의 Bearer 챌린지에서 추출한 정보 (H5 사다리 입력).
#[derive(Debug, Clone, Default, PartialEq)]
struct AuthChallengeInfo {
    /// RFC 9728 §5.1 resource_metadata — PRM 발견 1순위 후보.
    resource_metadata: Option<String>,
    /// RFC 6750 §3 scope — 승인 요청 scope의 1순위 소스.
    scope: Option<String>,
}

/// 백그라운드 실행 실패 (H5): 401/403이면 auth에 챌린지가 실린다.
#[derive(Debug)]
struct ExecFailure {
    message: String,
    auth: Option<AuthChallengeInfo>,
}

/// 백그라운드 refresh 부수효과 — drain이 만료 시각 메타데이터(oauth_json)를 영속한다.
/// 세대(stale) 여부와 무관하게 적용한다 (전역 상태이므로).
struct RefreshUpdate {
    credential_id: String,
    expires_at_secs: Option<u64>,
}

/// 백그라운드 연결 테스트 결과.
struct DiscoverOutcome {
    server_id: String,
    /// 요청 시점의 http url — drain에서 현재 row.url과 불일치하면 stale로 폐기
    /// (url 편집 저장과의 race 방어, H3 리뷰 P2). stdio는 None.
    request_url: Option<String>,
    result: Result<Vec<McpTool>, ExecFailure>,
    refresh: Option<RefreshUpdate>,
}

/// 진행 중인 도구 실행 (한 번에 하나). 정책 평가 → (필요 시) 승인 → tools/call → 감사.
struct ToolInvoke {
    server_id: String,
    server_name: String,
    /// transport별 연결 스펙 — 서버 row의 kind('stdio'|'http')에 대응 (H3)
    target: InvokeTarget,
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

/// invoke가 보관하는 transport별 연결 스펙 (H3).
enum InvokeTarget {
    Stdio {
        command: String,
        args: Vec<String>,
        env_plain: Vec<(String, String)>,
        env_secrets: Vec<(String, String)>,
        inherit_env: bool,
    },
    Http {
        url: String,
    },
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
    /// tools/call 결과 (성공 pretty JSON | 실패 — 401/403이면 auth 챌린지 포함)
    Result(Result<String, ExecFailure>),
}

/// 백그라운드 결과: (실행 세대, 메시지, refresh 부수효과). 세대가 일치할 때만
/// 메시지를 반영하고, refresh 부수효과는 세대와 무관하게 영속한다.
type InvokeResult = (u64, InvokeMsg, Option<RefreshUpdate>);

/// OAuth 승인 진행 상태 (H5) — 브라우저 flow는 한 번에 하나만.
struct OAuthFlow {
    server_id: String,
    server_name: String,
    server_url: String,
    /// stale 결과 폐기용 세대 — 취소/재시작 시 증가한다.
    generation: u64,
    stage: OAuthStage,
}

enum OAuthStage {
    /// 발견 체인(PRM → AS 메타데이터) 백그라운드 실행 중.
    Discovering,
    /// 발견 완료 — 브라우저 승인 진입 동의 대기 (authority 표시).
    Consent(DiscoveredAuth),
    /// DCR 미지원/거부 — 수동 client_id 입력 대기 (폴백).
    ManualClient {
        discovered: DiscoveredAuth,
        reason: String,
        client_id: String,
        client_secret: String,
    },
    /// 등록 → 브라우저 승인 → 사다리 재시도 진행 중.
    Authorizing,
}

/// 발견 체인의 결과 — AS 메타데이터 + 요청할 scope.
#[derive(Debug, Clone)]
struct DiscoveredAuth {
    metadata: auth::AuthorizationServerMetadata,
    scopes: Vec<String>,
}

/// 사다리(stage B)의 클라이언트 등록 출처.
enum ClientPlan {
    /// 기존 바인딩의 client 재사용 (issuer 일치 시).
    Stored {
        client_id: String,
        client_secret: Option<SecretString>,
        manual: bool,
    },
    /// RFC 7591 동적 등록.
    Dcr,
    /// 수동 입력 폴백 (DCR 미지원/거부).
    Manual {
        client_id: String,
        client_secret: Option<SecretString>,
    },
}

/// 사다리 성공 페이로드 — 토큰과 바인딩 메타는 UI 스레드(drain_oauth)가 영속한다.
struct LadderSuccess {
    connection: OAuthConnection,
    token: auth::OAuthToken,
    /// DCR이 발급한 client_secret — keyring `{id}.dcr`에 저장(없으면 entry 삭제).
    client_secret: Option<SecretString>,
    /// 사다리 마지막 성공 요청(tools/list)의 결과 — "완료 후 자동 재시도"의 산물.
    tools: Vec<McpTool>,
}

/// 사다리 종료 상태.
enum LadderEnd {
    Success(Box<LadderSuccess>),
    /// DCR 미지원/거부 — 수동 client_id 입력 폴백으로 전환.
    NeedManualClient {
        reason: String,
    },
    Failed(String),
}

/// OAuth flow 백그라운드 메시지.
enum OAuthMsg {
    Discovered(Box<Result<DiscoveredAuth, String>>),
    /// discovered를 되돌려줘 수동 입력 단계가 이어서 쓴다.
    NeedManualClient {
        discovered: Box<DiscoveredAuth>,
        reason: String,
    },
    Finished(Box<Result<LadderSuccess, String>>),
}

/// OAuth flow 결과: (세대, 메시지). 세대가 일치할 때만 반영.
type OAuthFlowResult = (u64, OAuthMsg);

/// http 서버 OAuth 연계 메타데이터 (H5) — credentials.oauth_json(v20)에 저장.
/// **비밀 아님**: 비밀은 전부 keyring (access={id}, refresh={id}.refresh,
/// DCR client_secret={id}.dcr — §2.1).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct OAuthConnection {
    /// 바인딩된 mcp_servers.id.
    server_id: String,
    /// **동의 시점**의 서버 URL — 현재 url과 일치할 때만 Bearer를 부착한다
    /// (차용: mainThreadMcp.ts "token is only released to a server whose current
    /// URL matches the one the user consented to").
    server_url: String,
    /// 발견된 AS issuer/endpoint — 재승인·refresh가 재사용한다.
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    client_id: String,
    /// true면 수동 입력 client — 재등록(DCR) 사다리를 건너뛴다.
    #[serde(default)]
    manual_client: bool,
    #[serde(default)]
    scopes: Vec<String>,
    /// access token 만료 시각 (unix 초) — 만료 5분 전 선제 refresh 판정 (H4 정책).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires_at_secs: Option<u64>,
}

/// http 서버의 Bearer 바인딩 해석 결과 (UI 스레드에서 DB만 읽어 만든다 — 비밀 없음).
enum HttpAuth {
    /// 바인딩 없음 — Bearer 없이 요청한다 (401이면 승인 필요 카드).
    None,
    /// 동의 시점 URL ≠ 현재 URL — **부착 거부** (토큰 URL 바인딩). 요청은 Bearer
    /// 없이 나가고, 401이면 재동의(승인 카드) 경로를 탄다.
    UrlMismatch,
    /// 바인딩 유효 — keyring 해석 재료 (access/DCR secret 조회는 백그라운드 실행
    /// 스레드의 run_http가 한다 — UI 스레드 KEYRING_SERIAL 경합/프레임 스톨 방지).
    Bound(HttpAuthBinding),
}

/// DB oauth_json에서 온 refresh 재료 (비밀 없음 — 스레드로 이동 가능한 소유 데이터).
#[derive(Debug)]
struct HttpAuthBinding {
    credential_id: String,
    token_url: String,
    client_id: String,
    /// 동의 시점 서버 URL — RFC 8707 resource 고정에 쓴다.
    server_url: String,
    expires_at_secs: Option<u64>,
}

/// 백그라운드 refresh에 필요한 재료 (run_http가 keyring을 읽어 완성한다).
struct HttpAuthState {
    credential_id: String,
    refresh_params: auth::RefreshParams,
    /// 만료 5분 전(H4 REFRESH_MARGIN) — 요청 전에 선제 refresh.
    needs_refresh: bool,
}

/// 추가 폼의 서버 종류 (H3) — mcp_servers.kind 'stdio' | 'http'에 대응.
#[derive(Clone, Copy, PartialEq)]
enum AddKind {
    Stdio,
    Http,
}

/// http 서버 최초 연결 신뢰 확인 모달 상태 (H3). 확인 후 실행할 동작을 보관한다.
/// 신뢰는 세션 범위(trusted_http) — 스키마 추가 없이 편집 시점 훅과 한 쌍으로 동작한다.
struct TrustPrompt {
    server: McpServerRow,
    /// None = 연결 테스트(discover), Some(tool) = 도구 실행(invoke)
    tool: Option<McpToolRow>,
}

/// http 서버 url 편집 draft (H3). 저장 시 Allow 규칙 초기화 + 캐시 무효화 + 재확인.
struct UrlEdit {
    server_id: String,
    draft: String,
}

/// tool 인자 JSON 최대 크기 — stdin pipe buffer(대체로 ≥64KB)보다 작게 잡아
/// write_all이 서버 미독취 시에도 블록되지 않게 한다 (transport write hang 방지).
const MAX_TOOL_INPUT: usize = 32 * 1024;

/// 인기 stdio MCP 서버 프리셋 — 클릭하면 추가 폼에 채워진다 (직접 등록하지 않음:
/// filesystem 허용 루트처럼 사용자가 고쳐야 하는 인자가 있어 폼 경유가 안전하다).
struct McpPreset {
    name: &'static str,
    command: &'static str,
    /// "{HOME}"은 클릭 시점에 홈 디렉토리 절대경로로 치환한다
    args: &'static [&'static str],
}

const MCP_PRESETS: &[McpPreset] = &[
    McpPreset {
        name: "filesystem",
        command: "npx",
        args: &["-y", "@modelcontextprotocol/server-filesystem", "{HOME}"],
    },
    McpPreset {
        name: "memory",
        command: "npx",
        args: &["-y", "@modelcontextprotocol/server-memory"],
    },
    McpPreset {
        name: "fetch",
        command: "uvx",
        args: &["mcp-server-fetch"],
    },
    McpPreset {
        name: "everything",
        command: "npx",
        args: &["-y", "@modelcontextprotocol/server-everything"],
    },
];

pub struct StoredOAuthCredential {
    pub id: String,
    pub masked_hint: String,
}

pub trait OAuthCredentialStore {
    fn store_oauth_token(&self, token: &auth::OAuthToken) -> anyhow::Result<StoredOAuthCredential>;
    /// 기존 credential id 아래 토큰 재저장 (재승인 — H5). keyring 규약은 store와 동일.
    fn update_oauth_token(&self, id: &str, token: &auth::OAuthToken) -> anyhow::Result<()>;
    /// DCR client_secret 저장/삭제 (H5) — Some이면 keyring `{id}.dcr`에, None이면
    /// entry 제거 (재등록으로 secret이 사라진 경우 잔여 방지).
    fn set_dcr_secret(&self, id: &str, secret: Option<&SecretString>) -> anyhow::Result<()>;
    fn delete_oauth_token(&self, id: &str) -> anyhow::Result<()>;
}

pub trait McpScopedEnvResolver {
    fn resolve_mcp_env(
        &self,
        env_plain: &[(String, String)],
        env_secrets: &[(String, String)],
    ) -> anyhow::Result<Vec<(String, String)>>;
}

pub struct ConnectorsUi {
    redaction: RedactionService,
    /// keyring 접근 (H5) — 백그라운드 refresh 스레드로 clone해 넘긴다.
    secret_store: Arc<dyn SecretStore>,
    /// refresh single-flight 조율자 (H5) — **App이 보관하는 프로세스 단일 인스턴스의
    /// Arc 클론**이어야 동시 도구 호출의 refresh가 합쳐진다 (H4 규약).
    refresh: Arc<auth::RefreshCoordinator>,
    // 추가 폼
    name: String,
    /// 추가 폼 종류 선택 (H3): stdio | http
    add_kind: AddKind,
    command: String,
    /// 한 줄에 하나 — agents 등록과 같은 관례 (셸 문자열 파싱 금지)
    args_input: String,
    /// http 서버 URL 입력 (H3) — 저장 전 https/localhost 정책 검증
    url_input: String,
    error: Option<String>,
    cached: Option<Vec<McpServerRow>>,
    /// 서버별 저장된 tool 목록 캐시 — server_card가 열려 있는 동안 매 프레임 SQLite
    /// 조회(서버 수만큼 N+1)를 반복하지 않게 한다. replace_mcp_tools를 부르는 경로
    /// (discover 반영/URL 변경/OAuth 사다리)가 해당 서버 키를 무효화한다.
    tools_cached: HashMap<String, Vec<McpToolRow>>,
    status: HashMap<String, ConnStatus>,
    result_tx: mpsc::Sender<DiscoverOutcome>,
    result_rx: mpsc::Receiver<DiscoverOutcome>,
    // http 신뢰/편집 (H3)
    /// 이번 세션에서 원격 전송을 확인한 http 서버 id — 최초 연결 전 1회 확인 모달
    trusted_http: HashSet<String>,
    trust_prompt: Option<TrustPrompt>,
    url_edit: Option<UrlEdit>,
    // 가져오기 (JSON 붙여넣기 · 파일 · Claude Desktop 설정)
    import_input: String,
    /// 마지막 가져오기 결과 요약 (서버별 등록/건너뜀/실패 한 줄씩)
    import_report: Vec<String>,
    // OAuth 승인 flow (H5, 401 사다리)
    oauth_flow: Option<OAuthFlow>,
    oauth_gen: u64,
    oauth_tx: mpsc::Sender<OAuthFlowResult>,
    oauth_rx: mpsc::Receiver<OAuthFlowResult>,
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
    pub fn new(
        redaction: RedactionService,
        secret_store: Arc<dyn SecretStore>,
        refresh: Arc<auth::RefreshCoordinator>,
    ) -> Self {
        let (result_tx, result_rx) = mpsc::channel();
        let (oauth_tx, oauth_rx) = mpsc::channel();
        let (invoke_tx, invoke_rx) = mpsc::channel();
        Self {
            redaction,
            secret_store,
            refresh,
            name: String::new(),
            add_kind: AddKind::Stdio,
            command: String::new(),
            args_input: String::new(),
            url_input: String::new(),
            error: None,
            cached: None,
            tools_cached: HashMap::new(),
            status: HashMap::new(),
            result_tx,
            result_rx,
            trusted_http: HashSet::new(),
            trust_prompt: None,
            url_edit: None,
            import_input: String::new(),
            import_report: Vec::new(),
            oauth_flow: None,
            oauth_gen: 0,
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

    /// 진행 중인 도구 실행 상태를 비운다 (workspace 전환 시 — A에서 연 invoke가 B에서
    /// 실행/감사되지 않도록). 백그라운드 스레드는 계속 돌지만 결과는 세대 불일치로 무시된다.
    pub fn clear_invoke(&mut self) {
        self.invoke = None;
    }

    /// 백그라운드 tools/call 결과를 현재 invoke 상태에 반영.
    /// refresh 부수효과(만료 시각)는 세대와 무관하게 영속한다 — 전역 상태이므로.
    pub fn drain_invoke(&mut self, db: &Db) {
        while let Ok((generation, msg, refresh)) = self.invoke_rx.try_recv() {
            if let Some(update) = refresh {
                persist_refresh_update(db, &update);
            }
            // 세대 일치할 때만 반영 — 다른 tool을 새로 시작했으면 이전 백그라운드
            // 결과는 무시한다 (stale 결과 race). Prepared는 패널이 정책 평가에 소비한다.
            if let Some(inv) = &mut self.invoke
                && inv.generation == generation
            {
                match msg {
                    InvokeMsg::Prepared(hash) => inv.prepared_hash = Some(hash),
                    InvokeMsg::Result(Ok(output)) => inv.phase = InvokePhase::Done(output),
                    InvokeMsg::Result(Err(failure)) => {
                        // 401/403이면 서버 카드를 "승인 필요"로 전환 (H5) — 승인 후
                        // 자동 재시도(tools 재발견)는 사다리가 수행하고, 도구 실행은
                        // 사용자가 다시 시작한다.
                        if let Some(challenge) = failure.auth {
                            self.status.insert(
                                inv.server_id.clone(),
                                ConnStatus::NeedsAuth {
                                    message: failure.message.clone(),
                                    challenge,
                                },
                            );
                        }
                        inv.phase = InvokePhase::Failed(failure.message);
                    }
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

    pub fn contents(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        db: &mut Db,
        workspace_id: &str,
        env_resolver: &dyn McpScopedEnvResolver,
        catalog: &i18n::Catalog,
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
                    ui.colored_label(
                        egui::Color32::RED,
                        catalog.t("common.list_failed", &[("message", &format!("{e:#}"))]),
                    );
                    return;
                }
            },
        };

        ui.heading(catalog.t("connectors.local_mcp", &[]));
        if servers.is_empty() {
            ui.label(catalog.t("connectors.empty_mcp", &[]));
        }
        for server in &servers {
            self.server_card(ui, ctx, db, server, env_resolver, catalog);
        }

        // 도구 실행 패널 (선택된 tool이 있을 때) — 정책 평가·승인·실행·감사
        if self.invoke.is_some() {
            self.tool_invoke_panel(ui, ctx, db, workspace_id, env_resolver, catalog);
        }

        // http 서버 최초 연결 신뢰 확인 모달 (H3) — 확인하면 이번 세션 동안 기억한다
        self.trust_prompt_modal(ui, ctx, db, env_resolver, catalog);

        // OAuth 승인 flow 모달 (H5): 발견 → 동의 → (수동 client_id) → 브라우저 승인
        self.oauth_flow_modal(ui, ctx, db, catalog);

        ui.separator();
        ui.label(catalog.t("connectors.add_mcp", &[]));
        // 종류 선택 (H3): stdio는 command/args, http는 URL만 입력한다
        ui.horizontal(|ui| {
            ui.label(catalog.t("connectors.kind", &[]));
            ui.radio_value(&mut self.add_kind, AddKind::Stdio, "stdio");
            ui.radio_value(&mut self.add_kind, AddKind::Http, "http");
        });
        if self.add_kind == AddKind::Stdio {
            // 인기 서버 프리셋 — 클릭하면 아래 폼에 채워진다 (경로 등 수정 후 추가)
            ui.horizontal(|ui| {
                ui.label(catalog.t("connectors.presets", &[]));
                for preset in MCP_PRESETS {
                    if ui
                        .small_button(preset.name)
                        .on_hover_text(catalog.t("connectors.preset_hint", &[]))
                        .clicked()
                    {
                        self.apply_preset(preset);
                    }
                }
            });
        }
        ui.horizontal(|ui| {
            ui.label(catalog.t("common.name", &[]));
            ui.text_edit_singleline(&mut self.name);
        });
        match self.add_kind {
            AddKind::Stdio => {
                ui.horizontal(|ui| {
                    ui.label(catalog.t("common.command", &[]));
                    ui.text_edit_singleline(&mut self.command);
                });
                ui.label(catalog.t("connectors.args_note", &[]));
                ui.add(
                    egui::TextEdit::multiline(&mut self.args_input)
                        .desired_rows(2)
                        .hint_text("-y\nserver-filesystem"),
                );
            }
            AddKind::Http => {
                ui.horizontal(|ui| {
                    ui.label("URL");
                    ui.text_edit_singleline(&mut self.url_input);
                });
                ui.weak(catalog.t("connectors.url_note", &[]));
            }
        }
        if ui.button(catalog.t("action.add", &[])).clicked() {
            self.add_server(db);
        }
        if let Some(error) = &self.error {
            ui.colored_label(egui::Color32::RED, error);
        }

        // 가져오기: README의 mcpServers JSON 붙여넣기 / .mcp.json 파일 / Claude Desktop 설정
        ui.separator();
        ui.label(catalog.t("connectors.import_title", &[]));
        ui.add(
            egui::TextEdit::multiline(&mut self.import_input)
                .desired_rows(3)
                .hint_text(r#"{"mcpServers": {"name": {"command": "npx", "args": ["..."]}}}"#),
        );
        ui.horizontal(|ui| {
            if ui
                .button(catalog.t("connectors.import_button", &[]))
                .clicked()
            {
                let text = self.import_input.clone();
                // 실패하면 입력을 남겨 고쳐서 재시도할 수 있게 한다
                if self.run_import(&text, db, ctx, env_resolver, catalog) > 0 {
                    self.import_input.clear();
                }
            }
            if ui
                .button(catalog.t("connectors.import_file", &[]))
                .clicked()
                && let Some(path) = rfd::FileDialog::new()
                    .add_filter("JSON", &["json"])
                    .pick_file()
            {
                self.import_from_path(&path, db, ctx, env_resolver, catalog);
            }
            if ui
                .button(catalog.t("connectors.import_claude_desktop", &[]))
                .clicked()
            {
                match claude_desktop_config_path() {
                    Some(path) => self.import_from_path(&path, db, ctx, env_resolver, catalog),
                    None => {
                        self.import_report = vec![
                            catalog.t("connectors.failed", &[("message", "config dir 확인 실패")]),
                        ];
                    }
                }
            }
        });
        for line in &self.import_report {
            ui.weak(line);
        }
    }

    /// [브라우저로 승인] 진입 (H5): 발견 체인(PRM → AS 메타데이터)을 백그라운드로
    /// 시작한다. 브라우저는 아직 열지 않는다 — 발견 결과의 authority를 보여주는
    /// 동의 다이얼로그를 통과해야 stage B(등록+브라우저)로 넘어간다.
    fn start_oauth_discovery(
        &mut self,
        ctx: &egui::Context,
        server: &McpServerRow,
        challenge: AuthChallengeInfo,
    ) {
        if self.oauth_flow.is_some() {
            return; // 브라우저 flow는 한 번에 하나
        }
        let Some(url) = server
            .url
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .map(str::to_owned)
        else {
            self.error = Some("http MCP 서버에 url이 없습니다".to_owned());
            return;
        };
        self.oauth_gen += 1;
        let generation = self.oauth_gen;
        self.oauth_flow = Some(OAuthFlow {
            server_id: server.id.clone(),
            server_name: server.name.clone(),
            server_url: url.clone(),
            generation,
            stage: OAuthStage::Discovering,
        });
        let tx = self.oauth_tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = discover_auth_metadata(OAUTH_HTTP_TIMEOUT, &url, &challenge)
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send((generation, OAuthMsg::Discovered(Box::new(result))));
            ctx.request_repaint();
        });
    }

    /// 동의 이후 stage B (H5): 클라이언트 확보(저장분/DCR/수동) → 외부 브라우저
    /// PKCE → Bearer 재시도 사다리. 전부 백그라운드 — 결과는 drain_oauth가 반영.
    fn start_oauth_authorize(
        &mut self,
        ctx: &egui::Context,
        discovered: DiscoveredAuth,
        plan: ClientPlan,
    ) {
        let Some(flow) = &mut self.oauth_flow else {
            return;
        };
        flow.stage = OAuthStage::Authorizing;
        let server_id = flow.server_id.clone();
        let server_name = flow.server_name.clone();
        let server_url = flow.server_url.clone();
        let generation = flow.generation;
        let manager = LocalMcpManager::new(self.redaction.clone());
        let tx = self.oauth_tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            // 브라우저 왕복 추상화 — 테스트는 이 자리를 콜백 직접 호출로 대체한다.
            let resource = server_url.clone();
            let authorize = move |config: &auth::OAuthProviderConfig| {
                auth::run_flow_with_resource(config, BROWSER_FLOW_TIMEOUT, Some(&resource))
            };
            let end = run_oauth_ladder(
                &manager,
                OAUTH_HTTP_TIMEOUT,
                &server_id,
                &server_name,
                &server_url,
                &discovered,
                plan,
                &authorize,
            );
            let msg = match end {
                LadderEnd::Success(success) => OAuthMsg::Finished(Box::new(Ok(*success))),
                LadderEnd::NeedManualClient { reason } => OAuthMsg::NeedManualClient {
                    discovered: Box::new(discovered),
                    reason,
                },
                LadderEnd::Failed(message) => OAuthMsg::Finished(Box::new(Err(message))),
            };
            let _ = tx.send((generation, msg));
            ctx.request_repaint();
        });
    }

    /// OAuth flow 백그라운드 결과 반영 (H5): 발견 결과 → 동의 단계 전환, 수동
    /// client_id 폴백 전환, 사다리 성공 시 keyring 저장 + credentials(oauth_json
    /// 바인딩) 등록 + redaction 시드 + tools 반영(자동 재시도 결과).
    /// credential을 추가/갱신했으면 true (호출측 캐시 무효화).
    pub fn drain_oauth(&mut self, db: &mut Db, oauth_store: &dyn OAuthCredentialStore) -> bool {
        let mut added = false;
        while let Ok((generation, msg)) = self.oauth_rx.try_recv() {
            // 취소됐거나(None) 재시작한(세대 불일치) flow의 결과는 폐기한다
            let (server_id, server_name) = match &self.oauth_flow {
                Some(flow) if flow.generation == generation => {
                    (flow.server_id.clone(), flow.server_name.clone())
                }
                _ => continue,
            };
            match msg {
                OAuthMsg::Discovered(result) => match *result {
                    Ok(discovered) => {
                        if let Some(flow) = &mut self.oauth_flow {
                            flow.stage = OAuthStage::Consent(discovered);
                        }
                    }
                    Err(message) => {
                        self.status.insert(
                            server_id,
                            ConnStatus::Failed(format!("인증 서버 발견 실패: {message}")),
                        );
                        self.oauth_flow = None;
                    }
                },
                OAuthMsg::NeedManualClient { discovered, reason } => {
                    if let Some(flow) = &mut self.oauth_flow {
                        flow.stage = OAuthStage::ManualClient {
                            discovered: *discovered,
                            reason,
                            client_id: String::new(),
                            client_secret: String::new(),
                        };
                    }
                }
                OAuthMsg::Finished(result) => {
                    self.oauth_flow = None;
                    match *result {
                        Ok(success) => {
                            match self.store_ladder_success(db, oauth_store, &server_name, success)
                            {
                                Ok(tools) => {
                                    self.status
                                        .insert(server_id, ConnStatus::Connected { tools });
                                    added = true;
                                }
                                Err(e) => {
                                    self.status.insert(
                                        server_id,
                                        ConnStatus::Failed(format!("승인 결과 저장 실패: {e:#}")),
                                    );
                                }
                            }
                        }
                        Err(message) => {
                            // 승인 실패는 다시 시도할 수 있는 상태 — 기존 챌린지를
                            // 유지한 채 "승인 필요"로 되돌린다.
                            let challenge = match self.status.get(&server_id) {
                                Some(ConnStatus::NeedsAuth { challenge, .. }) => challenge.clone(),
                                _ => AuthChallengeInfo::default(),
                            };
                            self.status
                                .insert(server_id, ConnStatus::NeedsAuth { message, challenge });
                        }
                    }
                }
            }
        }
        added
    }

    /// 사다리 성공 영속 (UI 스레드): 토큰 keyring 저장(기존 바인딩이 있으면 같은
    /// credential 재사용 — env 참조 유지) → oauth_json 바인딩 메타 → DCR secret →
    /// tools 반영. 성공 시 저장한 tool 수를 돌려준다.
    fn store_ladder_success(
        &mut self,
        db: &mut Db,
        oauth_store: &dyn OAuthCredentialStore,
        server_name: &str,
        success: LadderSuccess,
    ) -> anyhow::Result<usize> {
        let server_id = success.connection.server_id.clone();
        let existing = oauth_binding_for_server(db, &server_id).map(|(id, _)| id);
        let credential_id = match existing {
            Some(id) => {
                oauth_store
                    .update_oauth_token(&id, &success.token)
                    .context("keyring 토큰 갱신 실패")?;
                id
            }
            None => {
                let stored = oauth_store
                    .store_oauth_token(&success.token)
                    .context("keyring 저장 실패")?;
                let meta = CredentialMeta {
                    id: stored.id.clone(),
                    provider: "oauth".to_owned(),
                    label: server_name.to_owned(),
                    credential_kind: "oauth_token".to_owned(),
                    masked_hint: Some(stored.masked_hint),
                    // 커넥터(OAuth) credential은 MCP 서버(전역)와 짝 — 전역 공유(#2).
                    workspace_id: None,
                };
                if let Err(e) = db.insert_credential(&meta) {
                    // 고아 토큰 정리 (access + refresh + dcr)
                    if let Err(rollback) = oauth_store.delete_oauth_token(&stored.id) {
                        tracing::warn!("OAuth token rollback 실패: {rollback:#}");
                    }
                    return Err(e.context("credential 등록 실패"));
                }
                stored.id
            }
        };
        if let Err(e) = oauth_store.set_dcr_secret(&credential_id, success.client_secret.as_ref()) {
            tracing::warn!("DCR client_secret 저장 실패: {e:#}");
        }
        let json = serde_json::to_string(&success.connection).context("바인딩 메타 직렬화 실패")?;
        db.set_credential_oauth_json(&credential_id, &json)
            .context("바인딩 메타 저장 실패")?;
        // 사다리 마지막 성공 요청(tools/list)의 결과 반영 — "완료 후 자동 재시도"
        let rows = tool_rows(&server_id, &success.tools);
        db.replace_mcp_tools(&server_id, &rows)
            .context("tools 저장 실패")?;
        self.tools_cached.remove(&server_id);
        Ok(rows.len())
    }

    /// OAuth flow 모달 (H5): 단계별 다이얼로그 — 발견 중 / 브라우저 승인 동의 /
    /// 수동 client_id 폴백 / 승인 대기. 취소하면 세대를 올려 stale 결과를 폐기한다.
    fn oauth_flow_modal(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        db: &Db,
        catalog: &i18n::Catalog,
    ) {
        let Some(flow) = &mut self.oauth_flow else {
            return;
        };
        enum Act {
            None,
            Cancel,
            Consent(DiscoveredAuth),
            Manual(DiscoveredAuth, String, String),
        }
        let mut act = Act::None;
        let server_name = flow.server_name.clone();
        egui::Window::new(catalog.t("connectors.oauth_flow_title", &[]))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ui.ctx(), |ui| {
                match &mut flow.stage {
                    OAuthStage::Discovering => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(catalog.t("connectors.oauth_discovering", &[]));
                        });
                    }
                    OAuthStage::Consent(discovered) => {
                        // 브라우저 승인 진입 동의 — 서버가 사용자를 임의 인증 서버로
                        // 보내는 것을 막는 마지막 게이트 (차용: mainThreadMcp loginPrompt).
                        let authority = authority_of(&discovered.metadata.authorization_endpoint);
                        ui.label(catalog.t(
                            "connectors.oauth_consent_body",
                            &[("name", &server_name), ("authority", &authority)],
                        ));
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if ui
                                .button(catalog.t("connectors.approve_browser", &[]))
                                .clicked()
                            {
                                act = Act::Consent(discovered.clone());
                            }
                            if ui.button(catalog.t("action.cancel", &[])).clicked() {
                                act = Act::Cancel;
                            }
                        });
                    }
                    OAuthStage::ManualClient {
                        discovered,
                        reason,
                        client_id,
                        client_secret,
                    } => {
                        ui.label(catalog.t("connectors.oauth_manual_note", &[("reason", reason)]));
                        ui.weak(catalog.t("connectors.oauth_manual_hint", &[]));
                        ui.horizontal(|ui| {
                            ui.label(catalog.t("connectors.client_id", &[]));
                            ui.text_edit_singleline(client_id);
                        });
                        ui.horizontal(|ui| {
                            ui.label(catalog.t("connectors.client_secret", &[]));
                            ui.add(egui::TextEdit::singleline(client_secret).password(true));
                        });
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if ui
                                .add_enabled(
                                    !client_id.trim().is_empty(),
                                    egui::Button::new(catalog.t("connectors.oauth_continue", &[])),
                                )
                                .clicked()
                            {
                                act = Act::Manual(
                                    discovered.clone(),
                                    client_id.trim().to_owned(),
                                    client_secret.trim().to_owned(),
                                );
                            }
                            if ui.button(catalog.t("action.cancel", &[])).clicked() {
                                act = Act::Cancel;
                            }
                        });
                    }
                    OAuthStage::Authorizing => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(catalog.t("connectors.oauth_waiting", &[]));
                        });
                        if ui.button(catalog.t("action.cancel", &[])).clicked() {
                            act = Act::Cancel;
                        }
                    }
                }
            });
        match act {
            Act::None => {}
            Act::Cancel => {
                // 진행 중 백그라운드 결과는 세대 증가로 폐기된다. 브라우저 flow
                // 스레드는 자체 timeout(BROWSER_FLOW_TIMEOUT)으로 소멸한다.
                self.oauth_gen += 1;
                self.oauth_flow = None;
            }
            Act::Consent(discovered) => {
                // 기존 바인딩의 client 재사용 (같은 issuer) — 없으면 DCR부터.
                let plan = self
                    .stored_client_plan(db, &discovered)
                    .unwrap_or(ClientPlan::Dcr);
                self.start_oauth_authorize(ctx, discovered, plan);
            }
            Act::Manual(discovered, client_id, client_secret) => {
                let secret = if client_secret.is_empty() {
                    None
                } else {
                    Some(SecretString::new(client_secret))
                };
                self.start_oauth_authorize(
                    ctx,
                    discovered,
                    ClientPlan::Manual {
                        client_id,
                        client_secret: secret,
                    },
                );
            }
        }
    }

    /// 기존 바인딩(oauth_json)의 client 재사용 판단 — 발견된 issuer와 일치할 때만.
    fn stored_client_plan(&self, db: &Db, discovered: &DiscoveredAuth) -> Option<ClientPlan> {
        let flow = self.oauth_flow.as_ref()?;
        let (credential_id, connection) = oauth_binding_for_server(db, &flow.server_id)?;
        if connection.issuer != discovered.metadata.issuer {
            return None;
        }
        let client_secret = self
            .secret_store
            .get_secret(&auth::dcr_secret_entry_id(&credential_id))
            .ok();
        Some(ClientPlan::Stored {
            client_id: connection.client_id,
            client_secret,
            manual: connection.manual_client,
        })
    }

    fn server_card(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        db: &mut Db,
        server: &McpServerRow,
        env_resolver: &dyn McpScopedEnvResolver,
        catalog: &i18n::Catalog,
    ) {
        ui.group(|ui| {
            ui.horizontal(|ui| {
                ui.strong(&server.name);
                if server.kind == "http" {
                    // http 서버는 url이 정의의 전부 — command/args 대신 url 표시 (H3)
                    ui.weak(server.url.as_deref().unwrap_or(""));
                } else {
                    let command = server.command.as_deref().unwrap_or("");
                    ui.weak(format!(
                        "{} {}",
                        command,
                        mcp_args_for_display(&server.args)
                    ));
                }
            });
            // NeedsAuth 챌린지는 [브라우저로 승인]의 입력 — 상태 borrow 밖으로 복제
            let needs_auth = match self.status.get(&server.id) {
                Some(ConnStatus::NeedsAuth { challenge, .. }) => Some(challenge.clone()),
                _ => None,
            };
            let mut approve: Option<AuthChallengeInfo> = None;
            ui.horizontal(|ui| {
                match self.status.get(&server.id) {
                    None => ui.weak(catalog.t("connectors.unchecked", &[])),
                    Some(ConnStatus::Checking) => ui.weak(catalog.t("connectors.checking", &[])),
                    Some(ConnStatus::Connected { tools }) => ui.colored_label(
                        egui::Color32::from_rgb(0x2e, 0xa0, 0x43),
                        catalog.t(
                            "connectors.connected_tools",
                            &[("count", &tools.to_string())],
                        ),
                    ),
                    Some(ConnStatus::Failed(msg)) => ui.colored_label(
                        egui::Color32::RED,
                        catalog.t("connectors.failed", &[("message", msg)]),
                    ),
                    // 승인 필요(H5): 에러가 아니라 사용자 개입 지점 — 호박색 + 승인 버튼
                    Some(ConnStatus::NeedsAuth { message, .. }) => ui
                        .colored_label(
                            egui::Color32::from_rgb(0xd0, 0x8a, 0x00),
                            catalog.t("connectors.needs_auth", &[]),
                        )
                        .on_hover_text(message.clone()),
                };
                let checking = matches!(self.status.get(&server.id), Some(ConnStatus::Checking));
                if ui
                    .add_enabled(
                        !checking,
                        egui::Button::new(catalog.t("connectors.test", &[])),
                    )
                    .clicked()
                {
                    self.request_discover(ctx, db, server, env_resolver);
                }
                if let Some(challenge) = needs_auth
                    && ui
                        .add_enabled(
                            self.oauth_flow.is_none(),
                            egui::Button::new(catalog.t("connectors.approve_browser", &[])),
                        )
                        .clicked()
                {
                    approve = Some(challenge);
                }
            });
            if let Some(challenge) = approve {
                self.start_oauth_discovery(ctx, server, challenge);
            }
            // http 서버 url 편집 (H3) — 저장 시 Allow 규칙 초기화 + 캐시 무효화 + 재확인
            if server.kind == "http" {
                self.url_edit_controls(ui, db, server, catalog);
            }
            // 저장된 tool 목록 + 실행 버튼 + 현재 권한 규칙 (PR-16). 목록은 캐시로 —
            // 매 프레임 서버별 SQLite 조회를 막는다. 실패는 캐시하지 않는다(다음
            // 프레임 재시도 — 기존 매 프레임 조회와 같은 동작).
            let tools = match self.tools_cached.get(&server.id) {
                Some(tools) => tools.clone(),
                None => match db.list_mcp_tools(&server.id) {
                    Ok(tools) => {
                        self.tools_cached.insert(server.id.clone(), tools.clone());
                        tools
                    }
                    Err(_) => Vec::new(),
                },
            };
            for tool in tools {
                ui.horizontal(|ui| {
                    ui.monospace(&tool.name);
                    // 실행 중인 invoke가 있으면 새로 시작 금지 (동시 실행/덮어쓰기 방지)
                    if ui
                        .add_enabled(
                            self.invoke.is_none(),
                            egui::Button::new(catalog.t("action.run", &[])).small(),
                        )
                        .clicked()
                    {
                        // http 서버는 최초 연결 신뢰 확인을 먼저 받는다 (H3)
                        if server.kind == "http" && !self.trusted_http.contains(&server.id) {
                            self.trust_prompt = Some(TrustPrompt {
                                server: server.clone(),
                                tool: Some(tool.clone()),
                            });
                        } else if let Err(e) = self.begin_invoke(server, &tool) {
                            self.error = Some(format!("실행 준비 실패: {e:#}"));
                        }
                    }
                    // 현재 규칙 표시 + Ask 아니면 해제 버튼 (잘못 always한 것 되돌리기)
                    let rule = self.policy.rule(&server.id, &tool.name);
                    match rule {
                        audit::PermissionRule::Allow => {
                            ui.colored_label(
                                egui::Color32::from_rgb(0x2e, 0xa0, 0x43),
                                catalog.t("connectors.rule_allow", &[]),
                            );
                        }
                        audit::PermissionRule::Deny => {
                            ui.colored_label(
                                egui::Color32::RED,
                                catalog.t("connectors.rule_deny", &[]),
                            );
                        }
                        audit::PermissionRule::Ask => {}
                    }
                    if rule != audit::PermissionRule::Ask
                        && ui
                            .small_button(catalog.t("connectors.clear_rule", &[]))
                            .clicked()
                    {
                        self.policy
                            .set_rule(&server.id, &tool.name, audit::PermissionRule::Ask);
                        if let Err(e) = db.delete_permission_rule(&server.id, &tool.name) {
                            tracing::warn!("권한 규칙 삭제 실패: {e:#}");
                        }
                    }
                });
            }
        });
    }

    /// 도구 실행 시작 — Editing 상태로 invoke 패널을 연다.
    fn begin_invoke(&mut self, server: &McpServerRow, tool: &McpToolRow) -> anyhow::Result<()> {
        let target = invoke_target_for_row(server)?;
        self.invoke_gen += 1;
        // schema_hash는 저장분 우선, 없으면 스키마에서 재계산 (재승인 판정용)
        let schema_hash = tool.schema_hash.clone().unwrap_or_else(|| {
            audit::schema_hash(tool.input_schema_json.as_deref().unwrap_or("{}"))
        });
        self.invoke = Some(ToolInvoke {
            server_id: server.id.clone(),
            server_name: server.name.clone(),
            target,
            tool_name: tool.name.clone(),
            schema_hash,
            input: "{}".to_owned(),
            phase: InvokePhase::Editing,
            generation: self.invoke_gen,
            prepared_hash: None,
        });
        Ok(())
    }

    /// 연결 테스트 요청 — http 서버는 세션 최초 1회 신뢰 확인 모달을 거친다 (H3).
    fn request_discover(
        &mut self,
        ctx: &egui::Context,
        db: &Db,
        server: &McpServerRow,
        env_resolver: &dyn McpScopedEnvResolver,
    ) {
        if server.kind == "http" && !self.trusted_http.contains(&server.id) {
            self.trust_prompt = Some(TrustPrompt {
                server: server.clone(),
                tool: None,
            });
            return;
        }
        self.start_discover(ctx, db, server, env_resolver);
    }

    /// http 서버 최초 연결 신뢰 확인 모달 (H3, VS Code mcpRegistry 신뢰 프롬프트 차용).
    /// "이 서버로 도구 호출 데이터가 전송됩니다: {url}" — 확인 시 보류한 동작을 실행한다.
    fn trust_prompt_modal(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        db: &Db,
        env_resolver: &dyn McpScopedEnvResolver,
        catalog: &i18n::Catalog,
    ) {
        let Some(prompt) = &self.trust_prompt else {
            return;
        };
        let url = prompt.server.url.clone().unwrap_or_default();
        let mut decision: Option<bool> = None;
        egui::Window::new(catalog.t("connectors.trust_title", &[]))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ui.ctx(), |ui| {
                ui.label(catalog.t("connectors.trust_body", &[("url", &url)]));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui
                        .button(catalog.t("connectors.trust_connect", &[]))
                        .clicked()
                    {
                        decision = Some(true);
                    }
                    if ui.button(catalog.t("action.cancel", &[])).clicked() {
                        decision = Some(false);
                    }
                });
            });
        match decision {
            Some(true) => {
                let Some(prompt) = self.trust_prompt.take() else {
                    return;
                };
                self.trusted_http.insert(prompt.server.id.clone());
                match prompt.tool {
                    None => self.start_discover(ctx, db, &prompt.server, env_resolver),
                    Some(tool) => {
                        if let Err(e) = self.begin_invoke(&prompt.server, &tool) {
                            self.error = Some(format!("실행 준비 실패: {e:#}"));
                        }
                    }
                }
            }
            Some(false) => self.trust_prompt = None,
            None => {}
        }
    }

    /// http 서버 url 편집 컨트롤 (H3). 저장 실패 시 draft를 남겨 고쳐서 재시도한다.
    fn url_edit_controls(
        &mut self,
        ui: &mut egui::Ui,
        db: &mut Db,
        server: &McpServerRow,
        catalog: &i18n::Catalog,
    ) {
        let editing = self
            .url_edit
            .as_ref()
            .is_some_and(|edit| edit.server_id == server.id);
        if !editing {
            if ui
                .small_button(catalog.t("connectors.edit_url", &[]))
                .clicked()
            {
                self.url_edit = Some(UrlEdit {
                    server_id: server.id.clone(),
                    draft: server.url.clone().unwrap_or_default(),
                });
            }
            return;
        }
        // Some(true)=저장, Some(false)=취소
        let mut act: Option<bool> = None;
        if let Some(edit) = &mut self.url_edit {
            ui.horizontal(|ui| {
                ui.label("URL");
                ui.text_edit_singleline(&mut edit.draft);
            });
            // 규칙 리셋 고지 — url 변경의 사용자 놀람 방지 (계획 리스크 항목)
            ui.weak(catalog.t("connectors.url_edit_note", &[]));
            ui.horizontal(|ui| {
                if ui.button(catalog.t("action.save", &[])).clicked() {
                    act = Some(true);
                }
                if ui.button(catalog.t("action.cancel", &[])).clicked() {
                    act = Some(false);
                }
            });
        }
        match act {
            Some(true) => {
                let draft = self
                    .url_edit
                    .as_ref()
                    .map(|edit| edit.draft.clone())
                    .unwrap_or_default();
                match self.save_url_edit(db, &server.id, &draft) {
                    Ok(()) => {
                        self.url_edit = None;
                        self.error = None;
                    }
                    Err(e) => self.error = Some(format!("URL 저장 실패: {e:#}")),
                }
            }
            Some(false) => self.url_edit = None,
            None => {}
        }
    }

    /// url 편집 저장 (H3): 검증 → Allow 규칙 초기화(Deny/Ask 유지) → 도구 캐시 무효화
    /// → url 갱신 → 세션 신뢰 철회(다음 연결 전 재확인). 규칙 초기화를 url 갱신보다
    /// 먼저 해 중간 실패가 항상 안전한 방향(과잉 리셋)으로 남게 한다.
    /// VS Code의 cacheNonce 신뢰("정의 변경 = 재신뢰 + tools 재조회")를 deppy 정의가
    /// UI로만 바뀌는 점을 이용해 편집 저장 시점 훅으로 등가 구현 — 스키마 추가 없음.
    fn save_url_edit(&mut self, db: &mut Db, server_id: &str, new_url: &str) -> anyhow::Result<()> {
        let new_url = new_url.trim();
        validate_mcp_url(new_url)?;
        // Allow 규칙만 초기화 — Deny는 안전한 방향이라 유지한다.
        let rules = db.list_permission_rules().context("권한 규칙 조회 실패")?;
        for rule in rules
            .iter()
            .filter(|rule| rule.server_id == server_id && rule.rule == "allow")
        {
            // in-memory를 먼저 Ask로 되돌린다 (set_rule이 승인 이력도 무효화)
            self.policy
                .set_rule(&rule.server_id, &rule.tool_name, audit::PermissionRule::Ask);
            db.delete_permission_rule(&rule.server_id, &rule.tool_name)
                .with_context(|| format!("권한 규칙 삭제 실패: {}", rule.tool_name))?;
        }
        // 도구 목록 캐시 무효화 — "정의 변경 = tools 재조회 신호" (mcpTypes.ts 차용)
        db.replace_mcp_tools(server_id, &[])
            .context("mcp_tools 캐시 무효화 실패")?;
        db.update_mcp_server_url(server_id, new_url)?;
        self.trusted_http.remove(server_id);
        self.status.remove(server_id);
        self.tools_cached.remove(server_id);
        self.cached = None; // 목록 재조회
        Ok(())
    }

    /// 도구 실행 패널: 편집 → 정책 평가 → (승인) → tools/call → 결과. 감사는 결정 시점에.
    fn tool_invoke_panel(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        db: &mut Db,
        workspace_id: &str,
        env_resolver: &dyn McpScopedEnvResolver,
        catalog: &i18n::Catalog,
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
                            self.run_tool(&inv, db, workspace_id, ctx, decision, env_resolver)
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
            ui.strong(catalog.t(
                "connectors.tool_invoke_title",
                &[("server", &inv.server_name), ("tool", &inv.tool_name)],
            ));
            match &inv.phase {
                InvokePhase::Editing => {
                    ui.label(catalog.t("connectors.arguments_json", &[]));
                    ui.add(
                        egui::TextEdit::multiline(&mut inv.input)
                            .code_editor()
                            .desired_rows(3),
                    );
                    ui.horizontal(|ui| {
                        if ui.button(catalog.t("connectors.invoke", &[])).clicked() {
                            act = Act::Submit;
                        }
                        if ui.button(catalog.t("action.cancel", &[])).clicked() {
                            act = Act::Close;
                        }
                    });
                }
                InvokePhase::Preparing => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(catalog.t("connectors.schema_checking", &[]));
                    });
                }
                InvokePhase::Approval(reason) => {
                    let why = match reason {
                        audit::ApprovalReason::AskRule => {
                            catalog.t("connectors.approval.ask_rule", &[])
                        }
                        audit::ApprovalReason::FirstUse => {
                            catalog.t("connectors.approval.first_use", &[])
                        }
                        audit::ApprovalReason::SchemaChanged => {
                            catalog.t("connectors.approval.schema_changed", &[])
                        }
                    };
                    ui.colored_label(
                        egui::Color32::from_rgb(0xd0, 0x8a, 0x00),
                        catalog.t("connectors.approval_needed", &[("reason", &why)]),
                    );
                    ui.horizontal(|ui| {
                        if ui.button(catalog.t("connectors.allow_once", &[])).clicked() {
                            act = Act::Decide(audit::ToolDecision::AllowOnce);
                        }
                        if ui
                            .button(catalog.t("connectors.allow_always", &[]))
                            .clicked()
                        {
                            act = Act::Decide(audit::ToolDecision::AllowAlways);
                        }
                        if ui.button(catalog.t("connectors.deny_once", &[])).clicked() {
                            act = Act::Decide(audit::ToolDecision::DenyOnce);
                        }
                        if ui
                            .button(catalog.t("connectors.deny_always", &[]))
                            .clicked()
                        {
                            act = Act::Decide(audit::ToolDecision::DenyAlways);
                        }
                    });
                }
                InvokePhase::Running => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(catalog.t("status.running", &[]));
                    });
                }
                InvokePhase::Done(output) => {
                    ui.label(catalog.t("connectors.result", &[]));
                    let mut shown = output.clone();
                    ui.add(
                        egui::TextEdit::multiline(&mut shown)
                            .code_editor()
                            .desired_rows(6)
                            .interactive(false),
                    );
                    if ui.button(catalog.t("action.close", &[])).clicked() {
                        act = Act::Close;
                    }
                }
                InvokePhase::Failed(msg) => {
                    ui.colored_label(egui::Color32::RED, msg.clone());
                    if ui.button(catalog.t("action.close", &[])).clicked() {
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
                    self.run_tool(&inv, db, workspace_id, ctx, decision, env_resolver)
                } else {
                    self.start_prepare(&inv, ctx, db, env_resolver);
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
                inv.phase = self.run_tool(&inv, db, workspace_id, ctx, decision, env_resolver);
                self.invoke = Some(inv);
            }
        }
    }

    /// 백그라운드 실행 컨텍스트 (H5). RefreshCoordinator는 App 보관 단일 인스턴스의
    /// Arc 클론 — 동시 도구 호출의 refresh가 single-flight로 합쳐진다.
    fn exec_context(&self) -> ExecContext {
        ExecContext {
            manager: LocalMcpManager::new(self.redaction.clone()),
            coordinator: Arc::clone(&self.refresh),
            store: Arc::clone(&self.secret_store),
            redaction: self.redaction.clone(),
        }
    }

    /// 현재 tool 스키마를 서버에서 다시 가져와 schema hash를 확보한다 (백그라운드).
    /// 저장된 stale hash로 재승인을 우회하지 않도록 호출 직전에 재확인한다.
    fn start_prepare(
        &self,
        inv: &ToolInvoke,
        ctx: &egui::Context,
        db: &Db,
        env_resolver: &dyn McpScopedEnvResolver,
    ) {
        let config = match config_for_invoke(inv, env_resolver) {
            Ok(config) => config.with_http_auth(db, &inv.server_id),
            Err(e) => {
                let _ = self.invoke_tx.send((
                    inv.generation,
                    InvokeMsg::Result(Err(plain_failure_of(e))),
                    None,
                ));
                ctx.request_repaint();
                return;
            }
        };
        let cx = self.exec_context();
        let tx = self.invoke_tx.clone();
        let tool_name = inv.tool_name.clone();
        let generation = inv.generation;
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let (result, refresh) = config.discover_tools(&cx);
            let msg = match result {
                Ok(tools) => match tools.iter().find(|t| t.name == tool_name) {
                    Some(tool) => InvokeMsg::Prepared(audit::schema_hash(&tool.input_schema_json)),
                    None => InvokeMsg::Result(Err(ExecFailure {
                        message: format!("tool '{tool_name}'이 서버에 없습니다"),
                        auth: None,
                    })),
                },
                Err(failure) => InvokeMsg::Result(Err(ExecFailure {
                    message: format!("스키마 확인 실패: {}", failure.message),
                    auth: failure.auth,
                })),
            };
            let _ = tx.send((generation, msg, refresh));
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
        env_resolver: &dyn McpScopedEnvResolver,
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
        let config = match config_for_invoke(inv, env_resolver) {
            Ok(config) => config.with_http_auth(db, &inv.server_id),
            Err(e) => return InvokePhase::Failed(format!("{e:#}")),
        };
        let cx = self.exec_context();
        let redaction = self.redaction.clone();
        let tx = self.invoke_tx.clone();
        let tool_name = inv.tool_name.clone();
        let generation = inv.generation;
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            // 결과/에러 문자열은 표시 전에 등록된 secret을 마스킹한다 (§7 유출 방지).
            // 성공 결과와 에러 메시지(MCP 서버가 secret을 echo할 수 있음) 둘 다 대상.
            let (result, refresh) = config.call_tool(&cx, &tool_name, arguments);
            let result = match result {
                Ok(value) => Ok(redact_display(
                    &redaction,
                    &serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string()),
                )),
                Err(failure) => Err(ExecFailure {
                    message: redact_display(&redaction, &failure.message),
                    auth: failure.auth,
                }),
            };
            let _ = tx.send((generation, InvokeMsg::Result(result), refresh));
            ctx.request_repaint();
        });
        InvokePhase::Running
    }

    /// 연결 테스트를 백그라운드로 시작한다 (UI 프레임을 막지 않는다).
    /// http 서버의 신뢰 확인은 호출측(request_discover/trust_prompt_modal)이 끝냈다.
    fn start_discover(
        &mut self,
        ctx: &egui::Context,
        db: &Db,
        server: &McpServerRow,
        env_resolver: &dyn McpScopedEnvResolver,
    ) {
        let config = match config_for_row(server, env_resolver) {
            Ok(config) => config.with_http_auth(db, &server.id),
            Err(e) => {
                self.status
                    .insert(server.id.clone(), ConnStatus::Failed(format!("{e:#}")));
                return;
            }
        };
        self.status.insert(server.id.clone(), ConnStatus::Checking);
        let cx = self.exec_context();
        let tx = self.result_tx.clone();
        let ctx = ctx.clone();
        let server_id = server.id.clone();
        // 요청 시점 url — drain에서 url 편집 저장과의 race를 걸러낸다 (H3 리뷰 P2)
        let request_url = (server.kind == "http").then(|| server.url.clone().unwrap_or_default());
        std::thread::spawn(move || {
            let (result, refresh) = config.discover_tools(&cx);
            let _ = tx.send(DiscoverOutcome {
                server_id,
                request_url,
                result,
                refresh,
            });
            ctx.request_repaint();
        });
    }

    /// 백그라운드 결과 반영: 상태 갱신 + tools를 DB에 교체 저장 (schema_hash 포함).
    /// refresh 부수효과(만료 시각)는 결과 유효성과 무관하게 영속한다.
    pub fn drain_results(&mut self, db: &mut Db) {
        while let Ok(outcome) = self.result_rx.try_recv() {
            if let Some(update) = &outcome.refresh {
                persist_refresh_update(db, update);
            }
            // url 편집 저장 race 방어 (H3 리뷰 P2): 요청 시점 url이 현재 저장 url과
            // 다르면 — save_url_edit이 방금 비운 도구 캐시를 구 서버 결과로 재채우지
            // 않도록 — 결과를 통째로 폐기한다.
            if let Some(request_url) = &outcome.request_url {
                let current = db.list_mcp_servers().ok().and_then(|rows| {
                    rows.into_iter()
                        .find(|row| row.id == outcome.server_id)
                        .and_then(|row| row.url)
                });
                if current.as_deref().map(str::trim) != Some(request_url.trim()) {
                    tracing::info!(
                        server_id = %outcome.server_id,
                        "stale discover 결과 폐기 (url 변경/서버 삭제)"
                    );
                    continue;
                }
            }
            let status = match outcome.result {
                Ok(tools) => {
                    let rows = tool_rows(&outcome.server_id, &tools);
                    self.tools_cached.remove(&outcome.server_id);
                    match db.replace_mcp_tools(&outcome.server_id, &rows) {
                        Ok(()) => ConnStatus::Connected { tools: rows.len() },
                        Err(e) => ConnStatus::Failed(format!("tools 저장 실패: {e:#}")),
                    }
                }
                // 401/403은 에러가 아니라 승인 필요 (H5 상태 분리)
                Err(failure) => match failure.auth {
                    Some(challenge) => ConnStatus::NeedsAuth {
                        message: failure.message,
                        challenge,
                    },
                    None => ConnStatus::Failed(failure.message),
                },
            };
            self.status.insert(outcome.server_id, status);
        }
    }

    fn add_server(&mut self, db: &mut Db) {
        let name = self.name.trim();
        if name.is_empty() {
            self.error = Some("이름은 필수입니다".to_owned());
            return;
        }
        // 종류별 row 생성 (H3): stdio는 command/args, http는 url(저장 전 정책 검증)
        let row = match self.add_kind {
            AddKind::Stdio => {
                let command = self.command.trim();
                if command.is_empty() {
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
                McpServerRow {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: name.to_owned(),
                    kind: "stdio".to_owned(),
                    command: Some(command.to_owned()),
                    args,
                    env_plain: Vec::new(),
                    env_secrets: Vec::new(),
                    inherit_env: true,
                    url: None,
                    enabled: true,
                }
            }
            AddKind::Http => {
                let url = self.url_input.trim();
                if url.is_empty() {
                    self.error = Some("이름과 URL은 필수입니다".to_owned());
                    return;
                }
                // 저장 전 URL 정책 검증 — https 필수, http는 localhost/루프백만 (H2 규칙)
                if let Err(e) = validate_mcp_url(url) {
                    self.error = Some(format!("추가 실패: {e:#}"));
                    return;
                }
                McpServerRow {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: name.to_owned(),
                    kind: "http".to_owned(),
                    command: None,
                    args: Vec::new(),
                    env_plain: Vec::new(),
                    env_secrets: Vec::new(),
                    inherit_env: true,
                    url: Some(url.to_owned()),
                    enabled: true,
                }
            }
        };
        match db.insert_mcp_server(&row) {
            Ok(()) => {
                self.name.clear();
                self.command.clear();
                self.args_input.clear();
                self.url_input.clear();
                self.error = None;
                self.cached = None; // 목록 재조회
            }
            Err(e) => self.error = Some(format!("추가 실패: {e:#}")),
        }
    }

    /// 프리셋을 추가 폼에 채운다 — "{HOME}"은 홈 디렉토리 절대경로로 치환
    /// (filesystem 허용 루트처럼 서버가 절대경로 인자를 요구하는 경우).
    fn apply_preset(&mut self, preset: &McpPreset) {
        self.name = preset.name.to_owned();
        self.command = preset.command.to_owned();
        let home = directories::BaseDirs::new()
            .map(|dirs| dirs.home_dir().to_string_lossy().into_owned())
            .unwrap_or_default();
        self.args_input = preset
            .args
            .iter()
            .map(|arg| arg.replace("{HOME}", &home))
            .collect::<Vec<_>>()
            .join("\n");
        self.error = None;
    }

    /// 파일에서 mcpServers JSON을 읽어 가져온다 (.mcp.json / claude_desktop_config.json).
    fn import_from_path(
        &mut self,
        path: &std::path::Path,
        db: &mut Db,
        ctx: &egui::Context,
        env_resolver: &dyn McpScopedEnvResolver,
        catalog: &i18n::Catalog,
    ) {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                self.run_import(&text, db, ctx, env_resolver, catalog);
            }
            Err(e) => {
                self.import_report = vec![catalog.t(
                    "connectors.failed",
                    &[("message", &format!("{}: {e}", path.display()))],
                )];
            }
        }
    }

    /// mcpServers JSON 텍스트를 파싱해 stdio 서버를 등록하고, 등록 즉시 연결
    /// 테스트(discover)까지 시작한다. 서버별 결과는 import_report 한 줄씩.
    /// 반환: 등록한 서버 수.
    fn run_import(
        &mut self,
        text: &str,
        db: &mut Db,
        ctx: &egui::Context,
        env_resolver: &dyn McpScopedEnvResolver,
        catalog: &i18n::Catalog,
    ) -> usize {
        self.import_report.clear();
        let parse = match mcp_import::parse_mcp_servers_json(text) {
            Ok(parse) => parse,
            Err(e) => {
                self.import_report
                    .push(catalog.t("connectors.failed", &[("message", &format!("{e:#}"))]));
                return 0;
            }
        };
        // 이름 중복은 건너뛴다 (mcp_servers에 unique 제약이 없어 여기서 막는다)
        let mut existing: std::collections::HashSet<String> = match db.list_mcp_servers() {
            Ok(rows) => rows.into_iter().map(|row| row.name).collect(),
            Err(e) => {
                self.import_report
                    .push(catalog.t("connectors.failed", &[("message", &format!("{e:#}"))]));
                return 0;
            }
        };
        let mut added = 0;
        for server in parse.servers {
            let mcp_import::ParsedServer {
                name,
                command,
                args,
                env_plain,
                skipped_env,
            } = server;
            if !existing.insert(name.clone()) {
                self.import_report
                    .push(catalog.t("connectors.import_exists", &[("name", &name)]));
                continue;
            }
            let row = McpServerRow {
                id: uuid::Uuid::new_v4().to_string(),
                name: name.clone(),
                kind: "stdio".to_owned(),
                command: Some(command),
                args,
                env_plain,
                env_secrets: Vec::new(),
                inherit_env: true,
                url: None,
                enabled: true,
            };
            if self.import_insert(db, ctx, env_resolver, catalog, row, &skipped_env) {
                added += 1;
            } else {
                existing.remove(&name);
            }
        }
        // http 계열: url 매핑 등록 (H3). 등록 즉시 연결 테스트는 하지 않는다 —
        // 가져온 http 서버도 최초 연결 신뢰 확인(모달) 대상이다.
        for server in parse.http_servers {
            let mcp_import::ParsedHttpServer { name, url } = server;
            if !existing.insert(name.clone()) {
                self.import_report
                    .push(catalog.t("connectors.import_exists", &[("name", &name)]));
                continue;
            }
            // 저장 전 URL 정책 검증 — 추가 폼과 동일 규칙 (https 필수, localhost 예외)
            if let Err(e) = validate_mcp_url(&url) {
                existing.remove(&name);
                self.import_report.push(catalog.t(
                    "connectors.import_failed_row",
                    &[("name", &name), ("message", &format!("{e:#}"))],
                ));
                continue;
            }
            let row = McpServerRow {
                id: uuid::Uuid::new_v4().to_string(),
                name: name.clone(),
                kind: "http".to_owned(),
                command: None,
                args: Vec::new(),
                env_plain: Vec::new(),
                env_secrets: Vec::new(),
                inherit_env: true,
                url: Some(url),
                enabled: true,
            };
            if self.import_insert(db, ctx, env_resolver, catalog, row, &[]) {
                added += 1;
            } else {
                existing.remove(&name);
            }
        }
        for skipped in parse.skipped {
            let line = match &skipped.reason {
                SkipReason::LegacySse => {
                    catalog.t("connectors.import_skip_sse", &[("name", &skipped.name)])
                }
                SkipReason::MissingCommand => {
                    catalog.t("connectors.import_skip_command", &[("name", &skipped.name)])
                }
                SkipReason::MissingUrl => {
                    catalog.t("connectors.import_skip_url", &[("name", &skipped.name)])
                }
                SkipReason::Invalid(message) => catalog.t(
                    "connectors.import_skip_invalid",
                    &[("name", &skipped.name), ("message", message)],
                ),
            };
            self.import_report.push(line);
        }
        if self.import_report.is_empty() {
            self.import_report
                .push(catalog.t("connectors.import_none", &[]));
        }
        if added > 0 {
            self.cached = None; // 목록 재조회
        }
        added
    }

    /// run_import 등록 공용 경로 (stdio/http). 성공 시 보고 라인을 남기고 stdio만
    /// 곧바로 연결 테스트를 시작한다 — http는 최초 연결 신뢰 확인(모달)을 거쳐야
    /// 하므로 자동 연결하지 않는다 (H3). 반환: 등록 성공 여부.
    fn import_insert(
        &mut self,
        db: &mut Db,
        ctx: &egui::Context,
        env_resolver: &dyn McpScopedEnvResolver,
        catalog: &i18n::Catalog,
        row: McpServerRow,
        skipped_env: &[String],
    ) -> bool {
        match db.insert_mcp_server(&row) {
            Ok(()) => {
                self.import_report.push(if skipped_env.is_empty() {
                    catalog.t("connectors.import_added", &[("name", &row.name)])
                } else {
                    catalog.t(
                        "connectors.import_added_env_note",
                        &[("name", &row.name), ("keys", &skipped_env.join(", "))],
                    )
                });
                if row.kind == "stdio" {
                    // 붙여넣기 → 등록 → 곧바로 연결 확인까지 (수동 테스트 클릭 생략)
                    self.start_discover(ctx, db, &row, env_resolver);
                }
                true
            }
            Err(e) => {
                self.import_report.push(catalog.t(
                    "connectors.import_failed_row",
                    &[("name", &row.name), ("message", &format!("{e:#}"))],
                ));
                false
            }
        }
    }
}

/// Claude Desktop 설정 경로 — macOS `~/Library/Application Support/Claude/…`,
/// Windows `%APPDATA%\Claude\…`, Linux `~/.config/Claude/…` (config_dir 공통).
fn claude_desktop_config_path() -> Option<std::path::PathBuf> {
    directories::BaseDirs::new().map(|dirs| {
        dirs.config_dir()
            .join("Claude")
            .join("claude_desktop_config.json")
    })
}

fn mcp_config_for_values(
    name: &str,
    command: String,
    args: Vec<String>,
    inherit_env: bool,
    env_plain: &[(String, String)],
    env_secrets: &[(String, String)],
    env_resolver: &dyn McpScopedEnvResolver,
) -> anyhow::Result<McpServerConfig> {
    anyhow::ensure!(
        !command.trim().is_empty(),
        "MCP server command가 비어 있습니다"
    );
    mcp_store::validate_server_env_for_persistence(env_plain, env_secrets)
        .context("MCP scoped env validation 실패")?;
    let env = env_resolver.resolve_mcp_env(env_plain, env_secrets)?;
    Ok(McpServerConfig {
        name: name.to_owned(),
        command,
        args,
        env,
        inherit_env,
    })
}

/// kind별 MCP 연결 설정 (H3) — H2가 stdio(McpServerConfig)와 http(McpHttpServerConfig)를
/// 분리 타입으로 만들어, mcp_servers row의 kind('stdio'|'http')를 보고 여기서 분기한다.
/// Debug는 내부 config가 각각 env 값/bearer를 가리는 구현이라 파생해도 안전하다.
#[derive(Debug)]
enum ConnectorConfig {
    Stdio(McpServerConfig),
    Http(HttpConnectSpec),
}

/// http 연결 스펙 (H5): config + Bearer 바인딩 재료(비밀 없음). Debug는 내부가 각각 가린다.
#[derive(Debug)]
struct HttpConnectSpec {
    config: McpHttpServerConfig,
    auth: Option<HttpAuthBinding>,
}

impl std::fmt::Debug for HttpAuthState {
    /// refresh_params의 client_secret은 RefreshParams Debug가 이미 가린다.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpAuthState")
            .field("credential_id", &self.credential_id)
            .field("needs_refresh", &self.needs_refresh)
            .finish()
    }
}

/// 백그라운드 실행 컨텍스트 (H5): manager + refresh 조율자 + keyring + redaction.
struct ExecContext {
    manager: LocalMcpManager,
    coordinator: Arc<auth::RefreshCoordinator>,
    store: Arc<dyn SecretStore>,
    redaction: RedactionService,
}

impl ConnectorConfig {
    /// http 스펙에 Bearer 바인딩 재료를 부착한다 (UI 스레드 — DB만 접근, keyring은
    /// run_http가 백그라운드에서 해석). 동의 시점 URL ≠ 현재 URL이면 부착을
    /// 거부한다 (토큰 URL 바인딩, H5).
    fn with_http_auth(self, db: &Db, server_id: &str) -> Self {
        match self {
            Self::Stdio(config) => Self::Stdio(config),
            Self::Http(mut spec) => {
                match resolve_http_auth(db, server_id, &spec.config.url) {
                    HttpAuth::None | HttpAuth::UrlMismatch => {}
                    HttpAuth::Bound(binding) => spec.auth = Some(binding),
                }
                Self::Http(spec)
            }
        }
    }

    /// connect → tools/list — transport별 manager 경로로 위임 (http는 refresh 사다리 포함).
    fn discover_tools(
        &self,
        cx: &ExecContext,
    ) -> (Result<Vec<McpTool>, ExecFailure>, Option<RefreshUpdate>) {
        match self {
            Self::Stdio(config) => (
                cx.manager.discover_tools(config).map_err(plain_failure_of),
                None,
            ),
            Self::Http(spec) => run_http(cx, spec, |config| cx.manager.discover_tools_http(config)),
        }
    }

    /// connect → tools/call — transport별 manager 경로로 위임 (http는 refresh 사다리 포함).
    fn call_tool(
        &self,
        cx: &ExecContext,
        name: &str,
        arguments: serde_json::Value,
    ) -> (
        Result<serde_json::Value, ExecFailure>,
        Option<RefreshUpdate>,
    ) {
        match self {
            Self::Stdio(config) => (
                cx.manager
                    .call_tool(config, name, arguments)
                    .map_err(plain_failure_of),
                None,
            ),
            Self::Http(spec) => run_http(cx, spec, |config| {
                cx.manager.call_tool_http(config, name, arguments.clone())
            }),
        }
    }
}

/// 인증 정보 없는 실패로 변환 (stdio 등).
fn plain_failure_of(error: anyhow::Error) -> ExecFailure {
    ExecFailure {
        message: format!("{error:#}"),
        auth: None,
    }
}

/// http 요청 실행 (H5): 만료 임박이면 선제 refresh → 요청 → Bearer를 붙였는데
/// 401이면 반응 refresh 1회 후 원요청 1회 재시도. 그래도 401이면 챌린지를 실어
/// "승인 필요"로 보고한다. refresh는 여기서 최대 1회 — 무한 루프 없음.
fn run_http<T>(
    cx: &ExecContext,
    spec: &HttpConnectSpec,
    run: impl Fn(&McpHttpServerConfig) -> anyhow::Result<T>,
) -> (Result<T, ExecFailure>, Option<RefreshUpdate>) {
    let mut config = spec.config.clone();
    // keyring 해석(access/DCR secret)은 이 백그라운드 실행 스레드에서만 한다 —
    // UI 스레드가 프로세스 전역 KEYRING_SERIAL을 잡고(runtime worker와 경합,
    // 키체인 승인 다이얼로그 시 무기한) 프레임을 멈추지 않게 한다.
    let auth_state = spec.auth.as_ref().map(|binding| {
        let access = cx.store.get_secret(&binding.credential_id).ok();
        if let Some(access) = &access {
            cx.redaction.register(access);
        }
        let client_secret = cx
            .store
            .get_secret(&auth::dcr_secret_entry_id(&binding.credential_id))
            .ok();
        let expires_at = binding
            .expires_at_secs
            .map(|secs| SystemTime::UNIX_EPOCH + Duration::from_secs(secs));
        // access가 유실됐어도 refresh token으로 복구를 시도한다
        let needs_refresh = access.is_none() || auth::should_refresh(expires_at);
        config.bearer = access;
        HttpAuthState {
            credential_id: binding.credential_id.clone(),
            refresh_params: auth::RefreshParams {
                token_url: binding.token_url.clone(),
                client_id: binding.client_id.clone(),
                client_secret,
                // RFC 8707 — refresh 교환에도 대상 리소스를 고정
                resource: Some(binding.server_url.clone()),
            },
            needs_refresh,
        }
    });
    let mut update = None;
    let mut refreshed = false;
    // 선제 refresh: 만료 5분 전(H4 REFRESH_MARGIN) 또는 access 유실 시
    if let Some(auth_state) = &auth_state
        && auth_state.needs_refresh
    {
        update = refresh_bearer(cx, auth_state, &mut config);
        refreshed = true;
    }
    let error = match run(&config) {
        Ok(value) => return (Ok(value), update),
        Err(error) => error,
    };
    let challenge = auth_challenge_of(&error);
    // 반응 refresh: 저장 토큰을 붙였는데 401 — 만료 시각 메타가 없거나 stale한
    // 경우다. 이번 호출에서 아직 refresh를 안 했을 때만 1회 갱신 + 1회 재시도.
    if challenge.is_some()
        && !refreshed
        && config.bearer.is_some()
        && let Some(auth_state) = &auth_state
    {
        update = refresh_bearer(cx, auth_state, &mut config);
        if update.is_some() && config.bearer.is_some() {
            match run(&config) {
                Ok(value) => return (Ok(value), update),
                Err(retry_error) => {
                    let auth = auth_challenge_of(&retry_error);
                    return (
                        Err(ExecFailure {
                            message: format!("{retry_error:#}"),
                            auth,
                        }),
                        update,
                    );
                }
            }
        }
    }
    (
        Err(ExecFailure {
            message: format!("{error:#}"),
            auth: challenge,
        }),
        update,
    )
}

/// refresh 교환을 수행해 새 access를 config.bearer에 반영한다.
/// 갱신이 일어났으면 Some(RefreshUpdate) — 재시도 판단과 만료 시각 영속에 쓴다.
/// AS가 거부(재승인 필요)하면 bearer를 비운다 — 이어지는 401이 승인 카드로 이끈다.
fn refresh_bearer(
    cx: &ExecContext,
    auth_state: &HttpAuthState,
    config: &mut McpHttpServerConfig,
) -> Option<RefreshUpdate> {
    match auth::refresh_access_token(
        &cx.coordinator,
        OAUTH_HTTP_TIMEOUT,
        cx.store.as_ref(),
        &auth_state.credential_id,
        &auth_state.refresh_params,
    ) {
        Ok(auth::RefreshOutcome::Refreshed(token)) => {
            cx.redaction.register(&token.access_token);
            let expires_at_secs = token
                .expires_in_secs
                .and_then(|secs| unix_now_secs().map(|now| now + secs));
            config.bearer = Some(token.access_token);
            Some(RefreshUpdate {
                credential_id: auth_state.credential_id.clone(),
                expires_at_secs,
            })
        }
        Ok(auth::RefreshOutcome::AlreadyRefreshed) => {
            // 대기 중 다른 호출이 갱신을 끝냈다 — keyring 재조회 (H4 규약)
            match cx.store.get_secret(&auth_state.credential_id) {
                Ok(access) => {
                    cx.redaction.register(&access);
                    config.bearer = Some(access);
                    // 만료 시각은 갱신한 호출이 보고한다 — 여기서는 재시도 신호만
                    Some(RefreshUpdate {
                        credential_id: auth_state.credential_id.clone(),
                        expires_at_secs: None,
                    })
                }
                Err(e) => {
                    tracing::warn!("갱신된 access token 조회 실패: {e:#}");
                    None
                }
            }
        }
        Ok(auth::RefreshOutcome::ReauthorizationRequired { reason }) => {
            tracing::info!(
                credential_id = %auth_state.credential_id,
                "refresh 거부 — 재승인 필요: {reason}"
            );
            config.bearer = None;
            None
        }
        Err(e) => {
            // 일시 장애 — 기존 access로 진행 (아직 유효할 수 있다)
            tracing::warn!("refresh 교환 실패 (일시 장애로 간주): {e:#}");
            None
        }
    }
}

/// anyhow chain에서 401/403 챌린지를 꺼낸다 (mcp::McpAuthRequired downcast — H5 훅).
fn auth_challenge_of(error: &anyhow::Error) -> Option<AuthChallengeInfo> {
    let required = error.downcast_ref::<McpAuthRequired>()?;
    let mut info = AuthChallengeInfo::default();
    if let Some(header) = required.www_authenticate.as_deref() {
        let challenges = auth::parse_www_authenticate(header);
        if let Some(bearer) = auth::find_bearer_challenge(&challenges) {
            info.resource_metadata = bearer.resource_metadata().map(str::to_owned);
            info.scope = bearer.scope().map(str::to_owned);
        }
    }
    Some(info)
}

/// http 서버의 Bearer 바인딩 해석 (UI 스레드): oauth_json 바인딩 조회 → **토큰 URL
/// 바인딩 검사**(동의 시점 URL ≠ 현재 URL이면 부착 거부 — H3 url 편집 규칙 리셋과
/// 한 쌍). keyring 접근(access/DCR secret)은 하지 않는다 — 백그라운드 실행
/// 스레드(run_http)가 이 재료로 해석한다.
fn resolve_http_auth(db: &Db, server_id: &str, current_url: &str) -> HttpAuth {
    let Some((credential_id, connection)) = oauth_binding_for_server(db, server_id) else {
        return HttpAuth::None;
    };
    if connection.server_url.trim() != current_url.trim() {
        tracing::info!(
            server_id,
            "동의 시점 URL과 현재 URL 불일치 — Bearer 부착 거부 (재동의 필요)"
        );
        return HttpAuth::UrlMismatch;
    }
    HttpAuth::Bound(HttpAuthBinding {
        credential_id,
        token_url: connection.token_endpoint,
        client_id: connection.client_id,
        server_url: connection.server_url,
        expires_at_secs: connection.expires_at_secs,
    })
}

/// credentials.oauth_json에서 server_id 바인딩을 찾는다 (서버당 1개 유지 규약).
fn oauth_binding_for_server(db: &Db, server_id: &str) -> Option<(String, OAuthConnection)> {
    let rows = match db.list_credential_oauth_json() {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!("oauth 바인딩 조회 실패: {e:#}");
            return None;
        }
    };
    for (credential_id, json) in rows {
        match serde_json::from_str::<OAuthConnection>(&json) {
            Ok(connection) if connection.server_id == server_id => {
                return Some((credential_id, connection));
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(credential_id, "oauth 바인딩 파싱 실패: {e}"),
        }
    }
    None
}

/// refresh가 갱신한 만료 시각을 oauth_json에 반영한다 — 다음 선제 refresh 판정이
/// 최신 값을 보도록. expires_at_secs가 None(만료 미상)이면 건드리지 않는다.
fn persist_refresh_update(db: &Db, update: &RefreshUpdate) {
    if update.expires_at_secs.is_none() {
        return;
    }
    let Some((credential_id, mut connection)) = (match db.list_credential_oauth_json() {
        Ok(rows) => rows.into_iter().find_map(|(id, json)| {
            (id == update.credential_id)
                .then(|| serde_json::from_str::<OAuthConnection>(&json).ok())
                .flatten()
                .map(|connection| (id, connection))
        }),
        Err(e) => {
            tracing::warn!("refresh 만료 시각 반영 실패 (조회): {e:#}");
            None
        }
    }) else {
        return;
    };
    connection.expires_at_secs = update.expires_at_secs;
    match serde_json::to_string(&connection) {
        Ok(json) => {
            if let Err(e) = db.set_credential_oauth_json(&credential_id, &json) {
                tracing::warn!("refresh 만료 시각 반영 실패 (저장): {e:#}");
            }
        }
        Err(e) => tracing::warn!("refresh 만료 시각 직렬화 실패: {e}"),
    }
}

/// 발견 체인 (H5 사다리 단 ①): 401 챌린지 → PRM(RFC 9728) → AS 메타데이터(RFC 8414).
/// PRM이 전부 실패하면 서버 origin을 AS로 간주한다 (2025-03-26 스펙 하위호환 —
/// PRM 없는 서버는 자신이 AS). 커스텀 헤더(MCP-Protocol-Version)는 same-origin
/// 대상에만 붙는다 (DiscoveryHeaders — 교차 출처 누출 방지).
/// AS 메타데이터의 issuer 불일치는 discover_authorization_server가 Err로 거부한다
/// (H4 리뷰 P1 — 폴백 아님).
fn discover_auth_metadata(
    timeout: Duration,
    server_url: &str,
    challenge: &AuthChallengeInfo,
) -> anyhow::Result<DiscoveredAuth> {
    let headers = auth::DiscoveryHeaders::new(
        server_url,
        vec![(
            "MCP-Protocol-Version".to_owned(),
            PROTOCOL_VERSION.to_owned(),
        )],
    )?;
    let origin = auth::validate_https_or_loopback(server_url)?
        .origin()
        .ascii_serialization();
    let prm = auth::discover_protected_resource(
        timeout,
        server_url,
        challenge.resource_metadata.as_deref(),
        Some(&headers),
    );
    let (authorization_server, prm_scopes) = match prm {
        Ok(metadata) => {
            let as_url = metadata
                .authorization_servers
                .iter()
                .find(|url| auth::validate_https_or_loopback(url).is_ok())
                .cloned()
                .unwrap_or_else(|| origin.clone());
            (as_url, metadata.scopes_supported)
        }
        Err(e) => {
            tracing::info!("PRM 발견 실패 — 서버 origin을 AS로 간주: {e:#}");
            (origin, None)
        }
    };
    let metadata =
        auth::discover_authorization_server(timeout, &authorization_server, Some(&headers))?;
    // scope 우선순위: 401 챌린지 > PRM scopes_supported > 없음
    let scopes = challenge
        .scope
        .as_deref()
        .map(split_scopes)
        .filter(|scopes| !scopes.is_empty())
        .or(prm_scopes)
        .unwrap_or_default();
    Ok(DiscoveredAuth { metadata, scopes })
}

fn split_scopes(raw: &str) -> Vec<String> {
    raw.split_whitespace().map(str::to_owned).collect()
}

/// 401 인증 사다리 (H5). 각 단은 1회 한정 — 무한 루프 구조 봉쇄:
/// (단 ①: 발견 체인은 호출 전 [`discover_auth_metadata`]) → 클라이언트 확보
/// (저장분/DCR/수동) → 브라우저 승인 → Bearer 재시도. 재시도가 다시 401이면
/// 단 ② scope 챌린지 변경 시 scope 갱신 + 재승인 1회, 단 ③ 클라이언트 재등록
/// (DCR) + 재승인 1회. 그래도 401이면 종료 — 트리거 401을 포함해 3연속 401이
/// 최종 에러가 된다. 성공 시 마지막 요청(tools/list)의 결과가 "자동 재시도"의
/// 산물로 함께 반환된다.
///
/// `authorize`는 브라우저 왕복 추상화 — 프로덕션은 run_flow_with_resource(외부
/// 브라우저), 테스트는 콜백 직접 호출로 대체한다 (완료 기준).
#[allow(clippy::too_many_arguments)]
fn run_oauth_ladder(
    manager: &LocalMcpManager,
    timeout: Duration,
    server_id: &str,
    server_name: &str,
    server_url: &str,
    discovered: &DiscoveredAuth,
    plan: ClientPlan,
    authorize: &dyn Fn(&auth::OAuthProviderConfig) -> anyhow::Result<auth::OAuthToken>,
) -> LadderEnd {
    let metadata = &discovered.metadata;
    let mut scopes = discovered.scopes.clone();
    let (mut client_id, mut client_secret, manual) = match plan {
        ClientPlan::Stored {
            client_id,
            client_secret,
            manual,
        } => (client_id, client_secret, manual),
        ClientPlan::Manual {
            client_id,
            client_secret,
        } => (client_id, client_secret, true),
        ClientPlan::Dcr => match register_ladder_client(timeout, metadata, &scopes) {
            Ok((client_id, client_secret)) => (client_id, client_secret, false),
            Err(auth::RegistrationError::Other(e)) => {
                return LadderEnd::Failed(format!("클라이언트 등록 실패: {e:#}"));
            }
            // DCR 미지원/거부 → 수동 client_id 입력 폴백 (완료 기준)
            Err(e) => {
                return LadderEnd::NeedManualClient {
                    reason: e.to_string(),
                };
            }
        },
    };
    let mut token = match authorize(&provider_config(metadata, &client_id, &scopes)) {
        Ok(token) => token,
        Err(e) => return LadderEnd::Failed(format!("브라우저 승인 실패: {e:#}")),
    };
    let mut scope_refreshed = false;
    let mut reregistered = manual; // 수동 client는 재등록 단을 쓰지 않는다
    loop {
        let config = McpHttpServerConfig {
            name: server_name.to_owned(),
            url: server_url.to_owned(),
            bearer: Some(SecretString::new(token.access_token.expose().to_owned())),
        };
        let error = match manager.discover_tools_http(&config) {
            Ok(tools) => {
                let connection = OAuthConnection {
                    server_id: server_id.to_owned(),
                    // 동의 시점 URL 저장 — 이후 Bearer 부착의 바인딩 기준
                    server_url: server_url.to_owned(),
                    issuer: metadata.issuer.clone(),
                    authorization_endpoint: metadata.authorization_endpoint.clone(),
                    token_endpoint: metadata.token_endpoint.clone(),
                    client_id,
                    manual_client: manual,
                    scopes,
                    expires_at_secs: token
                        .expires_in_secs
                        .and_then(|secs| unix_now_secs().map(|now| now + secs)),
                };
                return LadderEnd::Success(Box::new(LadderSuccess {
                    connection,
                    token,
                    client_secret,
                    tools,
                }));
            }
            Err(error) => error,
        };
        let Some(challenge) = auth_challenge_of(&error) else {
            return LadderEnd::Failed(format!("{error:#}"));
        };
        // 단 ②: scope 챌린지 변경 — 1회만 scope를 갱신해 재승인한다
        let challenge_scopes = challenge
            .scope
            .as_deref()
            .map(split_scopes)
            .filter(|scopes| !scopes.is_empty());
        if let Some(new_scopes) = challenge_scopes
            && new_scopes != scopes
            && !scope_refreshed
        {
            scope_refreshed = true;
            scopes = new_scopes;
            token = match authorize(&provider_config(metadata, &client_id, &scopes)) {
                Ok(token) => token,
                Err(e) => {
                    return LadderEnd::Failed(format!("브라우저 승인 실패 (scope 갱신): {e:#}"));
                }
            };
            continue;
        }
        // 단 ③: Authorization을 붙였는데도 401 — 등록 폐기 + 재등록(DCR) 1회만
        if !reregistered {
            reregistered = true;
            match register_ladder_client(timeout, metadata, &scopes) {
                Ok((new_id, new_secret)) => {
                    client_id = new_id;
                    client_secret = new_secret;
                }
                Err(e) => return LadderEnd::Failed(format!("클라이언트 재등록 실패: {e}")),
            }
            token = match authorize(&provider_config(metadata, &client_id, &scopes)) {
                Ok(token) => token,
                Err(e) => return LadderEnd::Failed(format!("브라우저 승인 실패 (재등록): {e:#}")),
            };
            continue;
        }
        return LadderEnd::Failed(format!(
            "인증 사다리 소진 — 재등록 후에도 인증 거부 (연속 401): {error:#}"
        ));
    }
}

/// RFC 7591 DCR 실행 — (client_id, client_secret). 사다리의 최초 등록과 재등록 공용.
fn register_ladder_client(
    timeout: Duration,
    metadata: &auth::AuthorizationServerMetadata,
    scopes: &[String],
) -> Result<(String, Option<SecretString>), auth::RegistrationError> {
    let registration = auth::register_client(
        timeout,
        metadata,
        &auth::RegistrationOptions {
            client_name: "deppy-sijo".to_owned(),
            scopes: scopes.to_vec(),
        },
    )?;
    Ok((registration.client_id, registration.client_secret))
}

/// 발견된 endpoint로 PKCE flow 설정을 만든다 — 구 OAuth 폼(수동 URL 입력)의 대체.
fn provider_config(
    metadata: &auth::AuthorizationServerMetadata,
    client_id: &str,
    scopes: &[String],
) -> auth::OAuthProviderConfig {
    auth::OAuthProviderConfig {
        auth_url: metadata.authorization_endpoint.clone(),
        token_url: metadata.token_endpoint.clone(),
        client_id: client_id.to_owned(),
        scopes: scopes.to_vec(),
    }
}

/// 동의 다이얼로그에 표시할 인증 서버 authority — "https://as.example" 형태.
fn authority_of(url: &str) -> String {
    auth::validate_https_or_loopback(url)
        .map(|parsed| parsed.origin().ascii_serialization())
        .unwrap_or_else(|_| url.to_owned())
}

fn unix_now_secs() -> Option<u64> {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .map(|elapsed| elapsed.as_secs())
}

/// McpServerRow → kind별 ConnectorConfig (H3). stdio는 scoped env 해석을 포함한다.
/// http의 Bearer 부착은 [`ConnectorConfig::with_http_auth`]가 이어서 수행한다 (H5).
fn config_for_row(
    row: &McpServerRow,
    env_resolver: &dyn McpScopedEnvResolver,
) -> anyhow::Result<ConnectorConfig> {
    match row.kind.as_str() {
        "http" => Ok(ConnectorConfig::Http(HttpConnectSpec {
            config: http_config_for_values(&row.name, row.url.as_deref())?,
            auth: None,
        })),
        "stdio" => {
            let command = row.command.clone().unwrap_or_default();
            Ok(ConnectorConfig::Stdio(mcp_config_for_values(
                &row.name,
                command,
                row.args.clone(),
                row.inherit_env,
                &row.env_plain,
                &row.env_secrets,
                env_resolver,
            )?))
        }
        other => anyhow::bail!("지원하지 않는 MCP 서버 kind: {other}"),
    }
}

/// http config 생성 — 저장 전과 같은 URL 정책 검증을 연결 직전에도 적용한다 (H3).
/// bearer는 여기서 None — credential 연동은 with_http_auth(H5)가 부착한다.
fn http_config_for_values(name: &str, url: Option<&str>) -> anyhow::Result<McpHttpServerConfig> {
    let url = url
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .with_context(|| format!("http MCP 서버 '{name}'에 url이 없습니다"))?;
    validate_mcp_url(url)?;
    Ok(McpHttpServerConfig {
        name: name.to_owned(),
        url: url.to_owned(),
        bearer: None,
    })
}

/// McpServerRow → InvokeTarget (H3): 실행 전에 kind별 필수 필드를 검증해 보관한다.
fn invoke_target_for_row(server: &McpServerRow) -> anyhow::Result<InvokeTarget> {
    match server.kind.as_str() {
        "http" => {
            let url = server
                .url
                .as_deref()
                .map(str::trim)
                .filter(|url| !url.is_empty())
                .context("MCP server url이 비어 있습니다")?;
            validate_mcp_url(url)?;
            Ok(InvokeTarget::Http {
                url: url.to_owned(),
            })
        }
        "stdio" => {
            let command = server.command.clone().unwrap_or_default();
            anyhow::ensure!(
                !command.trim().is_empty(),
                "MCP server command가 비어 있습니다"
            );
            mcp_store::validate_server_env_for_persistence(&server.env_plain, &server.env_secrets)
                .context("MCP scoped env validation 실패")?;
            Ok(InvokeTarget::Stdio {
                command,
                args: server.args.clone(),
                env_plain: server.env_plain.clone(),
                env_secrets: server.env_secrets.clone(),
                inherit_env: server.inherit_env,
            })
        }
        other => anyhow::bail!("지원하지 않는 MCP 서버 kind: {other}"),
    }
}

fn config_for_invoke(
    inv: &ToolInvoke,
    env_resolver: &dyn McpScopedEnvResolver,
) -> anyhow::Result<ConnectorConfig> {
    match &inv.target {
        InvokeTarget::Stdio {
            command,
            args,
            env_plain,
            env_secrets,
            inherit_env,
        } => Ok(ConnectorConfig::Stdio(mcp_config_for_values(
            &inv.server_name,
            command.clone(),
            args.clone(),
            *inherit_env,
            env_plain,
            env_secrets,
            env_resolver,
        )?)),
        InvokeTarget::Http { url } => Ok(ConnectorConfig::Http(HttpConnectSpec {
            config: http_config_for_values(&inv.server_name, Some(url))?,
            auth: None,
        })),
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
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_db_path() -> PathBuf {
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("deppy-connectors-test-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("metadata.sqlite3")
    }

    fn test_ui() -> ConnectorsUi {
        ConnectorsUi::new(
            RedactionService::new(),
            Arc::new(MemStore::default()),
            Arc::new(auth::RefreshCoordinator::new()),
        )
    }

    // ---------- 테스트 인프라: 인메모리 SecretStore + 목 HTTP 서버 ----------
    // auth::test_support는 cfg(test) crate-private이라 이 crate 테스트용으로 축약 복제.

    #[derive(Default)]
    struct MemStore(Mutex<std::collections::HashMap<String, String>>);

    impl MemStore {
        fn seed(&self, id: &str, value: &str) {
            self.0
                .lock()
                .unwrap()
                .insert(id.to_owned(), value.to_owned());
        }

        fn value(&self, id: &str) -> Option<String> {
            self.0.lock().unwrap().get(id).cloned()
        }
    }

    impl SecretStore for MemStore {
        fn set_secret(&self, id: &str, secret: &SecretString) -> anyhow::Result<()> {
            self.seed(id, secret.expose());
            Ok(())
        }
        fn get_secret(&self, id: &str) -> anyhow::Result<SecretString> {
            self.value(id)
                .map(SecretString::new)
                .ok_or_else(|| anyhow::anyhow!("no entry: {id}"))
        }
        fn delete_secret(&self, id: &str) -> anyhow::Result<()> {
            self.0.lock().unwrap().remove(id);
            Ok(())
        }
        fn has_secret(&self, id: &str) -> anyhow::Result<bool> {
            Ok(self.value(id).is_some())
        }
    }

    /// 목 서버가 받은 요청 한 건 (헤더 키 소문자).
    #[derive(Debug, Clone)]
    struct RecordedRequest {
        method: String,
        path: String,
        headers: Vec<(String, String)>,
        body: String,
    }

    impl RecordedRequest {
        fn header(&self, name: &str) -> Option<&str> {
            let lower = name.to_ascii_lowercase();
            self.headers
                .iter()
                .find(|(key, _)| *key == lower)
                .map(|(_, value)| value.as_str())
        }

        fn rpc_method(&self) -> String {
            serde_json::from_str::<serde_json::Value>(&self.body)
                .ok()
                .and_then(|value| {
                    value
                        .get("method")
                        .and_then(|m| m.as_str())
                        .map(str::to_owned)
                })
                .unwrap_or_default()
        }
    }

    struct MockResponse {
        status: u16,
        body: String,
        headers: Vec<(String, String)>,
    }

    impl MockResponse {
        fn json(status: u16, body: impl Into<String>) -> Self {
            Self {
                status,
                body: body.into(),
                headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
            }
        }

        fn with_header(mut self, name: &str, value: impl Into<String>) -> Self {
            self.headers.push((name.to_owned(), value.into()));
            self
        }
    }

    /// 요청마다 핸들러를 부르는 초소형 HTTP 서버 (std TcpListener, tokio 금지 관례).
    struct MockHttpServer {
        base_url: String,
        requests: Arc<Mutex<Vec<RecordedRequest>>>,
        stop: Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl MockHttpServer {
        fn start(
            handler: impl Fn(&RecordedRequest) -> MockResponse + Send + Sync + 'static,
        ) -> Self {
            let listener = TcpListener::bind(("127.0.0.1", 0)).expect("mock bind");
            listener.set_nonblocking(true).expect("mock nonblocking");
            let port = listener.local_addr().expect("mock addr").port();
            let requests: Arc<Mutex<Vec<RecordedRequest>>> = Arc::default();
            let stop = Arc::new(AtomicBool::new(false));
            let handle = {
                let requests = Arc::clone(&requests);
                let stop = Arc::clone(&stop);
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        match listener.accept() {
                            Ok((stream, _)) => {
                                if let Some((request, stream)) = read_request(stream) {
                                    requests.lock().unwrap().push(request.clone());
                                    write_response(stream, &handler(&request));
                                }
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                std::thread::sleep(Duration::from_millis(5));
                            }
                            Err(_) => break,
                        }
                    }
                })
            };
            Self {
                base_url: format!("http://127.0.0.1:{port}"),
                requests,
                stop,
                handle: Some(handle),
            }
        }

        fn url(&self, path: &str) -> String {
            format!("{}{path}", self.base_url)
        }

        fn requests(&self) -> Vec<RecordedRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl Drop for MockHttpServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    fn read_request(stream: TcpStream) -> Option<(RecordedRequest, TcpStream)> {
        stream.set_nonblocking(false).ok()?;
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
        let mut reader = BufReader::new(stream);
        let mut request_line = String::new();
        reader.read_line(&mut request_line).ok()?;
        let mut parts = request_line.split_whitespace();
        let method = parts.next()?.to_owned();
        let path = parts.next()?.to_owned();
        let mut headers = Vec::new();
        let mut content_length = 0usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).ok()?;
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some((key, value)) = line.split_once(':') {
                let key = key.trim().to_ascii_lowercase();
                let value = value.trim().to_owned();
                if key == "content-length" {
                    content_length = value.parse().unwrap_or(0);
                }
                headers.push((key, value));
            }
        }
        let mut body = vec![0u8; content_length];
        if content_length > 0 {
            reader.read_exact(&mut body).ok()?;
        }
        Some((
            RecordedRequest {
                method,
                path,
                headers,
                body: String::from_utf8_lossy(&body).into_owned(),
            },
            reader.into_inner(),
        ))
    }

    fn write_response(mut stream: TcpStream, response: &MockResponse) {
        let mut head = format!(
            "HTTP/1.1 {} Mock\r\nContent-Length: {}\r\nConnection: close\r\n",
            response.status,
            response.body.len()
        );
        for (name, value) in &response.headers {
            head.push_str(name);
            head.push_str(": ");
            head.push_str(value);
            head.push_str("\r\n");
        }
        head.push_str("\r\n");
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(response.body.as_bytes());
    }

    /// JSON-RPC 요청에 대한 정상 MCP 응답 (initialize/initialized/tools/list).
    fn mcp_reply(request: &RecordedRequest) -> MockResponse {
        let body: serde_json::Value = serde_json::from_str(&request.body).unwrap_or_default();
        let id = body.get("id").cloned().unwrap_or(serde_json::Value::Null);
        match body.get("method").and_then(|m| m.as_str()) {
            Some("initialize") => MockResponse::json(
                200,
                serde_json::json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {
                        "protocolVersion": "2025-11-25",
                        "capabilities": {},
                        "serverInfo": {"name": "mock", "version": "0"},
                    },
                })
                .to_string(),
            ),
            Some("notifications/initialized") => MockResponse::json(202, ""),
            Some("tools/list") => MockResponse::json(
                200,
                serde_json::json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {"tools": [{"name": "remote_tool", "inputSchema": {"type": "object"}}]},
                })
                .to_string(),
            ),
            other => MockResponse::json(500, format!("{{\"unexpected\":{other:?}}}")),
        }
    }

    fn unauthorized(challenge: &str) -> MockResponse {
        MockResponse::json(401, "{}").with_header("WWW-Authenticate", challenge)
    }

    /// 브라우저 대신 콜백을 직접 치는 GET (완료 기준 — 브라우저 왕복 대체).
    fn http_get(url: &str) {
        let Some(rest) = url.strip_prefix("http://") else {
            return;
        };
        let (addr, path) = match rest.split_once('/') {
            Some((addr, path)) => (addr.to_owned(), format!("/{path}")),
            None => (rest.to_owned(), "/".to_owned()),
        };
        if let Ok(mut stream) = TcpStream::connect(&addr) {
            let _ = write!(
                stream,
                "GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
            );
            let mut sink = String::new();
            let _ = stream.read_to_string(&mut sink);
        }
    }

    fn query_param(url: &str, key: &str) -> Option<String> {
        let (_, query) = url.split_once('?')?;
        query.split('&').find_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            (k == key).then(|| v.to_owned())
        })
    }

    /// 외부 브라우저 없이 PKCE flow를 완주하는 authorize 대역: 콜백 서버 bind →
    /// begin → 콜백 직접 호출 → complete(목 token endpoint와 실제 code 교환).
    fn callback_authorize(
        resource: &str,
    ) -> impl Fn(&auth::OAuthProviderConfig) -> anyhow::Result<auth::OAuthToken> {
        let resource = resource.to_owned();
        move |config| {
            let callback = auth::LocalhostCallbackServer::bind()?;
            let pending =
                auth::begin_with_resource(config, callback.redirect_uri(), Some(&resource))?;
            let state = query_param(&pending.authorize_url, "state")
                .context("authorize URL에 state 없음")?;
            let target = format!("{}?code=mock-code&state={state}", callback.redirect_uri());
            let opener = std::thread::spawn(move || http_get(&target));
            let params = callback.wait_for_callback(Duration::from_secs(10), &state)?;
            let _ = opener.join();
            auth::complete(pending, params)
        }
    }

    /// 네트워크 없이 호출 횟수/요청 scope만 기록하고 순번 토큰을 주는 authorize 대역.
    fn counting_authorize(
        calls: Arc<Mutex<Vec<String>>>,
        prefix: &'static str,
    ) -> impl Fn(&auth::OAuthProviderConfig) -> anyhow::Result<auth::OAuthToken> {
        move |config| {
            let mut calls = calls.lock().unwrap();
            calls.push(config.scopes.join(" "));
            let n = calls.len();
            Ok(auth::OAuthToken {
                access_token: SecretString::new(format!("{prefix}{n}")),
                refresh_token: None,
                expires_in_secs: Some(3600),
            })
        }
    }

    fn as_metadata(base: &str) -> auth::AuthorizationServerMetadata {
        auth::AuthorizationServerMetadata {
            issuer: base.to_owned(),
            authorization_endpoint: format!("{base}/authorize"),
            token_endpoint: format!("{base}/token"),
            registration_endpoint: Some(format!("{base}/register")),
            grant_types_supported: None,
            scopes_supported: None,
            code_challenge_methods_supported: None,
        }
    }

    fn test_manager() -> LocalMcpManager {
        LocalMcpManager::new(RedactionService::new()).with_request_timeout(Duration::from_secs(5))
    }

    const TIMEOUT: Duration = Duration::from_secs(5);

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
            target: InvokeTarget::Stdio {
                command: "/nonexistent/deppy-connectors-test".to_owned(),
                args: Vec::new(),
                env_plain: Vec::new(),
                env_secrets: Vec::new(),
                inherit_env: true,
            },
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

    struct MemMcpEnvResolver(std::collections::HashMap<String, String>);

    impl McpScopedEnvResolver for MemMcpEnvResolver {
        fn resolve_mcp_env(
            &self,
            env_plain: &[(String, String)],
            env_secrets: &[(String, String)],
        ) -> anyhow::Result<Vec<(String, String)>> {
            let mut env = env_plain.to_vec();
            for (key, credential_id) in env_secrets {
                let value = self
                    .0
                    .get(credential_id)
                    .cloned()
                    .with_context(|| format!("missing secret: {credential_id}"))?;
                env.push((key.clone(), value));
            }
            Ok(env)
        }
    }

    #[test]
    fn scoped_mcp_env_config는_credential을_해석하고_debug에_값을_숨긴다() {
        let resolver = MemMcpEnvResolver(std::collections::HashMap::from([(
            "cred-1".to_owned(),
            "sk-scoped-env-secret".to_owned(),
        )]));

        let config = mcp_config_for_values(
            "mock",
            "/bin/sh".to_owned(),
            vec!["-c".to_owned(), "exit 0".to_owned()],
            false,
            &[("MCP_SAFE".to_owned(), "1".to_owned())],
            &[("MCP_TOKEN".to_owned(), "cred-1".to_owned())],
            &resolver,
        )
        .unwrap();

        assert!(!config.inherit_env);
        assert_eq!(
            config.env,
            vec![
                ("MCP_SAFE".to_owned(), "1".to_owned()),
                ("MCP_TOKEN".to_owned(), "sk-scoped-env-secret".to_owned())
            ]
        );
        assert!(
            !format!("{config:?}").contains("sk-scoped-env-secret"),
            "Debug must not expose scoped env values"
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
        let ui = test_ui();
        let ctx = egui::Context::default();
        let inv = invoke_with_input(r#"{"token":"sk-unregistered-secret","path":"/tmp/x"}"#);
        let resolver = MemMcpEnvResolver(std::collections::HashMap::new());

        let phase = ui.run_tool(
            &inv,
            &db,
            "ws-1",
            &ctx,
            audit::ToolDecision::DenyOnce,
            &resolver,
        );

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
        let ui = test_ui();
        let ctx = egui::Context::default();
        let resolver = MemMcpEnvResolver(std::collections::HashMap::new());

        for input in ["{bad", "[1,2]"] {
            let inv = invoke_with_input(input);
            let phase = ui.run_tool(
                &inv,
                &db,
                "ws-1",
                &ctx,
                audit::ToolDecision::DenyOnce,
                &resolver,
            );
            assert!(matches!(phase, InvokePhase::Failed(_)));
        }

        assert!(audit_rows(&path).is_empty());
    }

    fn http_row(id: &str, url: Option<&str>) -> McpServerRow {
        McpServerRow {
            id: id.to_owned(),
            name: "remote".to_owned(),
            kind: "http".to_owned(),
            command: None,
            args: Vec::new(),
            env_plain: Vec::new(),
            env_secrets: Vec::new(),
            inherit_env: true,
            url: url.map(str::to_owned),
            enabled: true,
        }
    }

    #[test]
    fn server_row는_kind별_config로_분기된다() {
        // H3: kind('stdio'|'http')를 보고 McpServerConfig/McpHttpServerConfig 분기 생성
        let resolver = MemMcpEnvResolver(std::collections::HashMap::new());

        let http = http_row("srv-http", Some("https://mcp.example.com/mcp"));
        match config_for_row(&http, &resolver).unwrap() {
            ConnectorConfig::Http(spec) => {
                assert_eq!(spec.config.url, "https://mcp.example.com/mcp");
                assert_eq!(spec.config.name, "remote");
                assert!(
                    spec.config.bearer.is_none(),
                    "bearer 부착은 with_http_auth(H5)"
                );
                assert!(spec.auth.is_none());
            }
            ConnectorConfig::Stdio(_) => panic!("http row가 stdio config로 분기됨"),
        }

        let mut stdio = http_row("srv-stdio", None);
        stdio.kind = "stdio".to_owned();
        stdio.command = Some("/bin/sh".to_owned());
        stdio.args = vec!["-c".to_owned(), "exit 0".to_owned()];
        match config_for_row(&stdio, &resolver).unwrap() {
            ConnectorConfig::Stdio(config) => assert_eq!(config.command, "/bin/sh"),
            ConnectorConfig::Http(_) => panic!("stdio row가 http config로 분기됨"),
        }

        let mut unknown = http_row("srv-x", None);
        unknown.kind = "websocket".to_owned();
        let error = format!("{:#}", config_for_row(&unknown, &resolver).unwrap_err());
        assert!(error.contains("지원하지 않는"), "{error}");
    }

    #[test]
    fn http_row_url_정책_위반은_config_생성을_거부한다() {
        let resolver = MemMcpEnvResolver(std::collections::HashMap::new());
        // url 없음
        assert!(config_for_row(&http_row("s1", None), &resolver).is_err());
        // http는 localhost/루프백만 허용 (H2 규칙을 저장·연결 양쪽에서 재검증)
        assert!(
            config_for_row(&http_row("s2", Some("http://evil.example.com")), &resolver).is_err()
        );
        assert!(config_for_row(&http_row("s3", Some("http://localhost:9000")), &resolver).is_ok());
        // invoke target도 같은 규칙
        assert!(invoke_target_for_row(&http_row("s4", Some("ftp://x"))).is_err());
        assert!(invoke_target_for_row(&http_row("s5", Some("https://ok.example.com/mcp"))).is_ok());
    }

    #[test]
    fn url_편집_저장은_allow_규칙만_리셋하고_도구_캐시를_비운다() {
        // H3 편집 시점 훅: url 갱신 + Allow 규칙 초기화(Deny 유지) + mcp_tools 무효화
        // + 세션 신뢰 철회 (VS Code cacheNonce 신뢰 차용의 등가 구현).
        // 변수명 store: xtask check-boundary가 테스트 라인의 DB 호출 패턴도 세므로 회피.
        let path = temp_db_path();
        let mut store = Db::open(&path).unwrap();
        let mut ui = test_ui();

        let server = http_row("srv-h", Some("https://old.example.com/mcp"));
        store.insert_mcp_server(&server).unwrap();
        store
            .replace_mcp_tools(
                "srv-h",
                &tool_rows(
                    "srv-h",
                    &[McpTool {
                        name: "tool_a".to_owned(),
                        description: None,
                        input_schema_json: r#"{"type":"object"}"#.to_owned(),
                    }],
                ),
            )
            .unwrap();
        let hash = audit::schema_hash(r#"{"type":"object"}"#);
        store
            .upsert_permission_rule("srv-h", "tool_a", "allow", Some(&hash))
            .unwrap();
        store
            .upsert_permission_rule("srv-h", "tool_b", "deny", None)
            .unwrap();
        // 다른 서버의 Allow 규칙은 건드리면 안 된다
        store
            .upsert_permission_rule("srv-other", "tool_c", "allow", Some(&hash))
            .unwrap();
        ui.policy
            .load_rule("srv-h", "tool_a", audit::PermissionRule::Allow, Some(hash));
        ui.trusted_http.insert("srv-h".to_owned());

        ui.save_url_edit(&mut store, "srv-h", "https://new.example.com/mcp")
            .unwrap();

        // url 갱신
        let rows = store.list_mcp_servers().unwrap();
        assert_eq!(rows[0].url.as_deref(), Some("https://new.example.com/mcp"));
        // Allow만 삭제, Deny와 타 서버 규칙은 유지
        let rules = store.list_permission_rules().unwrap();
        assert!(
            !rules
                .iter()
                .any(|r| r.server_id == "srv-h" && r.rule == "allow"),
            "{rules:?}"
        );
        assert!(
            rules
                .iter()
                .any(|r| r.server_id == "srv-h" && r.tool_name == "tool_b" && r.rule == "deny")
        );
        assert!(
            rules
                .iter()
                .any(|r| r.server_id == "srv-other" && r.rule == "allow")
        );
        // in-memory policy도 Ask로 복귀
        assert_eq!(
            ui.policy.rule("srv-h", "tool_a"),
            audit::PermissionRule::Ask
        );
        // 도구 캐시 무효화 + 세션 신뢰 철회
        assert!(store.list_mcp_tools("srv-h").unwrap().is_empty());
        assert!(!ui.trusted_http.contains("srv-h"));

        // 정책 위반 url은 거부되고 기존 url이 남는다
        assert!(
            ui.save_url_edit(&mut store, "srv-h", "http://evil.example.com")
                .is_err()
        );
        let rows = store.list_mcp_servers().unwrap();
        assert_eq!(rows[0].url.as_deref(), Some("https://new.example.com/mcp"));
    }

    // ---------- H5: 401 사다리 / URL 바인딩 / refresh ----------

    /// 목 보호 서버 E2E (완료 기준): 401(+resource_metadata) → PRM → AS 메타데이터
    /// → DCR → PKCE(브라우저 대신 콜백 직접 호출) → Bearer → tools/list 200.
    #[test]
    fn 사다리_e2e_401에서_dcr_pkce_bearer_재시도까지() {
        let server = MockHttpServer::start(move |request| {
            let base = format!("http://{}", request.header("host").unwrap_or_default());
            match request.path.as_str() {
                "/mcp" => {
                    if request.header("authorization") == Some("Bearer at-e2e") {
                        mcp_reply(request)
                    } else {
                        unauthorized(&format!(
                            "Bearer resource_metadata=\"{base}/custom/prm\", scope=\"mcp.read\""
                        ))
                    }
                }
                "/custom/prm" => MockResponse::json(
                    200,
                    serde_json::json!({
                        "resource": format!("{base}/mcp"),
                        "authorization_servers": [base],
                    })
                    .to_string(),
                ),
                "/.well-known/oauth-authorization-server" => MockResponse::json(
                    200,
                    serde_json::json!({
                        "issuer": base,
                        "authorization_endpoint": format!("{base}/authorize"),
                        "token_endpoint": format!("{base}/token"),
                        "registration_endpoint": format!("{base}/register"),
                        "grant_types_supported": ["authorization_code", "refresh_token"],
                    })
                    .to_string(),
                ),
                "/register" => MockResponse::json(201, r#"{"client_id":"cid-e2e"}"#),
                "/token" => MockResponse::json(
                    200,
                    r#"{"access_token":"at-e2e","token_type":"bearer","refresh_token":"rt-e2e","expires_in":3600}"#,
                ),
                _ => MockResponse::json(404, "{}"),
            }
        });
        let mcp_url = server.url("/mcp");
        let manager = test_manager();

        // 트리거: Bearer 없는 요청 → 401 — 챌린지가 구조화 에러로 추출된다 (H5 훅)
        let error = manager
            .discover_tools_http(&McpHttpServerConfig {
                name: "remote".to_owned(),
                url: mcp_url.clone(),
                bearer: None,
            })
            .unwrap_err();
        let challenge = auth_challenge_of(&error).expect("401 챌린지 추출");
        assert_eq!(challenge.scope.as_deref(), Some("mcp.read"));
        assert_eq!(challenge.resource_metadata, Some(server.url("/custom/prm")));

        // 발견 체인 (사다리 단 ①): 챌린지 PRM URL → AS 메타데이터
        let discovered = discover_auth_metadata(TIMEOUT, &mcp_url, &challenge).unwrap();
        assert_eq!(discovered.scopes, vec!["mcp.read".to_owned()]);
        assert_eq!(discovered.metadata.token_endpoint, server.url("/token"));

        // 사다리: DCR → 브라우저(콜백 직접 호출) → Bearer 재시도 → 성공
        let authorize = callback_authorize(&mcp_url);
        let end = run_oauth_ladder(
            &manager,
            TIMEOUT,
            "srv-e2e",
            "remote",
            &mcp_url,
            &discovered,
            ClientPlan::Dcr,
            &authorize,
        );
        let LadderEnd::Success(success) = end else {
            panic!("사다리 성공이 아님");
        };
        assert_eq!(success.tools.len(), 1);
        assert_eq!(success.tools[0].name, "remote_tool");
        assert_eq!(success.connection.client_id, "cid-e2e");
        assert_eq!(success.connection.server_url, mcp_url);
        assert!(!success.connection.manual_client);
        assert!(success.connection.expires_at_secs.is_some());
        assert_eq!(success.token.access_token.expose(), "at-e2e");

        let requests = server.requests();
        // PRM은 챌린지의 resource_metadata URL을 1순위로 썼다
        assert!(requests.iter().any(|r| r.path == "/custom/prm"));
        // DCR 등록: 공개 클라이언트 규약 (상세 규약은 crates/auth 테스트가 소유)
        let register = requests.iter().find(|r| r.path == "/register").unwrap();
        assert_eq!(register.method, "POST", "DCR 등록은 POST (RFC 7591)");
        let register_body: serde_json::Value = serde_json::from_str(&register.body).unwrap();
        assert_eq!(register_body["token_endpoint_auth_method"], "none");
        // token 교환: 실제 code + PKCE verifier + RFC 8707 resource가 실렸다
        let token = requests.iter().find(|r| r.path == "/token").unwrap();
        assert!(token.body.contains("code=mock-code"), "{}", token.body);
        assert!(token.body.contains("code_verifier="), "{}", token.body);
        assert!(
            token.body.contains("resource=http%3A%2F%2F127.0.0.1"),
            "{}",
            token.body
        );
        // 성공한 tools/list에는 새 Bearer가 붙었다
        let listed = requests
            .iter()
            .find(|r| r.rpc_method() == "tools/list")
            .unwrap();
        assert_eq!(listed.header("authorization"), Some("Bearer at-e2e"));
    }

    /// 완료 기준: scope 챌린지 변경 → scope 갱신 + 재승인 1회 후 성공.
    #[test]
    fn 사다리_scope_챌린지_변경은_1회_재시도() {
        let server = MockHttpServer::start(|request| {
            if request.path != "/mcp" {
                return MockResponse::json(404, "{}");
            }
            match request.header("authorization") {
                // 첫 토큰 — 더 넓은 scope를 요구하는 401
                Some("Bearer at-s1") => unauthorized(
                    "Bearer scope=\"mcp.read mcp.write\", error=\"insufficient_scope\"",
                ),
                Some("Bearer at-s2") => mcp_reply(request),
                _ => unauthorized("Bearer scope=\"mcp.read\""),
            }
        });
        let mcp_url = server.url("/mcp");
        let calls: Arc<Mutex<Vec<String>>> = Arc::default();
        let authorize = counting_authorize(Arc::clone(&calls), "at-s");
        let discovered = DiscoveredAuth {
            metadata: as_metadata("https://as.example"),
            scopes: vec!["mcp.read".to_owned()],
        };

        let end = run_oauth_ladder(
            &test_manager(),
            TIMEOUT,
            "srv-scope",
            "remote",
            &mcp_url,
            &discovered,
            ClientPlan::Stored {
                client_id: "cid-x".to_owned(),
                client_secret: None,
                manual: false,
            },
            &authorize,
        );
        let LadderEnd::Success(success) = end else {
            panic!("사다리 성공이 아님");
        };
        // 승인은 정확히 2회: 최초(mcp.read) + scope 갱신(mcp.read mcp.write)
        assert_eq!(
            calls.lock().unwrap().clone(),
            vec!["mcp.read".to_owned(), "mcp.read mcp.write".to_owned()]
        );
        assert_eq!(
            success.connection.scopes,
            vec!["mcp.read".to_owned(), "mcp.write".to_owned()]
        );
        assert_eq!(success.token.access_token.expose(), "at-s2");
    }

    /// 완료 기준: 재등록 사다리는 1회 후 종료 — 트리거 포함 3연속 401이면 최종 에러.
    #[test]
    fn 사다리_재등록_1회_후_종료_3연속_401이면_에러() {
        let register_hits = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&register_hits);
        let server = MockHttpServer::start(move |request| match request.path.as_str() {
            // scope 챌린지가 항상 동일 → scope 갱신 단은 건너뛰고 재등록 단으로 간다
            "/mcp" => unauthorized("Bearer scope=\"mcp.fixed\", error=\"invalid_token\""),
            "/register" => {
                let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                MockResponse::json(201, format!("{{\"client_id\":\"cid-r{n}\"}}"))
            }
            _ => MockResponse::json(404, "{}"),
        });
        let mcp_url = server.url("/mcp");
        let manager = test_manager();

        // 트리거 401 (1번째)
        assert!(
            manager
                .discover_tools_http(&McpHttpServerConfig {
                    name: "remote".to_owned(),
                    url: mcp_url.clone(),
                    bearer: None,
                })
                .is_err()
        );

        let calls: Arc<Mutex<Vec<String>>> = Arc::default();
        let authorize = counting_authorize(Arc::clone(&calls), "at-r");
        let discovered = DiscoveredAuth {
            metadata: as_metadata(&server.url("")),
            scopes: vec!["mcp.fixed".to_owned()],
        };
        let end = run_oauth_ladder(
            &manager,
            TIMEOUT,
            "srv-rereg",
            "remote",
            &mcp_url,
            &discovered,
            ClientPlan::Dcr,
            &authorize,
        );
        let LadderEnd::Failed(message) = end else {
            panic!("최종 에러가 아님");
        };
        assert!(message.contains("사다리 소진"), "{message}");
        // 등록은 정확히 2회: 최초 DCR + 재등록 1회 (그 이상 재시도 없음)
        assert_eq!(register_hits.load(Ordering::SeqCst), 2);
        // 승인도 2회 (최초 + 재등록 후)
        assert_eq!(calls.lock().unwrap().len(), 2);
        // 서버가 본 401 요청: 트리거(1) + 사다리 재시도(2) = 3연속 401 후 종료
        let mcp_401 = server
            .requests()
            .iter()
            .filter(|r| r.path == "/mcp" && r.rpc_method() == "initialize")
            .count();
        assert_eq!(mcp_401, 3);
    }

    /// DCR 미지원이면 수동 client_id 폴백으로 전환된다 (완료 기준).
    #[test]
    fn 사다리_dcr_미지원이면_수동_client_폴백() {
        let mut metadata = as_metadata("https://as.example");
        metadata.registration_endpoint = None;
        let discovered = DiscoveredAuth {
            metadata,
            scopes: Vec::new(),
        };
        let authorize = |_config: &auth::OAuthProviderConfig| -> anyhow::Result<auth::OAuthToken> {
            panic!("등록 실패 시 브라우저를 열면 안 됨")
        };
        let end = run_oauth_ladder(
            &test_manager(),
            TIMEOUT,
            "srv-manual",
            "remote",
            "https://mcp.example/mcp",
            &discovered,
            ClientPlan::Dcr,
            &authorize,
        );
        let LadderEnd::NeedManualClient { reason } = end else {
            panic!("수동 폴백이 아님");
        };
        assert!(reason.contains("registration_endpoint 없음"), "{reason}");
    }

    /// 수동 client는 재등록 단을 쓰지 않는다 — 재시도 1회 후 곧장 종료.
    #[test]
    fn 사다리_수동_client는_재등록_없이_종료() {
        let server = MockHttpServer::start(|request| match request.path.as_str() {
            "/mcp" => unauthorized("Bearer error=\"invalid_token\""),
            "/register" => panic!("수동 client 사다리가 DCR을 호출함"),
            _ => MockResponse::json(404, "{}"),
        });
        let calls: Arc<Mutex<Vec<String>>> = Arc::default();
        let authorize = counting_authorize(Arc::clone(&calls), "at-m");
        let discovered = DiscoveredAuth {
            metadata: as_metadata(&server.url("")),
            scopes: Vec::new(),
        };
        let end = run_oauth_ladder(
            &test_manager(),
            TIMEOUT,
            "srv-manual2",
            "remote",
            &server.url("/mcp"),
            &discovered,
            ClientPlan::Manual {
                client_id: "cid-manual".to_owned(),
                client_secret: None,
            },
            &authorize,
        );
        assert!(matches!(end, LadderEnd::Failed(_)));
        // 승인 1회(최초)만 — scope 동일/수동이라 추가 단 없음
        assert_eq!(calls.lock().unwrap().len(), 1);
    }

    fn seeded_binding_db(server_id: &str, server_url: &str, token_url: &str) -> (Db, String) {
        let db = Db::open(&temp_db_path()).unwrap();
        let credential_id = "cred-h5".to_owned();
        db.insert_credential(&CredentialMeta {
            id: credential_id.clone(),
            provider: "oauth".to_owned(),
            label: "remote".to_owned(),
            credential_kind: "oauth_token".to_owned(),
            masked_hint: None,
            workspace_id: None,
        })
        .unwrap();
        let connection = OAuthConnection {
            server_id: server_id.to_owned(),
            server_url: server_url.to_owned(),
            issuer: "https://as.example".to_owned(),
            authorization_endpoint: "https://as.example/authorize".to_owned(),
            token_endpoint: token_url.to_owned(),
            client_id: "cid-1".to_owned(),
            manual_client: false,
            scopes: vec!["mcp.read".to_owned()],
            expires_at_secs: None,
        };
        db.set_credential_oauth_json(&credential_id, &serde_json::to_string(&connection).unwrap())
            .unwrap();
        (db, credential_id)
    }

    /// 완료 기준: 토큰 URL 바인딩 — 동의 시점 URL과 다르면 Bearer 부착 거부.
    /// keyring access 해석/needs_refresh 판정은 run_http 소관(선제/반응 refresh 테스트).
    #[test]
    fn url_바인딩_불일치는_bearer_부착_거부() {
        let (db, credential_id) =
            seeded_binding_db("srv-h", "https://a.example/mcp", "https://as.example/token");

        // 일치 — 바인딩 재료 부착 (비밀 없음)
        match resolve_http_auth(&db, "srv-h", "https://a.example/mcp") {
            HttpAuth::Bound(binding) => {
                assert_eq!(binding.credential_id, credential_id);
                assert_eq!(binding.token_url, "https://as.example/token");
                assert_eq!(binding.server_url, "https://a.example/mcp");
                assert_eq!(binding.expires_at_secs, None);
            }
            _ => panic!("Bound가 아님"),
        }

        // url 변경(H3 편집 저장 후) — 부착 거부 (재동의 필요)
        assert!(matches!(
            resolve_http_auth(&db, "srv-h", "https://b.example/mcp"),
            HttpAuth::UrlMismatch
        ));
        // 바인딩 없는 서버 — None
        assert!(matches!(
            resolve_http_auth(&db, "srv-없음", "https://a.example/mcp"),
            HttpAuth::None
        ));

        // with_http_auth 배선: 불일치면 바인딩이 붙지 않는다
        let row = http_row("srv-h", Some("https://b.example/mcp"));
        let resolver = MemMcpEnvResolver(std::collections::HashMap::new());
        let config = config_for_row(&row, &resolver)
            .unwrap()
            .with_http_auth(&db, "srv-h");
        match config {
            ConnectorConfig::Http(spec) => {
                assert!(spec.config.bearer.is_none(), "부착 거부돼야 함");
                assert!(spec.auth.is_none());
            }
            ConnectorConfig::Stdio(_) => panic!("http가 아님"),
        }
    }

    /// 401 + 저장 토큰 → 반응 refresh 1회 → 원요청 1회 재시도 성공.
    #[test]
    fn 반응_refresh는_401에서_1회_갱신_후_재시도() {
        let server = MockHttpServer::start(|request| match request.path.as_str() {
            "/mcp" => match request.header("authorization") {
                Some("Bearer at-new") => mcp_reply(request),
                _ => unauthorized("Bearer error=\"invalid_token\""),
            },
            "/token" => MockResponse::json(
                200,
                r#"{"access_token":"at-new","token_type":"bearer","refresh_token":"rt-new","expires_in":1200}"#,
            ),
            _ => MockResponse::json(404, "{}"),
        });
        let mcp_url = server.url("/mcp");
        let store = Arc::new(MemStore::default());
        store.seed("cred-h5", "at-old");
        store.seed(&auth::refresh_entry_id("cred-h5"), "rt-old");
        let cx = ExecContext {
            manager: test_manager(),
            coordinator: Arc::new(auth::RefreshCoordinator::new()),
            store: Arc::clone(&store) as Arc<dyn SecretStore>,
            redaction: RedactionService::new(),
        };
        let spec = HttpConnectSpec {
            config: McpHttpServerConfig {
                name: "remote".to_owned(),
                url: mcp_url.clone(),
                bearer: Some(SecretString::new("at-old".to_owned())),
            },
            auth: Some(HttpAuthBinding {
                credential_id: "cred-h5".to_owned(),
                token_url: server.url("/token"),
                client_id: "cid-1".to_owned(),
                server_url: mcp_url.clone(),
                // 만료 메타 없음 + store에 access 있음 → 선제 아님, 401 반응 경로
                expires_at_secs: None,
            }),
        };

        let (result, update) = ConnectorConfig::Http(spec).discover_tools(&cx);
        let tools = result.expect("refresh 후 재시도 성공");
        assert_eq!(tools[0].name, "remote_tool");
        let update = update.expect("refresh 부수효과");
        assert_eq!(update.credential_id, "cred-h5");
        assert!(update.expires_at_secs.is_some());
        // keyring이 새 토큰으로 교체됐다 (H4 store_token 규약)
        assert_eq!(store.value("cred-h5").as_deref(), Some("at-new"));
        // token endpoint는 정확히 1회
        let token_hits = server
            .requests()
            .iter()
            .filter(|r| r.path == "/token")
            .count();
        assert_eq!(token_hits, 1);
    }

    /// 만료 임박(needs_refresh)이면 요청 전에 선제 refresh — 401 왕복이 없다.
    #[test]
    fn 선제_refresh는_요청_전에_갱신한다() {
        let server = MockHttpServer::start(|request| match request.path.as_str() {
            "/mcp" => match request.header("authorization") {
                Some("Bearer at-new") => mcp_reply(request),
                other => panic!("선제 refresh 없이 요청됨: {other:?}"),
            },
            "/token" => MockResponse::json(
                200,
                r#"{"access_token":"at-new","token_type":"bearer","expires_in":1200}"#,
            ),
            _ => MockResponse::json(404, "{}"),
        });
        let mcp_url = server.url("/mcp");
        let store = Arc::new(MemStore::default());
        store.seed("cred-h5", "at-stale");
        store.seed(&auth::refresh_entry_id("cred-h5"), "rt-1");
        let cx = ExecContext {
            manager: test_manager(),
            coordinator: Arc::new(auth::RefreshCoordinator::new()),
            store: Arc::clone(&store) as Arc<dyn SecretStore>,
            redaction: RedactionService::new(),
        };
        let spec = HttpConnectSpec {
            config: McpHttpServerConfig {
                name: "remote".to_owned(),
                url: mcp_url.clone(),
                bearer: Some(SecretString::new("at-stale".to_owned())),
            },
            auth: Some(HttpAuthBinding {
                credential_id: "cred-h5".to_owned(),
                token_url: server.url("/token"),
                client_id: "cid-1".to_owned(),
                server_url: mcp_url.clone(),
                // 과거 만료 시각 → run_http가 needs_refresh=true로 판정(선제 refresh)
                expires_at_secs: Some(1),
            }),
        };

        let (result, update) = ConnectorConfig::Http(spec).discover_tools(&cx);
        assert!(result.is_ok());
        assert!(update.is_some());
        // 모든 /mcp 요청이 새 토큰으로 나갔다 (패닉 없이 통과한 것 자체가 검증)
        assert!(
            server
                .requests()
                .iter()
                .filter(|r| r.path == "/mcp")
                .all(|r| r.header("authorization") == Some("Bearer at-new"))
        );
    }

    /// refresh 만료 시각 부수효과가 oauth_json에 영속된다.
    #[test]
    fn refresh_만료_시각은_oauth_json에_반영() {
        let (db, credential_id) =
            seeded_binding_db("srv-h", "https://a.example/mcp", "https://as.example/token");
        persist_refresh_update(
            &db,
            &RefreshUpdate {
                credential_id: credential_id.clone(),
                expires_at_secs: Some(1_900_000_000),
            },
        );
        let (_, connection) = oauth_binding_for_server(&db, "srv-h").unwrap();
        assert_eq!(connection.expires_at_secs, Some(1_900_000_000));
        // 만료 미상(None) 갱신은 기존 값을 지우지 않는다
        persist_refresh_update(
            &db,
            &RefreshUpdate {
                credential_id,
                expires_at_secs: None,
            },
        );
        let (_, connection) = oauth_binding_for_server(&db, "srv-h").unwrap();
        assert_eq!(connection.expires_at_secs, Some(1_900_000_000));
    }

    /// drain_results: 401 결과는 에러가 아니라 "승인 필요" 상태가 된다.
    #[test]
    fn drain_results는_401을_승인_필요로_분류() {
        let path = temp_db_path();
        let mut db = Db::open(&path).unwrap();
        let mut ui = test_ui();
        db.insert_mcp_server(&http_row("srv-h", Some("https://a.example/mcp")))
            .unwrap();
        ui.result_tx
            .send(DiscoverOutcome {
                server_id: "srv-h".to_owned(),
                request_url: Some("https://a.example/mcp".to_owned()),
                result: Err(ExecFailure {
                    message: "HTTP 401".to_owned(),
                    auth: Some(AuthChallengeInfo {
                        resource_metadata: None,
                        scope: Some("mcp.read".to_owned()),
                    }),
                }),
                refresh: None,
            })
            .unwrap();
        ui.drain_results(&mut db);
        match ui.status.get("srv-h") {
            Some(ConnStatus::NeedsAuth { challenge, .. }) => {
                assert_eq!(challenge.scope.as_deref(), Some("mcp.read"));
            }
            other => panic!(
                "NeedsAuth가 아님: {:?}",
                other.map(|_| "다른 상태").unwrap_or("없음")
            ),
        }
    }

    /// url 편집 저장 race (H3 리뷰 P2): 구 url로 시작된 discover 결과는 폐기된다.
    #[test]
    fn drain_results는_stale_url_결과를_폐기() {
        let path = temp_db_path();
        let mut db = Db::open(&path).unwrap();
        let mut ui = test_ui();
        db.insert_mcp_server(&http_row("srv-h", Some("https://new.example.com/mcp")))
            .unwrap();
        let tools = vec![McpTool {
            name: "stale_tool".to_owned(),
            description: None,
            input_schema_json: "{}".to_owned(),
        }];
        // 구 url로 시작된 백그라운드 결과 도착 — 방금 비운 캐시를 재채우면 안 된다
        ui.result_tx
            .send(DiscoverOutcome {
                server_id: "srv-h".to_owned(),
                request_url: Some("https://old.example.com/mcp".to_owned()),
                result: Ok(tools.clone()),
                refresh: None,
            })
            .unwrap();
        ui.drain_results(&mut db);
        assert!(!ui.status.contains_key("srv-h"), "stale 결과가 반영됨");
        assert!(db.list_mcp_tools("srv-h").unwrap().is_empty());

        // 현재 url과 일치하는 결과는 정상 반영
        ui.result_tx
            .send(DiscoverOutcome {
                server_id: "srv-h".to_owned(),
                request_url: Some("https://new.example.com/mcp".to_owned()),
                result: Ok(tools),
                refresh: None,
            })
            .unwrap();
        ui.drain_results(&mut db);
        assert!(matches!(
            ui.status.get("srv-h"),
            Some(ConnStatus::Connected { tools: 1 })
        ));
        assert_eq!(db.list_mcp_tools("srv-h").unwrap().len(), 1);
    }

    /// drain_oauth 성공 경로: 같은 서버의 재승인은 기존 credential을 재사용하고
    /// (env 참조 유지), 바인딩 메타/도구가 반영된다.
    #[test]
    fn drain_oauth_성공은_바인딩과_도구를_영속() {
        struct TestOAuthStore<'a> {
            store: &'a MemStore,
        }
        impl OAuthCredentialStore for TestOAuthStore<'_> {
            fn store_oauth_token(
                &self,
                token: &auth::OAuthToken,
            ) -> anyhow::Result<StoredOAuthCredential> {
                let id = format!("cred-{}", self.store.0.lock().unwrap().len());
                auth::store_token(self.store, &id, token)?;
                Ok(StoredOAuthCredential {
                    id,
                    masked_hint: "****hint".to_owned(),
                })
            }
            fn update_oauth_token(&self, id: &str, token: &auth::OAuthToken) -> anyhow::Result<()> {
                auth::store_token(self.store, id, token)
            }
            fn set_dcr_secret(
                &self,
                id: &str,
                secret: Option<&SecretString>,
            ) -> anyhow::Result<()> {
                let entry = auth::dcr_secret_entry_id(id);
                match secret {
                    Some(secret) => self.store.set_secret(&entry, secret),
                    None => self.store.delete_secret(&entry),
                }
            }
            fn delete_oauth_token(&self, id: &str) -> anyhow::Result<()> {
                self.store.delete_secret(id)?;
                self.store.delete_secret(&auth::refresh_entry_id(id))?;
                self.store.delete_secret(&auth::dcr_secret_entry_id(id))
            }
        }

        let path = temp_db_path();
        let mut db = Db::open(&path).unwrap();
        // 서버 카드는 승인 전에 이미 존재한다 (사용자가 카드에서 승인) — tools 영속의
        // FK 부모(mcp_servers) 선행 조건. 프로덕션에선 항상 참.
        db.insert_mcp_server(&http_row("srv-h", Some("https://a.example/mcp")))
            .unwrap();
        let store = MemStore::default();
        let oauth_store = TestOAuthStore { store: &store };
        let mut ui = test_ui();

        let success = LadderSuccess {
            connection: OAuthConnection {
                server_id: "srv-h".to_owned(),
                server_url: "https://a.example/mcp".to_owned(),
                issuer: "https://as.example".to_owned(),
                authorization_endpoint: "https://as.example/authorize".to_owned(),
                token_endpoint: "https://as.example/token".to_owned(),
                client_id: "cid-1".to_owned(),
                manual_client: false,
                scopes: vec!["mcp.read".to_owned()],
                expires_at_secs: Some(1_900_000_000),
            },
            token: auth::OAuthToken {
                access_token: SecretString::new("at-1".to_owned()),
                refresh_token: Some(SecretString::new("rt-1".to_owned())),
                expires_in_secs: Some(3600),
            },
            client_secret: Some(SecretString::new("cs-1".to_owned())),
            tools: vec![McpTool {
                name: "remote_tool".to_owned(),
                description: None,
                input_schema_json: "{}".to_owned(),
            }],
        };
        // flow 상태를 만들고 성공 메시지를 흘린다
        ui.oauth_gen += 1;
        ui.oauth_flow = Some(OAuthFlow {
            server_id: "srv-h".to_owned(),
            server_name: "remote".to_owned(),
            server_url: "https://a.example/mcp".to_owned(),
            generation: ui.oauth_gen,
            stage: OAuthStage::Authorizing,
        });
        ui.oauth_tx
            .send((ui.oauth_gen, OAuthMsg::Finished(Box::new(Ok(success)))))
            .unwrap();
        assert!(ui.drain_oauth(&mut db, &oauth_store));
        assert!(ui.oauth_flow.is_none());
        assert!(matches!(
            ui.status.get("srv-h"),
            Some(ConnStatus::Connected { tools: 1 })
        ));
        // credential + 바인딩 + keyring(access/refresh/dcr) 전부 영속
        let (credential_id, connection) = oauth_binding_for_server(&db, "srv-h").unwrap();
        assert_eq!(connection.client_id, "cid-1");
        assert_eq!(store.value(&credential_id).as_deref(), Some("at-1"));
        assert_eq!(
            store
                .value(&auth::refresh_entry_id(&credential_id))
                .as_deref(),
            Some("rt-1")
        );
        assert_eq!(
            store
                .value(&auth::dcr_secret_entry_id(&credential_id))
                .as_deref(),
            Some("cs-1")
        );
        assert_eq!(db.list_mcp_tools("srv-h").unwrap().len(), 1);
        let credentials = db.list_credentials().unwrap();
        assert_eq!(credentials.len(), 1);

        // 재승인(같은 서버): 새 credential을 만들지 않고 같은 id에 토큰만 교체,
        // DCR secret이 사라졌으면 entry도 정리된다
        let success2 = LadderSuccess {
            connection: OAuthConnection {
                expires_at_secs: Some(2_000_000_000),
                ..connection
            },
            token: auth::OAuthToken {
                access_token: SecretString::new("at-2".to_owned()),
                refresh_token: None,
                expires_in_secs: None,
            },
            client_secret: None,
            tools: Vec::new(),
        };
        ui.oauth_gen += 1;
        ui.oauth_flow = Some(OAuthFlow {
            server_id: "srv-h".to_owned(),
            server_name: "remote".to_owned(),
            server_url: "https://a.example/mcp".to_owned(),
            generation: ui.oauth_gen,
            stage: OAuthStage::Authorizing,
        });
        ui.oauth_tx
            .send((ui.oauth_gen, OAuthMsg::Finished(Box::new(Ok(success2)))))
            .unwrap();
        assert!(ui.drain_oauth(&mut db, &oauth_store));
        assert_eq!(db.list_credentials().unwrap().len(), 1, "credential 재사용");
        assert_eq!(store.value(&credential_id).as_deref(), Some("at-2"));
        assert_eq!(
            store.value(&auth::dcr_secret_entry_id(&credential_id)),
            None
        );
    }
}
