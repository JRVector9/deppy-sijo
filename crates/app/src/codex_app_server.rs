//! Stdio JSON-RPC client for `codex app-server`.
//!
//! The App Server is intentionally separate from the PTY terminal runtime. It
//! speaks newline-delimited JSON-RPC and streams typed thread/turn/item events,
//! which lets the UI render a real result table without altering terminal bytes.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

use anyhow::Context;
use serde_json::{Value, json};

use crate::agent_session::{
    AgentApproval, AgentApprovalDecision, AgentApprovalKind, AgentItem, AgentSessionEvent,
    AgentSessionId, AgentSkillSelection, AgentThreadStatus,
};

const MAX_BUFFERED_THREAD_STATUSES: usize = 64;
const COMMAND_QUEUE_CAPACITY: usize = 8;
const EVENT_QUEUE_CAPACITY: usize = 64;
const INCOMING_QUEUE_CAPACITY: usize = 16;
const INCOMING_DRAIN_PER_TICK: usize = 64;
const REPLY_QUEUE_CAPACITY: usize = 1;
const PREINITIALIZE_BACKLOG_CAPACITY: usize = 8;
const MAX_PENDING_REQUESTS: usize = 32;
const MAX_SERVER_REQUESTS: usize = 32;
const MAX_TRACKED_SESSIONS: usize = 256;
const MAX_COMMAND_SKILLS: usize = 64;
const MAX_LIST_SKILL_CWDS: usize = 256;
const MAX_CATALOG_ITEMS: usize = 4096;
const MAX_REASONING_EFFORTS_PER_MODEL: usize = 64;
const MAX_SKILL_GROUPS: usize = 256;
const MAX_THREAD_RESULT_ITEMS: usize = 4096;
const MAX_IDENTIFIER_BYTES: usize = 1024;
const MAX_RETAINED_IDENTIFIER_BYTES: usize = 2 * 1024 * 1024;
const MAX_CLIENT_COMMAND_BYTES: usize = 256 * 1024;
const MAX_EVENT_BYTES: usize = 256 * 1024;
const MAX_STDOUT_LINE_BYTES: usize = 1024 * 1024;
const MAX_STDERR_LINE_BYTES: usize = 16 * 1024;
const EVENT_BACKPRESSURE_ERROR: &str = "resource_backpressure:event_queue_full";
const COMMAND_BACKPRESSURE_ERROR: &str = "resource_backpressure:command_queue_full";
const REQUEST_BACKPRESSURE_ERROR: &str = "resource_backpressure:request_limit";

/// Configuration for one local App Server connection.
// Clone 미파생: llm_api_key(SecretString)는 평문 복제를 만들지 않는다 (사용처도 없음).
#[derive(Debug)]
pub struct CodexAppServerOptions {
    pub executable: OsString,
    pub client_name: String,
    pub client_title: String,
    pub client_version: String,
    /// 로컬 LLM 프로바이더 오버라이드 (PR-L2). None = 기본(구독/기존 codex 설정).
    /// 프로세스 argv `-c` 오버라이드라 spawn 시점에만 적용된다.
    pub llm_override: Option<CodexLlmOverride>,
    /// custom 프로바이더 API 키 (PR-L4). argv가 아니라 자식 프로세스 env
    /// `DEPPY_LLM_API_KEY`로만 전달한다 — argv는 ps로 노출되기 때문.
    /// custom이 아니면 무시된다.
    pub llm_api_key: Option<secret::SecretString>,
}

impl Default for CodexAppServerOptions {
    fn default() -> Self {
        Self {
            executable: OsString::from("codex"),
            client_name: "deppy_sijo".to_owned(),
            client_title: "Deppy Sijo".to_owned(),
            client_version: env!("CARGO_PKG_VERSION").to_owned(),
            llm_override: None,
            llm_api_key: None,
        }
    }
}

/// Agents(APP) Codex의 로컬 LLM 프로바이더 오버라이드 (PR-L2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexLlmOverride {
    /// 로컬 OSS (ollama) — codex 내장 oss 프로바이더.
    Oss,
    /// OpenAI 호환 커스텀 엔드포인트.
    Custom {
        base_url: String,
        wire: CodexLlmWire,
    },
}

/// custom upstream이 실제로 말하는 API (PR-L5). codex 쪽은 wire_api=responses만
/// 허용하므로 Chat이면 내장 변환 프록시(llm_proxy)를 경유한다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CodexLlmWire {
    /// upstream은 /v1/chat/completions만 지원 — 변환 프록시 경유 (기본,
    /// ollama 계열 원격/로컬 대부분이 여기 해당).
    #[default]
    Chat,
    /// upstream이 /v1/responses를 직접 지원 — 직결 (env_key 경로 유지).
    Responses,
}

/// config 문자열 → 오버라이드. custom인데 base URL이 없거나 잘못되면 Err —
/// 사용자가 명시한 프로바이더를 조용히 기본으로 폴백하지 않는다(권한계층 관례와 동일).
pub fn codex_llm_override_from_config(
    provider: Option<&str>,
    base_url: Option<&str>,
    wire: Option<&str>,
) -> anyhow::Result<Option<CodexLlmOverride>> {
    match provider {
        None => Ok(None),
        Some("oss") => Ok(Some(CodexLlmOverride::Oss)),
        Some("custom") => {
            let base_url = base_url
                .ok_or_else(|| anyhow::anyhow!("custom 프로바이더는 base URL이 필요합니다"))?;
            // None/chat = 변환 프록시 경유(기본). 미지값은 조용히 폴백하지 않는다.
            let wire = match wire {
                None | Some("chat") => CodexLlmWire::Chat,
                Some("responses") => CodexLlmWire::Responses,
                Some(other) => anyhow::bail!("알 수 없는 LLM wire API: {other}"),
            };
            Ok(Some(CodexLlmOverride::Custom {
                base_url: validate_llm_base_url(base_url)?,
                wire,
            }))
        }
        Some(other) => anyhow::bail!("알 수 없는 LLM 프로바이더: {other}"),
    }
}

/// custom 프로바이더 API 키를 자식 프로세스에 전달하는 env var 이름 (PR-L4).
/// argv에는 `-c model_providers.deppy_local.env_key=DEPPY_LLM_API_KEY`로 이름만 넘기고,
/// 값은 spawn 시 Command::env로만 주입한다.
pub const CODEX_LLM_API_KEY_ENV: &str = "DEPPY_LLM_API_KEY";

/// API 키 검증 (PR-L4) — env 값으로 주입되지만 붙여넣기 개행/제어문자 오염은
/// 인증 실패로 이어지므로 입력 단계에서 거부한다 (validate_llm_base_url과 동일 관례).
pub fn validate_llm_api_key(raw: &str) -> anyhow::Result<String> {
    let trimmed = raw.trim();
    anyhow::ensure!(!trimmed.is_empty(), "API 키가 비어 있습니다");
    anyhow::ensure!(
        !trimmed.contains(char::is_whitespace),
        "API 키에 공백을 넣을 수 없습니다"
    );
    anyhow::ensure!(
        !trimmed.contains(char::is_control),
        "API 키에 제어문자를 넣을 수 없습니다"
    );
    Ok(trimmed.to_owned())
}

/// base URL 검증 — 단일 argv `-c key=value`로 들어가므로 내부 공백/제어문자가 있으면
/// 한 덩어리 오설정이 된다 (agents.rs validate_mcp_config_flag 공백 거부와 동일 관례).
pub fn validate_llm_base_url(raw: &str) -> anyhow::Result<String> {
    let trimmed = raw.trim();
    anyhow::ensure!(!trimmed.is_empty(), "base URL이 비어 있습니다");
    anyhow::ensure!(
        !trimmed.contains(char::is_whitespace),
        "base URL에 공백을 넣을 수 없습니다"
    );
    anyhow::ensure!(
        !trimmed.contains(char::is_control),
        "base URL에 제어문자를 넣을 수 없습니다"
    );
    Ok(trimmed.to_owned())
}

/// `codex app-server` spawn argv 조립 (순수 함수 — 단위 테스트 가능하게).
/// 기본 `app-server --listen stdio://` 뒤에 프로바이더 `-c` 오버라이드를 붙인다.
/// `-c` 키는 실기기 codex(2026-07-18)로 검증됨: 내장 로컬 프로바이더 이름은
/// `ollama`(`oss`는 없음, `--oss` 플래그도 app-server에선 거부), custom의
/// wire_api는 `responses`만 허용(`chat`은 폐기 — codex#7782).
fn codex_app_server_args(
    llm_override: Option<&CodexLlmOverride>,
    llm_api_key_present: bool,
) -> anyhow::Result<Vec<String>> {
    let mut args: Vec<String> = ["app-server", "--listen", "stdio://"]
        .map(str::to_owned)
        .into();
    match llm_override {
        None => {}
        Some(CodexLlmOverride::Oss) => {
            args.extend(["-c", "model_provider=ollama"].map(str::to_owned));
        }
        Some(CodexLlmOverride::Custom { base_url, wire }) => {
            // Chat wire는 spawn이 변환 프록시를 띄워 Responses+프록시 주소로 치환한
            // 뒤에만 여기 도달해야 한다 — upstream 직결 argv가 새는 것을 막는다 (PR-L5).
            anyhow::ensure!(
                *wire == CodexLlmWire::Responses,
                "chat wire는 변환 프록시 치환 후에만 argv로 조립할 수 있습니다"
            );
            // enum 생성 경로가 검증을 거치지만, spawn 경계에서 한 번 더 — 잘못된 값이
            // 프로세스 argv로 새는 것을 막는다.
            let base_url = validate_llm_base_url(base_url)?;
            args.extend([
                "-c".to_owned(),
                "model_provider=deppy_local".to_owned(),
                "-c".to_owned(),
                "model_providers.deppy_local.name=deppy_local".to_owned(),
                "-c".to_owned(),
                format!("model_providers.deppy_local.base_url={base_url}"),
                "-c".to_owned(),
                "model_providers.deppy_local.wire_api=responses".to_owned(),
            ]);
            // 키가 있을 때만 env_key를 선언한다 (PR-L4) — env_key가 선언되어 있는데
            // env var가 비어 있으면 codex가 요청 시점에 인증 오류를 낸다. 키 자체는
            // argv가 아니라 spawn의 Command::env로만 주입한다.
            if llm_api_key_present {
                args.extend([
                    "-c".to_owned(),
                    format!("model_providers.deppy_local.env_key={CODEX_LLM_API_KEY_ENV}"),
                ]);
            }
        }
    }
    Ok(args)
}

/// Events are either tied to one local agent session or describe the transport
/// itself. The UI can preserve a finished session when the shared connection
/// later fails.
#[derive(Debug, PartialEq, Eq)]
pub enum CodexAppServerEvent {
    Session {
        session_id: AgentSessionId,
        event: AgentSessionEvent,
    },
    TransportError {
        message: String,
    },
    ConnectionStopped,
}

/// Capacity-limited UI event handoff. The worker never waits for a render
/// consumer: adjacent deltas/statuses coalesce, while a full non-coalescible
/// backlog fails the transport closed instead of growing or silently losing a
/// lifecycle/approval event.
#[derive(Debug, Default)]
struct EventBacklog {
    queue: Mutex<VecDeque<CodexAppServerEvent>>,
    overflowed: AtomicBool,
    overflow_reported: AtomicBool,
}

impl EventBacklog {
    fn push(&self, event: CodexAppServerEvent) -> bool {
        if event_retained_bytes(&event) > MAX_EVENT_BYTES {
            self.overflowed.store(true, Ordering::Release);
            return false;
        }
        let mut queue = lock_unpoisoned(&self.queue);
        if coalesce_event(queue.back_mut(), &event) {
            return true;
        }
        if queue.len() == EVENT_QUEUE_CAPACITY {
            self.overflowed.store(true, Ordering::Release);
            return false;
        }
        queue.push_back(event);
        true
    }

    fn drain(&self) -> Vec<CodexAppServerEvent> {
        let mut queue = lock_unpoisoned(&self.queue);
        let mut events = queue.drain(..).collect::<Vec<_>>();
        drop(queue);
        if self.overflowed.load(Ordering::Acquire)
            && !self.overflow_reported.swap(true, Ordering::AcqRel)
        {
            events.push(CodexAppServerEvent::TransportError {
                message: EVENT_BACKPRESSURE_ERROR.to_owned(),
            });
            events.push(CodexAppServerEvent::ConnectionStopped);
        }
        events
    }

    fn overflowed(&self) -> bool {
        self.overflowed.load(Ordering::Acquire)
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn coalesce_event(last: Option<&mut CodexAppServerEvent>, next: &CodexAppServerEvent) -> bool {
    match (last, next) {
        (
            Some(CodexAppServerEvent::Session {
                session_id: last_session,
                event:
                    AgentSessionEvent::ItemDelta {
                        item_id: last_item,
                        delta: last_delta,
                    },
            }),
            CodexAppServerEvent::Session {
                session_id: next_session,
                event:
                    AgentSessionEvent::ItemDelta {
                        item_id: next_item,
                        delta: next_delta,
                    },
            },
        ) if last_session == next_session && last_item == next_item => {
            let merged_bytes = last_session
                .len()
                .saturating_add(last_item.len())
                .saturating_add(last_delta.len())
                .saturating_add(next_delta.len());
            if merged_bytes > MAX_EVENT_BYTES {
                return false;
            }
            last_delta.push_str(next_delta);
            true
        }
        (
            Some(CodexAppServerEvent::Session {
                session_id: last_session,
                event:
                    AgentSessionEvent::ThreadStatusChanged {
                        status: last_status,
                    },
            }),
            CodexAppServerEvent::Session {
                session_id: next_session,
                event:
                    AgentSessionEvent::ThreadStatusChanged {
                        status: next_status,
                    },
            },
        ) if last_session == next_session => {
            *last_status = *next_status;
            true
        }
        _ => false,
    }
}

fn event_retained_bytes(event: &CodexAppServerEvent) -> usize {
    // Debug is never used here because it can expose command/output text.
    match event {
        CodexAppServerEvent::Session { session_id, event } => session_id
            .len()
            .saturating_add(session_event_retained_bytes(event)),
        CodexAppServerEvent::TransportError { message } => message.len(),
        CodexAppServerEvent::ConnectionStopped => 0,
    }
}

fn session_event_retained_bytes(event: &AgentSessionEvent) -> usize {
    match event {
        AgentSessionEvent::ThreadStarted { thread_id } => thread_id.len(),
        AgentSessionEvent::TurnStarted { turn_id } => turn_id.len(),
        AgentSessionEvent::ItemStarted { item } | AgentSessionEvent::ItemCompleted { item } => item
            .id
            .len()
            .saturating_add(item.status.as_ref().map_or(0, String::len))
            .saturating_add(item.summary.len())
            .saturating_add(item.location.as_ref().map_or(0, String::len))
            .saturating_add(item.detail.as_ref().map_or(0, String::len))
            .saturating_add(item.output.len())
            .saturating_add(item.files.iter().fold(0usize, |bytes, file| {
                bytes
                    .saturating_add(file.path.len())
                    .saturating_add(file.kind.len())
                    .saturating_add(file.diff.as_ref().map_or(0, String::len))
            })),
        AgentSessionEvent::ItemDelta { item_id, delta } => {
            item_id.len().saturating_add(delta.len())
        }
        AgentSessionEvent::ApprovalRequested { approval } => approval
            .request_key
            .len()
            .saturating_add(approval.thread_id.len())
            .saturating_add(approval.turn_id.len())
            .saturating_add(approval.item_id.len())
            .saturating_add(approval.reason.as_ref().map_or(0, String::len))
            .saturating_add(approval.command.as_ref().map_or(0, String::len))
            .saturating_add(approval.cwd.as_ref().map_or(0, String::len)),
        AgentSessionEvent::ApprovalResolved { request_key } => request_key.len(),
        AgentSessionEvent::TurnCompleted { status }
        | AgentSessionEvent::ControlError { message: status }
        | AgentSessionEvent::Failed { message: status } => status.len(),
        AgentSessionEvent::ConnectionReady
        | AgentSessionEvent::ThreadStatusChanged { .. }
        | AgentSessionEvent::Stopped => 0,
    }
}

/// A running local App Server process. It owns one writer worker and two reader
/// threads; all JSON-RPC writes occur on the worker to preserve message order.
pub struct CodexAppServerClient {
    commands: SyncSender<ClientCommand>,
    events: Arc<EventBacklog>,
    stop_requested: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
    /// chat wire 변환 프록시 (PR-L5) — client 수명에 묶여 shutdown/Drop 시 종료.
    llm_proxy: Option<crate::llm_proxy::LlmProxyHandle>,
}

/// One-shot asynchronous JSON result for history/catalog requests. Callers can
/// poll it without blocking the egui frame; the payload stays at the App Server
/// JSON boundary until a UI-specific view model consumes it.
#[allow(dead_code)] // PR-06 phase 2 wires the history UI consumer.
pub type CodexAppServerReply = Receiver<anyhow::Result<Value>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexReasoningEffort {
    pub reasoning_effort: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexModelInfo {
    pub id: String,
    pub model: String,
    pub display_name: String,
    pub description: String,
    pub is_default: bool,
    pub default_reasoning_effort: String,
    pub supported_reasoning_efforts: Vec<CodexReasoningEffort>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexModelCatalogPage {
    pub data: Vec<CodexModelInfo>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexSkillInfo {
    pub cwd: String,
    pub name: String,
    pub path: String,
    pub description: String,
    pub enabled: bool,
    pub scope: String,
}

pub type CodexModelCatalogReply = Receiver<anyhow::Result<CodexModelCatalogPage>>;
pub type CodexSkillCatalogReply = Receiver<anyhow::Result<Vec<CodexSkillInfo>>>;

impl CodexAppServerClient {
    pub fn spawn(
        mut options: CodexAppServerOptions,
        repaint: egui::Context,
    ) -> anyhow::Result<Self> {
        // 키는 spawn에서만 쓰고 worker로 넘기지 않는다 — 이 스코프가 끝나면 평문은
        // 자식 프로세스 env 또는 변환 프록시에만 남는다.
        let mut llm_api_key = options.llm_api_key.take();
        // chat wire custom이면 변환 프록시를 먼저 띄워 base_url을 치환한다 (PR-L5).
        // 키는 프록시가 upstream Authorization으로 붙인다 — 자식 env 노출 불필요.
        let mut llm_proxy = None;
        if let Some(CodexLlmOverride::Custom {
            base_url,
            wire: CodexLlmWire::Chat,
        }) = &options.llm_override
        {
            let upstream_base = validate_llm_base_url(base_url)?;
            let handle = crate::llm_proxy::spawn(upstream_base, llm_api_key.take())?;
            options.llm_override = Some(CodexLlmOverride::Custom {
                base_url: format!("http://127.0.0.1:{}/v1", handle.port),
                wire: CodexLlmWire::Responses,
            });
            llm_proxy = Some(handle);
        }
        let args = codex_app_server_args(options.llm_override.as_ref(), llm_api_key.is_some())?;
        let mut command = Command::new(&options.executable);
        command
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_process_group(&mut command);
        // custom + 키 존재 시에만 env 주입 (PR-L4). Command::env는 부모 env를 그대로
        // 상속한 위에 이 var 하나만 더한다 — env_clear 전체 재구성이 아니다.
        if let (Some(CodexLlmOverride::Custom { .. }), Some(key)) =
            (options.llm_override.as_ref(), llm_api_key.as_ref())
        {
            command.env(CODEX_LLM_API_KEY_ENV, key.expose());
        }
        let child = command.spawn().with_context(|| {
            format!(
                "Codex App Server 실행 실패: {} app-server --listen stdio://",
                options.executable.to_string_lossy()
            )
        })?;
        let mut child = ChildTreeGuard::new(child);
        let stdin = child
            .child_mut()
            .stdin
            .take()
            .context("Codex App Server stdin을 열 수 없음")?;
        let stdout = child
            .child_mut()
            .stdout
            .take()
            .context("Codex App Server stdout을 열 수 없음")?;
        let stderr = child
            .child_mut()
            .stderr
            .take()
            .context("Codex App Server stderr을 열 수 없음")?;

        let (commands_tx, commands_rx) = mpsc::sync_channel(COMMAND_QUEUE_CAPACITY);
        let events = Arc::new(EventBacklog::default());
        let stop_requested = Arc::new(AtomicBool::new(false));
        let worker = thread::Builder::new()
            .name("codex-app-server".to_owned())
            .spawn({
                let events = Arc::clone(&events);
                let stop_requested = Arc::clone(&stop_requested);
                move || {
                    worker_loop(
                        child,
                        stdin,
                        stdout,
                        stderr,
                        options,
                        commands_rx,
                        events,
                        stop_requested,
                        repaint,
                    )
                }
            })
            .context("Codex App Server 워커 생성 실패")?;

        Ok(Self {
            commands: commands_tx,
            events,
            stop_requested,
            worker: Some(worker),
            llm_proxy,
        })
    }

    /// Create a new Codex thread and immediately submit its first turn.
    pub fn start_session(
        &self,
        session_id: AgentSessionId,
        prompt: String,
        cwd: Option<String>,
        model: Option<String>,
        effort: Option<String>,
        skills: Vec<AgentSkillSelection>,
    ) -> anyhow::Result<()> {
        self.send(ClientCommand::StartSession {
            session_id,
            prompt,
            cwd,
            model,
            effort,
            skills,
        })
    }

    /// Submit a later turn to an existing application session.
    pub fn submit_turn(
        &self,
        session_id: AgentSessionId,
        prompt: String,
        cwd: Option<String>,
        model: Option<String>,
        effort: Option<String>,
        skills: Vec<AgentSkillSelection>,
    ) -> anyhow::Result<()> {
        self.send(ClientCommand::SubmitTurn {
            session_id,
            prompt,
            cwd,
            model,
            effort,
            skills,
        })
    }

    /// Add input to the selected active turn. The worker supplies the exact
    /// current `expectedTurnId`; no steer request is sent for idle sessions.
    pub fn steer_turn(
        &self,
        session_id: AgentSessionId,
        prompt: String,
        skills: Vec<AgentSkillSelection>,
    ) -> anyhow::Result<()> {
        self.send(ClientCommand::SteerTurn {
            session_id,
            prompt,
            skills,
        })
    }

    pub fn list_models(
        &self,
        cursor: Option<String>,
        limit: Option<u32>,
        include_hidden: bool,
    ) -> anyhow::Result<CodexModelCatalogReply> {
        let (reply, receiver) = mpsc::sync_channel(REPLY_QUEUE_CAPACITY);
        self.send(ClientCommand::ListModels {
            cursor,
            limit,
            include_hidden,
            reply,
        })?;
        Ok(receiver)
    }

    pub fn list_skills(
        &self,
        cwds: Vec<String>,
        force_reload: bool,
    ) -> anyhow::Result<CodexSkillCatalogReply> {
        let (reply, receiver) = mpsc::sync_channel(REPLY_QUEUE_CAPACITY);
        self.send(ClientCommand::ListSkills {
            cwds,
            force_reload,
            reply,
        })?;
        Ok(receiver)
    }

    pub fn read_rate_limits(&self) -> anyhow::Result<CodexAppServerReply> {
        let (reply, receiver) = mpsc::sync_channel(REPLY_QUEUE_CAPACITY);
        self.send(ClientCommand::ReadRateLimits { reply })?;
        Ok(receiver)
    }

    /// Interrupt only the selected thread/turn. The shared App Server process
    /// remains available for other sessions.
    pub fn interrupt(&self, session_id: AgentSessionId) -> anyhow::Result<()> {
        self.send(ClientCommand::Interrupt { session_id })
    }

    pub fn respond_approval(
        &self,
        session_id: AgentSessionId,
        request_key: String,
        decision: AgentApprovalDecision,
    ) -> anyhow::Result<()> {
        self.send(ClientCommand::RespondApproval {
            session_id,
            request_key,
            decision,
        })
    }

    /// List Deppy-created App Server threads, newest first. `cursor` is the
    /// opaque `nextCursor` from the preceding response.
    #[allow(dead_code)] // PR-06 phase 2 wires the history UI consumer.
    pub fn list_threads(
        &self,
        cursor: Option<String>,
        limit: Option<u32>,
        archived: bool,
    ) -> anyhow::Result<CodexAppServerReply> {
        let (reply, receiver) = mpsc::sync_channel(REPLY_QUEUE_CAPACITY);
        self.send(ClientCommand::ListThreads {
            cursor,
            limit,
            archived,
            reply,
        })?;
        Ok(receiver)
    }

    #[allow(dead_code)] // PR-06 phase 2 wires the history UI consumer.
    pub fn read_thread(
        &self,
        thread_id: String,
        include_turns: bool,
    ) -> anyhow::Result<CodexAppServerReply> {
        let (reply, receiver) = mpsc::sync_channel(REPLY_QUEUE_CAPACITY);
        self.send(ClientCommand::ReadThread {
            thread_id,
            include_turns,
            reply,
        })?;
        Ok(receiver)
    }

    /// Resume an existing Codex thread under an app-owned local session ID.
    #[allow(dead_code)] // PR-06 phase 2 wires the history UI consumer.
    pub fn resume_thread(
        &self,
        session_id: AgentSessionId,
        thread_id: String,
        cwd: Option<String>,
        model: Option<String>,
    ) -> anyhow::Result<CodexAppServerReply> {
        let (reply, receiver) = mpsc::sync_channel(REPLY_QUEUE_CAPACITY);
        self.send(ClientCommand::ResumeThread {
            session_id,
            thread_id,
            cwd,
            model,
            reply,
        })?;
        Ok(receiver)
    }

    #[allow(dead_code)] // PR-06 phase 2 wires the history UI consumer.
    pub fn archive_thread(&self, thread_id: String) -> anyhow::Result<CodexAppServerReply> {
        let (reply, receiver) = mpsc::sync_channel(REPLY_QUEUE_CAPACITY);
        self.send(ClientCommand::ArchiveThread { thread_id, reply })?;
        Ok(receiver)
    }

    /// Drain without blocking; worker-side repaint requests make the next frame
    /// arrive when App Server output lands.
    pub fn drain_events(&self) -> Vec<CodexAppServerEvent> {
        self.events.drain()
    }

    pub fn shutdown(&mut self) {
        self.stop_requested.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        // 자식(app-server) 종료 후 변환 프록시도 내린다 (PR-L5).
        self.llm_proxy.take();
    }

    fn send(&self, command: ClientCommand) -> anyhow::Result<()> {
        anyhow::ensure!(
            command_retained_bytes(&command) <= MAX_CLIENT_COMMAND_BYTES,
            "resource_limit:command_too_large"
        );
        anyhow::ensure!(
            command_item_count_is_valid(&command),
            "resource_limit:command_items"
        );
        anyhow::ensure!(
            command_identifiers_are_valid(&command),
            "resource_limit:identifier"
        );
        match self.commands.try_send(command) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(anyhow::anyhow!(COMMAND_BACKPRESSURE_ERROR)),
            Err(TrySendError::Disconnected(_)) => Err(anyhow::anyhow!(
                "Codex App Server 연결이 이미 종료되었습니다"
            )),
        }
    }
}

fn configure_process_group(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    #[cfg(not(unix))]
    let _ = command;
}

fn terminate_child_tree(child: &mut Child) {
    #[cfg(unix)]
    {
        // SAFETY: spawn always places this child in its own process group, so
        // its pid is the pgid until every descendant is gone. Kill the group
        // before reaping the leader; this avoids signaling a reused pid.
        unsafe {
            libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[derive(Debug)]
struct ChildTreeGuard {
    child: Option<Child>,
}

impl ChildTreeGuard {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    fn child_mut(&mut self) -> &mut Child {
        self.child.as_mut().expect("child guard is active")
    }

    fn terminate(&mut self) {
        if let Some(mut child) = self.child.take() {
            terminate_child_tree(&mut child);
        }
    }

    fn leader_exited_without_reap(&mut self) -> io::Result<bool> {
        #[cfg(unix)]
        {
            let child_id = self.child_mut().id() as libc::id_t;
            let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
            // SAFETY: `info` points to writable siginfo storage. WNOWAIT keeps
            // an exited leader waitable, so the pgid cannot be reused before
            // `terminate` kills the group and reaps the direct child.
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    child_id,
                    info.as_mut_ptr(),
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if result != 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: waitid initialized `info` on success; si_pid is zero
            // when WNOHANG found no waitable state change.
            Ok(unsafe { info.assume_init().si_pid() } != 0)
        }
        #[cfg(not(unix))]
        {
            self.child_mut().try_wait().map(|status| status.is_some())
        }
    }
}

impl Drop for ChildTreeGuard {
    fn drop(&mut self) {
        self.terminate();
    }
}

impl Drop for CodexAppServerClient {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[derive(Debug)]
enum ClientCommand {
    StartSession {
        session_id: AgentSessionId,
        prompt: String,
        cwd: Option<String>,
        model: Option<String>,
        effort: Option<String>,
        skills: Vec<AgentSkillSelection>,
    },
    SubmitTurn {
        session_id: AgentSessionId,
        prompt: String,
        cwd: Option<String>,
        model: Option<String>,
        effort: Option<String>,
        skills: Vec<AgentSkillSelection>,
    },
    SteerTurn {
        session_id: AgentSessionId,
        prompt: String,
        skills: Vec<AgentSkillSelection>,
    },
    Interrupt {
        session_id: AgentSessionId,
    },
    RespondApproval {
        session_id: AgentSessionId,
        request_key: String,
        decision: AgentApprovalDecision,
    },
    #[allow(dead_code)] // Constructed by the phase-2 public API consumer.
    ListThreads {
        cursor: Option<String>,
        limit: Option<u32>,
        archived: bool,
        reply: SyncSender<anyhow::Result<Value>>,
    },
    #[allow(dead_code)] // Constructed by the phase-2 public API consumer.
    ReadThread {
        thread_id: String,
        include_turns: bool,
        reply: SyncSender<anyhow::Result<Value>>,
    },
    #[allow(dead_code)] // Constructed by the phase-2 public API consumer.
    ResumeThread {
        session_id: AgentSessionId,
        thread_id: String,
        cwd: Option<String>,
        model: Option<String>,
        reply: SyncSender<anyhow::Result<Value>>,
    },
    #[allow(dead_code)] // Constructed by the phase-2 public API consumer.
    ArchiveThread {
        thread_id: String,
        reply: SyncSender<anyhow::Result<Value>>,
    },
    ListModels {
        cursor: Option<String>,
        limit: Option<u32>,
        include_hidden: bool,
        reply: SyncSender<anyhow::Result<CodexModelCatalogPage>>,
    },
    ListSkills {
        cwds: Vec<String>,
        force_reload: bool,
        reply: SyncSender<anyhow::Result<Vec<CodexSkillInfo>>>,
    },
    ReadRateLimits {
        reply: SyncSender<anyhow::Result<Value>>,
    },
}

fn command_retained_bytes(command: &ClientCommand) -> usize {
    fn option_bytes(value: &Option<String>) -> usize {
        value.as_ref().map_or(0, String::len)
    }

    fn skill_bytes(skills: &[AgentSkillSelection]) -> usize {
        skills.iter().fold(0usize, |bytes, skill| {
            bytes
                .saturating_add(skill.name.len())
                .saturating_add(skill.path.len())
        })
    }

    match command {
        ClientCommand::StartSession {
            session_id,
            prompt,
            cwd,
            model,
            effort,
            skills,
        }
        | ClientCommand::SubmitTurn {
            session_id,
            prompt,
            cwd,
            model,
            effort,
            skills,
        } => session_id
            .len()
            .saturating_add(prompt.len())
            .saturating_add(option_bytes(cwd))
            .saturating_add(option_bytes(model))
            .saturating_add(option_bytes(effort))
            .saturating_add(skill_bytes(skills)),
        ClientCommand::SteerTurn {
            session_id,
            prompt,
            skills,
        } => session_id
            .len()
            .saturating_add(prompt.len())
            .saturating_add(skill_bytes(skills)),
        ClientCommand::Interrupt { session_id } => session_id.len(),
        ClientCommand::RespondApproval {
            session_id,
            request_key,
            ..
        } => session_id.len().saturating_add(request_key.len()),
        ClientCommand::ListThreads { cursor, .. } => option_bytes(cursor),
        ClientCommand::ReadThread { thread_id, .. }
        | ClientCommand::ArchiveThread { thread_id, .. } => thread_id.len(),
        ClientCommand::ResumeThread {
            session_id,
            thread_id,
            cwd,
            model,
            ..
        } => session_id
            .len()
            .saturating_add(thread_id.len())
            .saturating_add(option_bytes(cwd))
            .saturating_add(option_bytes(model)),
        ClientCommand::ListModels { cursor, .. } => option_bytes(cursor),
        ClientCommand::ListSkills { cwds, .. } => cwds
            .iter()
            .fold(0usize, |bytes, cwd| bytes.saturating_add(cwd.len())),
        ClientCommand::ReadRateLimits { .. } => 0,
    }
}

fn command_creates_pending_request(command: &ClientCommand) -> bool {
    !matches!(command, ClientCommand::RespondApproval { .. })
}

fn command_item_count_is_valid(command: &ClientCommand) -> bool {
    match command {
        ClientCommand::StartSession { skills, .. }
        | ClientCommand::SubmitTurn { skills, .. }
        | ClientCommand::SteerTurn { skills, .. } => skills.len() <= MAX_COMMAND_SKILLS,
        ClientCommand::ListSkills { cwds, .. } => cwds.len() <= MAX_LIST_SKILL_CWDS,
        _ => true,
    }
}

fn command_identifiers_are_valid(command: &ClientCommand) -> bool {
    match command {
        ClientCommand::StartSession { session_id, .. }
        | ClientCommand::SubmitTurn { session_id, .. }
        | ClientCommand::SteerTurn { session_id, .. }
        | ClientCommand::Interrupt { session_id } => valid_identifier(session_id),
        ClientCommand::RespondApproval {
            session_id,
            request_key,
            ..
        } => valid_identifier(session_id) && valid_identifier(request_key),
        ClientCommand::ReadThread { thread_id, .. }
        | ClientCommand::ArchiveThread { thread_id, .. } => valid_identifier(thread_id),
        ClientCommand::ResumeThread {
            session_id,
            thread_id,
            ..
        } => valid_identifier(session_id) && valid_identifier(thread_id),
        ClientCommand::ListThreads { .. }
        | ClientCommand::ListModels { .. }
        | ClientCommand::ListSkills { .. }
        | ClientCommand::ReadRateLimits { .. } => true,
    }
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_IDENTIFIER_BYTES && !value.as_bytes().contains(&0)
}

fn preinitialize_backlog_len(state: &WorkerState) -> usize {
    state
        .queued_starts
        .len()
        .saturating_add(state.queued_rpc_commands.len())
}

#[derive(Debug)]
enum ClientCommandReply {
    Json(SyncSender<anyhow::Result<Value>>),
    Models(SyncSender<anyhow::Result<CodexModelCatalogPage>>),
    Skills(SyncSender<anyhow::Result<Vec<CodexSkillInfo>>>),
}

impl ClientCommandReply {
    fn send_error(self, repaint: &egui::Context, message: String) {
        match self {
            Self::Json(reply) => send_typed_reply(reply, repaint, Err(anyhow::anyhow!(message))),
            Self::Models(reply) => send_typed_reply(reply, repaint, Err(anyhow::anyhow!(message))),
            Self::Skills(reply) => send_typed_reply(reply, repaint, Err(anyhow::anyhow!(message))),
        }
    }
}

#[derive(Debug, PartialEq)]
enum Incoming {
    Json(Value),
    StdoutError(String),
    StdoutClosed,
    Stderr,
}

#[derive(Debug)]
enum PendingRequest {
    Initialize,
    StartThread {
        session_id: AgentSessionId,
        prompt: String,
        cwd: Option<String>,
        model: Option<String>,
        effort: Option<String>,
        skills: Vec<AgentSkillSelection>,
    },
    StartTurn {
        session_id: AgentSessionId,
    },
    Interrupt {
        session_id: AgentSessionId,
    },
    SteerTurn {
        session_id: AgentSessionId,
        expected_turn_id: String,
    },
    ListThreads {
        reply: SyncSender<anyhow::Result<Value>>,
    },
    ReadThread {
        thread_id: String,
        reply: SyncSender<anyhow::Result<Value>>,
    },
    ResumeThread {
        session_id: AgentSessionId,
        thread_id: String,
        reply: SyncSender<anyhow::Result<Value>>,
    },
    ArchiveThread {
        thread_id: String,
        reply: SyncSender<anyhow::Result<Value>>,
    },
    ListModels {
        reply: SyncSender<anyhow::Result<CodexModelCatalogPage>>,
    },
    ListSkills {
        reply: SyncSender<anyhow::Result<Vec<CodexSkillInfo>>>,
    },
    ReadRateLimits {
        reply: SyncSender<anyhow::Result<Value>>,
    },
}

#[derive(Debug)]
struct ServerRequest {
    session_id: AgentSessionId,
    id: Value,
}

#[derive(Debug, Default)]
struct WorkerState {
    next_request_id: u64,
    initialized: bool,
    pending: HashMap<String, PendingRequest>,
    queued_starts: Vec<QueuedStart>,
    queued_rpc_commands: Vec<ClientCommand>,
    thread_to_session: HashMap<String, AgentSessionId>,
    session_to_thread: HashMap<AgentSessionId, String>,
    /// A status notification can race ahead of `thread/start`'s response, which
    /// is where the local session mapping becomes known. Keep only the latest
    /// status for a small bounded number of such threads and replay it once the
    /// mapping is installed.
    buffered_thread_statuses: HashMap<String, AgentThreadStatus>,
    session_to_turn: HashMap<AgentSessionId, String>,
    server_requests: HashMap<String, ServerRequest>,
    known_sessions: HashSet<AgentSessionId>,
    stop_requested: bool,
}

#[derive(Debug)]
struct QueuedStart {
    session_id: AgentSessionId,
    prompt: String,
    cwd: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    skills: Vec<AgentSkillSelection>,
}

impl WorkerState {
    fn request_id(&mut self) -> Value {
        let id = self.next_request_id;
        self.next_request_id += 1;
        Value::from(id)
    }

    fn retained_identifier_bytes(&self) -> usize {
        fn map_bytes(map: &HashMap<String, String>) -> usize {
            map.iter().fold(0usize, |bytes, (key, value)| {
                bytes.saturating_add(key.len()).saturating_add(value.len())
            })
        }

        map_bytes(&self.thread_to_session)
            .saturating_add(map_bytes(&self.session_to_thread))
            .saturating_add(map_bytes(&self.session_to_turn))
            .saturating_add(
                self.buffered_thread_statuses
                    .keys()
                    .fold(0usize, |bytes, key| bytes.saturating_add(key.len())),
            )
            .saturating_add(
                self.server_requests
                    .iter()
                    .fold(0usize, |bytes, (key, request)| {
                        bytes
                            .saturating_add(key.len())
                            .saturating_add(request.session_id.len())
                            .saturating_add(rpc_key(&request.id).len())
                    }),
            )
            .saturating_add(
                self.known_sessions
                    .iter()
                    .fold(0usize, |bytes, id| bytes.saturating_add(id.len())),
            )
    }

    fn can_retain_identifier_bytes(&self, additional: usize) -> bool {
        self.retained_identifier_bytes().saturating_add(additional) <= MAX_RETAINED_IDENTIFIER_BYTES
    }

    fn insert_turn(&mut self, session_id: &str, turn_id: &str) -> bool {
        if !valid_identifier(session_id)
            || !valid_identifier(turn_id)
            || !self.can_retain_identifier_bytes(session_id.len().saturating_add(turn_id.len()))
        {
            return false;
        }
        self.session_to_turn
            .insert(session_id.to_owned(), turn_id.to_owned());
        true
    }
}

#[allow(clippy::too_many_arguments)]
fn worker_loop(
    mut child: ChildTreeGuard,
    mut stdin: ChildStdin,
    stdout: ChildStdout,
    stderr: ChildStderr,
    options: CodexAppServerOptions,
    commands: Receiver<ClientCommand>,
    events: Arc<EventBacklog>,
    stop_requested: Arc<AtomicBool>,
    repaint: egui::Context,
) {
    let (incoming_tx, incoming_rx) = mpsc::sync_channel(INCOMING_QUEUE_CAPACITY);
    let stdout_reader = match spawn_stdout_reader(stdout, incoming_tx.clone()) {
        Ok(reader) => reader,
        Err(_) => {
            emit_transport_error(
                &events,
                &repaint,
                "transport_error:stdout_reader_spawn_failed".to_owned(),
            );
            child.terminate();
            drop(incoming_rx);
            emit(&events, &repaint, CodexAppServerEvent::ConnectionStopped);
            return;
        }
    };
    let stderr_reader = match spawn_stderr_reader(stderr, incoming_tx) {
        Ok(reader) => reader,
        Err(_) => {
            emit_transport_error(
                &events,
                &repaint,
                "transport_error:stderr_reader_spawn_failed".to_owned(),
            );
            child.terminate();
            drop(incoming_rx);
            let _ = stdout_reader.join();
            emit(&events, &repaint, CodexAppServerEvent::ConnectionStopped);
            return;
        }
    };
    let mut state = WorkerState {
        next_request_id: 1,
        ..Default::default()
    };
    let mut stderr_seen = false;

    if send_initialize(&mut stdin, &mut state, &options).is_err() {
        emit_transport_error(
            &events,
            &repaint,
            "transport_error:initialize_send_failed".to_owned(),
        );
    } else {
        loop {
            drain_incoming(
                &incoming_rx,
                &mut stdin,
                &mut state,
                &events,
                &repaint,
                &mut stderr_seen,
            );
            if state.stop_requested || stop_requested.load(Ordering::Acquire) || events.overflowed()
            {
                break;
            }

            match commands.recv_timeout(Duration::from_millis(20)) {
                Ok(command) => {
                    handle_client_command(command, &mut stdin, &mut state, &events, &repaint)
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }

            match child.leader_exited_without_reap() {
                Ok(true) => {
                    emit_transport_error(
                        &events,
                        &repaint,
                        if stderr_seen {
                            "transport_error:child_exited:stderr_present".to_owned()
                        } else {
                            "transport_error:child_exited".to_owned()
                        },
                    );
                    break;
                }
                Ok(false) => {}
                Err(_) => {
                    emit_transport_error(
                        &events,
                        &repaint,
                        "transport_error:child_status_failed".to_owned(),
                    );
                    break;
                }
            }
        }
    }

    // Closing stdin requests a graceful shutdown first. If it does not exit
    // quickly, kill/reap prevents a detached helper from surviving app exit.
    drop(stdin);
    child.terminate();
    drop(incoming_rx);
    let _ = stdout_reader.join();
    let _ = stderr_reader.join();

    for session_id in &state.known_sessions {
        emit_session(
            &events,
            &repaint,
            session_id.clone(),
            AgentSessionEvent::Stopped,
        );
    }
    emit(&events, &repaint, CodexAppServerEvent::ConnectionStopped);
}

fn spawn_stdout_reader(
    stdout: ChildStdout,
    tx: SyncSender<Incoming>,
) -> io::Result<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name("codex-app-server-stdout".to_owned())
        .spawn(move || read_json_lines(stdout, tx))
}

fn spawn_stderr_reader(
    stderr: ChildStderr,
    tx: SyncSender<Incoming>,
) -> io::Result<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name("codex-app-server-stderr".to_owned())
        .spawn(move || read_stderr_lines(stderr, tx))
}

fn read_json_lines(reader: impl Read, tx: SyncSender<Incoming>) {
    let mut reader = BufReader::new(reader);
    loop {
        match read_line_limited(&mut reader, MAX_STDOUT_LINE_BYTES) {
            Ok(None) => {
                let _ = tx.send(Incoming::StdoutClosed);
                return;
            }
            Ok(Some(line)) if line.trim().is_empty() => {}
            Ok(Some(line)) => match serde_json::from_str::<Value>(&line) {
                Ok(value) => {
                    if tx.send(Incoming::Json(value)).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    let _ = error;
                    let _ = tx.send(Incoming::StdoutError(
                        "protocol_error:invalid_json".to_owned(),
                    ));
                    return;
                }
            },
            Err(error) => {
                let _ = tx.send(Incoming::StdoutError(format!(
                    "Codex App Server stdout 읽기 실패: {}",
                    io_error_code(&error)
                )));
                return;
            }
        }
    }
}

fn read_stderr_lines(reader: impl Read, tx: SyncSender<Incoming>) {
    let mut reader = BufReader::new(reader);
    loop {
        match read_line_limited(&mut reader, MAX_STDERR_LINE_BYTES) {
            Ok(Some(line)) => {
                let _ = line;
                if tx.send(Incoming::Stderr).is_err() {
                    return;
                }
            }
            Ok(None) | Err(_) => return,
        }
    }
}

fn read_line_limited(reader: &mut impl BufRead, max_bytes: usize) -> io::Result<Option<String>> {
    let mut bytes = Vec::with_capacity(max_bytes.min(8 * 1024));
    let read = (&mut *reader)
        .take(max_bytes.saturating_add(1) as u64)
        .read_until(b'\n', &mut bytes)?;
    if read == 0 {
        return Ok(None);
    }

    let has_newline = bytes.last() == Some(&b'\n');
    if has_newline {
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
    }
    if bytes.len() > max_bytes {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "line_too_large"));
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid_utf8"))
}

fn io_error_code(error: &io::Error) -> &'static str {
    match error.kind() {
        io::ErrorKind::InvalidData if error.to_string() == "line_too_large" => "line_too_large",
        io::ErrorKind::InvalidData => "invalid_data",
        io::ErrorKind::UnexpectedEof => "unexpected_eof",
        io::ErrorKind::BrokenPipe => "broken_pipe",
        _ => "io_error",
    }
}

fn drain_incoming(
    incoming: &Receiver<Incoming>,
    stdin: &mut ChildStdin,
    state: &mut WorkerState,
    events: &EventBacklog,
    repaint: &egui::Context,
    stderr_seen: &mut bool,
) {
    for _ in 0..INCOMING_DRAIN_PER_TICK {
        match incoming.try_recv() {
            Ok(Incoming::Json(message)) => {
                handle_server_message(message, stdin, state, events, repaint)
            }
            Ok(Incoming::StdoutError(message)) => {
                emit_transport_error(events, repaint, message);
                state.stop_requested = true;
            }
            Ok(Incoming::StdoutClosed) => {
                emit_transport_error(
                    events,
                    repaint,
                    if *stderr_seen {
                        "transport_error:stdout_closed:stderr_present".to_owned()
                    } else {
                        "transport_error:stdout_closed".to_owned()
                    },
                );
                state.stop_requested = true;
            }
            Ok(Incoming::Stderr) => *stderr_seen = true,
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
        }
    }
}

fn handle_client_command(
    command: ClientCommand,
    stdin: &mut ChildStdin,
    state: &mut WorkerState,
    events: &EventBacklog,
    repaint: &egui::Context,
) {
    let preserves_lifecycle_on_error = matches!(&command, ClientCommand::SteerTurn { .. });
    let target = match &command {
        ClientCommand::StartSession { session_id, .. }
        | ClientCommand::SubmitTurn { session_id, .. }
        | ClientCommand::SteerTurn { session_id, .. }
        | ClientCommand::Interrupt { session_id }
        | ClientCommand::RespondApproval { session_id, .. }
        | ClientCommand::ResumeThread { session_id, .. } => Some(session_id.clone()),
        ClientCommand::ListThreads { .. }
        | ClientCommand::ReadThread { .. }
        | ClientCommand::ArchiveThread { .. }
        | ClientCommand::ListModels { .. }
        | ClientCommand::ListSkills { .. }
        | ClientCommand::ReadRateLimits { .. } => None,
    };
    let rpc_reply = match &command {
        ClientCommand::ListThreads { reply, .. }
        | ClientCommand::ReadThread { reply, .. }
        | ClientCommand::ResumeThread { reply, .. }
        | ClientCommand::ArchiveThread { reply, .. }
        | ClientCommand::ReadRateLimits { reply } => Some(ClientCommandReply::Json(reply.clone())),
        ClientCommand::ListModels { reply, .. } => Some(ClientCommandReply::Models(reply.clone())),
        ClientCommand::ListSkills { reply, .. } => Some(ClientCommandReply::Skills(reply.clone())),
        _ => None,
    };
    let queues_before_initialize = !state.initialized
        && matches!(
            &command,
            ClientCommand::StartSession { .. }
                | ClientCommand::ListThreads { .. }
                | ClientCommand::ReadThread { .. }
                | ClientCommand::ResumeThread { .. }
                | ClientCommand::ArchiveThread { .. }
                | ClientCommand::ListModels { .. }
                | ClientCommand::ListSkills { .. }
                | ClientCommand::ReadRateLimits { .. }
        );
    if queues_before_initialize
        && preinitialize_backlog_len(state) >= PREINITIALIZE_BACKLOG_CAPACITY
    {
        reject_client_command(
            target,
            rpc_reply,
            events,
            repaint,
            REQUEST_BACKPRESSURE_ERROR,
            preserves_lifecycle_on_error,
        );
        return;
    }
    if !state.initialized
        && matches!(
            &command,
            ClientCommand::ListThreads { .. }
                | ClientCommand::ReadThread { .. }
                | ClientCommand::ResumeThread { .. }
                | ClientCommand::ArchiveThread { .. }
                | ClientCommand::ListModels { .. }
                | ClientCommand::ListSkills { .. }
                | ClientCommand::ReadRateLimits { .. }
        )
    {
        state.queued_rpc_commands.push(command);
        return;
    }
    if command_creates_pending_request(&command) && state.pending.len() >= MAX_PENDING_REQUESTS {
        reject_client_command(
            target,
            rpc_reply,
            events,
            repaint,
            REQUEST_BACKPRESSURE_ERROR,
            preserves_lifecycle_on_error,
        );
        return;
    }
    let adds_session = match &command {
        ClientCommand::StartSession { session_id, .. }
        | ClientCommand::ResumeThread { session_id, .. } => {
            !state.known_sessions.contains(session_id)
        }
        _ => false,
    };
    if adds_session && state.known_sessions.len() >= MAX_TRACKED_SESSIONS {
        reject_client_command(
            target,
            rpc_reply,
            events,
            repaint,
            "resource_backpressure:session_limit",
            preserves_lifecycle_on_error,
        );
        return;
    }
    if adds_session
        && target
            .as_ref()
            .is_some_and(|session_id| !state.can_retain_identifier_bytes(session_id.len()))
    {
        reject_client_command(
            target,
            rpc_reply,
            events,
            repaint,
            "resource_backpressure:identifier_bytes",
            preserves_lifecycle_on_error,
        );
        return;
    }
    let result = match command {
        ClientCommand::StartSession {
            session_id,
            prompt,
            cwd,
            model,
            effort,
            skills,
        } => {
            state.known_sessions.insert(session_id.clone());
            if state.initialized {
                start_thread(stdin, state, session_id, prompt, cwd, model, effort, skills)
            } else {
                state.queued_starts.push(QueuedStart {
                    session_id,
                    prompt,
                    cwd,
                    model,
                    effort,
                    skills,
                });
                Ok(())
            }
        }
        ClientCommand::SubmitTurn {
            session_id,
            prompt,
            cwd,
            model,
            effort,
            skills,
        } => start_turn_for_session(stdin, state, session_id, prompt, cwd, model, effort, skills),
        ClientCommand::SteerTurn {
            session_id,
            prompt,
            skills,
        } => steer_turn_for_session(stdin, state, session_id, prompt, skills),
        ClientCommand::Interrupt { session_id } => interrupt_turn(stdin, state, session_id),
        ClientCommand::RespondApproval {
            session_id,
            request_key,
            decision,
        } => respond_approval(stdin, state, &session_id, &request_key, decision),
        ClientCommand::ListThreads {
            cursor,
            limit,
            archived,
            reply,
        } => request_thread_list(stdin, state, cursor, limit, archived, reply),
        ClientCommand::ReadThread {
            thread_id,
            include_turns,
            reply,
        } => request_thread_read(stdin, state, thread_id, include_turns, reply),
        ClientCommand::ResumeThread {
            session_id,
            thread_id,
            cwd,
            model,
            reply,
        } => request_thread_resume(stdin, state, session_id, thread_id, cwd, model, reply),
        ClientCommand::ArchiveThread { thread_id, reply } => {
            request_thread_archive(stdin, state, thread_id, reply)
        }
        ClientCommand::ListModels {
            cursor,
            limit,
            include_hidden,
            reply,
        } => request_model_list(stdin, state, cursor, limit, include_hidden, reply),
        ClientCommand::ListSkills {
            cwds,
            force_reload,
            reply,
        } => request_skills_list(stdin, state, cwds, force_reload, reply),
        ClientCommand::ReadRateLimits { reply } => request_rate_limits(stdin, state, reply),
    };

    if result.is_err() {
        let message = "request_failed".to_owned();
        if let Some(reply) = rpc_reply {
            reply.send_error(repaint, message.clone());
        }
        if let Some(session_id) = target {
            emit_session(
                events,
                repaint,
                session_id,
                if preserves_lifecycle_on_error {
                    AgentSessionEvent::ControlError { message }
                } else {
                    AgentSessionEvent::Failed { message }
                },
            );
        } else {
            emit_transport_error(events, repaint, message);
        }
    }
}

fn reject_client_command(
    target: Option<AgentSessionId>,
    rpc_reply: Option<ClientCommandReply>,
    events: &EventBacklog,
    repaint: &egui::Context,
    error_code: &str,
    preserves_lifecycle_on_error: bool,
) {
    let message = error_code.to_owned();
    let had_rpc_reply = rpc_reply.is_some();
    if let Some(reply) = rpc_reply {
        reply.send_error(repaint, message.clone());
    }
    if let Some(session_id) = target {
        emit_session(
            events,
            repaint,
            session_id,
            if preserves_lifecycle_on_error {
                AgentSessionEvent::ControlError { message }
            } else {
                AgentSessionEvent::Failed { message }
            },
        );
    } else if !had_rpc_reply {
        emit_transport_error(events, repaint, message);
    }
}

fn send_initialize(
    stdin: &mut ChildStdin,
    state: &mut WorkerState,
    options: &CodexAppServerOptions,
) -> anyhow::Result<()> {
    let id = state.request_id();
    state
        .pending
        .insert(rpc_key(&id), PendingRequest::Initialize);
    write_message(
        stdin,
        &json!({
            "method": "initialize",
            "id": id,
            "params": {
                "clientInfo": {
                    "name": options.client_name,
                    "title": options.client_title,
                    "version": options.client_version,
                }
            }
        }),
    )
}

#[allow(clippy::too_many_arguments)] // Mirrors stable thread/start + first turn controls.
fn start_thread(
    stdin: &mut ChildStdin,
    state: &mut WorkerState,
    session_id: AgentSessionId,
    prompt: String,
    cwd: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    skills: Vec<AgentSkillSelection>,
) -> anyhow::Result<()> {
    let id = state.request_id();
    state.pending.insert(
        rpc_key(&id),
        PendingRequest::StartThread {
            session_id,
            prompt,
            cwd: cwd.clone(),
            model: model.clone(),
            effort,
            skills,
        },
    );
    let mut params = serde_json::Map::new();
    if let Some(cwd) = cwd {
        params.insert("cwd".to_owned(), Value::String(cwd));
    }
    if let Some(model) = model.filter(|model| !model.trim().is_empty()) {
        params.insert("model".to_owned(), Value::String(model));
    }
    write_message(
        stdin,
        &json!({"method": "thread/start", "id": id, "params": params}),
    )
}

#[allow(clippy::too_many_arguments)] // Mirrors stable turn/start control fields.
fn start_turn_for_session(
    stdin: &mut ChildStdin,
    state: &mut WorkerState,
    session_id: AgentSessionId,
    prompt: String,
    cwd: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    skills: Vec<AgentSkillSelection>,
) -> anyhow::Result<()> {
    let Some(thread_id) = state.session_to_thread.get(&session_id).cloned() else {
        anyhow::bail!("아직 Codex thread가 준비되지 않았습니다");
    };
    let params = turn_start_params(thread_id, prompt, cwd, model, effort, skills)?;
    let id = state.request_id();
    state
        .pending
        .insert(rpc_key(&id), PendingRequest::StartTurn { session_id });
    write_message(
        stdin,
        &json!({"method": "turn/start", "id": id, "params": params}),
    )
}

fn turn_start_params(
    thread_id: String,
    prompt: String,
    cwd: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    skills: Vec<AgentSkillSelection>,
) -> anyhow::Result<Value> {
    let mut params = serde_json::Map::new();
    params.insert("threadId".to_owned(), Value::String(thread_id));
    params.insert(
        "input".to_owned(),
        Value::Array(user_input(prompt, skills)?),
    );
    insert_non_empty(&mut params, "cwd", cwd);
    insert_non_empty(&mut params, "model", model);
    insert_non_empty(&mut params, "effort", effort);
    Ok(Value::Object(params))
}

fn steer_turn_for_session(
    stdin: &mut ChildStdin,
    state: &mut WorkerState,
    session_id: AgentSessionId,
    prompt: String,
    skills: Vec<AgentSkillSelection>,
) -> anyhow::Result<()> {
    let (thread_id, expected_turn_id) = active_turn_ids(state, &session_id)?;
    let id = state.request_id();
    let params = turn_steer_params(thread_id, expected_turn_id.clone(), prompt, skills)?;
    state.pending.insert(
        rpc_key(&id),
        PendingRequest::SteerTurn {
            session_id,
            expected_turn_id,
        },
    );
    write_message(
        stdin,
        &json!({"method": "turn/steer", "id": id, "params": params}),
    )
}

fn active_turn_ids(state: &WorkerState, session_id: &str) -> anyhow::Result<(String, String)> {
    let thread_id = state
        .session_to_thread
        .get(session_id)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("steer할 Codex thread가 없습니다"))?;
    let turn_id = state
        .session_to_turn
        .get(session_id)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("실행 중인 turn에만 steer할 수 있습니다"))?;
    Ok((thread_id, turn_id))
}

fn turn_steer_params(
    thread_id: String,
    expected_turn_id: String,
    prompt: String,
    skills: Vec<AgentSkillSelection>,
) -> anyhow::Result<Value> {
    Ok(json!({
        "threadId": thread_id,
        "expectedTurnId": expected_turn_id,
        "input": user_input(prompt, skills)?,
    }))
}

fn user_input(prompt: String, skills: Vec<AgentSkillSelection>) -> anyhow::Result<Vec<Value>> {
    let mut input = Vec::with_capacity(1 + skills.len());
    if !prompt.trim().is_empty() {
        input.push(json!({"type": "text", "text": prompt}));
    }
    for skill in skills {
        anyhow::ensure!(
            !skill.name.trim().is_empty() && !skill.path.trim().is_empty(),
            "skill name/path가 비어 있습니다"
        );
        input.push(json!({
            "type": "skill",
            "name": skill.name,
            "path": skill.path,
        }));
    }
    anyhow::ensure!(!input.is_empty(), "turn input이 비어 있습니다");
    Ok(input)
}

fn insert_non_empty(params: &mut serde_json::Map<String, Value>, key: &str, value: Option<String>) {
    if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
        params.insert(key.to_owned(), Value::String(value));
    }
}

fn interrupt_turn(
    stdin: &mut ChildStdin,
    state: &mut WorkerState,
    session_id: AgentSessionId,
) -> anyhow::Result<()> {
    let Some(thread_id) = state.session_to_thread.get(&session_id).cloned() else {
        anyhow::bail!("중단할 Codex thread가 없습니다");
    };
    let Some(turn_id) = state.session_to_turn.get(&session_id).cloned() else {
        anyhow::bail!("중단할 실행 중 turn이 없습니다");
    };
    let id = state.request_id();
    state
        .pending
        .insert(rpc_key(&id), PendingRequest::Interrupt { session_id });
    write_message(
        stdin,
        &json!({
            "method": "turn/interrupt",
            "id": id,
            "params": { "threadId": thread_id, "turnId": turn_id }
        }),
    )
}

fn respond_approval(
    stdin: &mut ChildStdin,
    state: &mut WorkerState,
    session_id: &AgentSessionId,
    request_key: &str,
    decision: AgentApprovalDecision,
) -> anyhow::Result<()> {
    let Some(request) = state.server_requests.get(request_key) else {
        anyhow::bail!("해당 승인 요청이 이미 해소되었습니다");
    };
    anyhow::ensure!(
        &request.session_id == session_id,
        "다른 세션의 승인 요청에는 응답할 수 없습니다"
    );
    write_message(
        stdin,
        &json!({"id": request.id, "result": {"decision": decision.wire_value()}}),
    )
}

fn request_thread_list(
    stdin: &mut ChildStdin,
    state: &mut WorkerState,
    cursor: Option<String>,
    limit: Option<u32>,
    archived: bool,
    reply: SyncSender<anyhow::Result<Value>>,
) -> anyhow::Result<()> {
    let id = state.request_id();
    let key = rpc_key(&id);
    state
        .pending
        .insert(key.clone(), PendingRequest::ListThreads { reply });
    let frame = json!({
        "method": "thread/list",
        "id": id,
        "params": thread_list_params(cursor, limit, archived),
    });
    if let Err(error) = write_message(stdin, &frame) {
        state.pending.remove(&key);
        return Err(error);
    }
    Ok(())
}

fn request_thread_read(
    stdin: &mut ChildStdin,
    state: &mut WorkerState,
    thread_id: String,
    include_turns: bool,
    reply: SyncSender<anyhow::Result<Value>>,
) -> anyhow::Result<()> {
    let id = state.request_id();
    let key = rpc_key(&id);
    state.pending.insert(
        key.clone(),
        PendingRequest::ReadThread {
            thread_id: thread_id.clone(),
            reply,
        },
    );
    let frame = json!({
        "method": "thread/read",
        "id": id,
        "params": {"threadId": thread_id, "includeTurns": include_turns},
    });
    if let Err(error) = write_message(stdin, &frame) {
        state.pending.remove(&key);
        return Err(error);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn request_thread_resume(
    stdin: &mut ChildStdin,
    state: &mut WorkerState,
    session_id: AgentSessionId,
    thread_id: String,
    cwd: Option<String>,
    model: Option<String>,
    reply: SyncSender<anyhow::Result<Value>>,
) -> anyhow::Result<()> {
    let id = state.request_id();
    let key = rpc_key(&id);
    state.pending.insert(
        key.clone(),
        PendingRequest::ResumeThread {
            session_id,
            thread_id: thread_id.clone(),
            reply,
        },
    );
    let frame = json!({
        "method": "thread/resume",
        "id": id,
        "params": thread_resume_params(thread_id, cwd, model),
    });
    if let Err(error) = write_message(stdin, &frame) {
        state.pending.remove(&key);
        return Err(error);
    }
    Ok(())
}

fn request_thread_archive(
    stdin: &mut ChildStdin,
    state: &mut WorkerState,
    thread_id: String,
    reply: SyncSender<anyhow::Result<Value>>,
) -> anyhow::Result<()> {
    let id = state.request_id();
    let key = rpc_key(&id);
    state.pending.insert(
        key.clone(),
        PendingRequest::ArchiveThread {
            thread_id: thread_id.clone(),
            reply,
        },
    );
    let frame = json!({
        "method": "thread/archive",
        "id": id,
        "params": {"threadId": thread_id},
    });
    if let Err(error) = write_message(stdin, &frame) {
        state.pending.remove(&key);
        return Err(error);
    }
    Ok(())
}

fn request_model_list(
    stdin: &mut ChildStdin,
    state: &mut WorkerState,
    cursor: Option<String>,
    limit: Option<u32>,
    include_hidden: bool,
    reply: SyncSender<anyhow::Result<CodexModelCatalogPage>>,
) -> anyhow::Result<()> {
    let id = state.request_id();
    let key = rpc_key(&id);
    state
        .pending
        .insert(key.clone(), PendingRequest::ListModels { reply });
    let frame = json!({
        "method": "model/list",
        "id": id,
        "params": model_list_params(cursor, limit, include_hidden),
    });
    if let Err(error) = write_message(stdin, &frame) {
        state.pending.remove(&key);
        return Err(error);
    }
    Ok(())
}

fn request_skills_list(
    stdin: &mut ChildStdin,
    state: &mut WorkerState,
    cwds: Vec<String>,
    force_reload: bool,
    reply: SyncSender<anyhow::Result<Vec<CodexSkillInfo>>>,
) -> anyhow::Result<()> {
    let id = state.request_id();
    let key = rpc_key(&id);
    state
        .pending
        .insert(key.clone(), PendingRequest::ListSkills { reply });
    let frame = json!({
        "method": "skills/list",
        "id": id,
        "params": skills_list_params(cwds, force_reload),
    });
    if let Err(error) = write_message(stdin, &frame) {
        state.pending.remove(&key);
        return Err(error);
    }
    Ok(())
}

fn request_rate_limits(
    stdin: &mut ChildStdin,
    state: &mut WorkerState,
    reply: SyncSender<anyhow::Result<Value>>,
) -> anyhow::Result<()> {
    let id = state.request_id();
    let key = rpc_key(&id);
    state
        .pending
        .insert(key.clone(), PendingRequest::ReadRateLimits { reply });
    let frame = json!({
        "method": "account/rateLimits/read",
        "id": id,
        "params": null,
    });
    if let Err(error) = write_message(stdin, &frame) {
        state.pending.remove(&key);
        return Err(error);
    }
    Ok(())
}

fn model_list_params(cursor: Option<String>, limit: Option<u32>, include_hidden: bool) -> Value {
    let mut params = serde_json::Map::new();
    if let Some(cursor) = cursor.filter(|cursor| !cursor.is_empty()) {
        params.insert("cursor".to_owned(), Value::String(cursor));
    }
    if let Some(limit) = limit {
        params.insert("limit".to_owned(), Value::from(limit));
    }
    params.insert("includeHidden".to_owned(), Value::Bool(include_hidden));
    Value::Object(params)
}

fn skills_list_params(cwds: Vec<String>, force_reload: bool) -> Value {
    json!({
        "cwds": cwds
            .into_iter()
            .filter(|cwd| !cwd.trim().is_empty())
            .collect::<Vec<_>>(),
        "forceReload": force_reload,
    })
}

fn thread_list_params(cursor: Option<String>, limit: Option<u32>, archived: bool) -> Value {
    let mut params = serde_json::Map::from_iter([
        ("sourceKinds".to_owned(), json!(["appServer"])),
        ("archived".to_owned(), Value::Bool(archived)),
        ("sortKey".to_owned(), Value::String("updated_at".to_owned())),
        ("sortDirection".to_owned(), Value::String("desc".to_owned())),
    ]);
    if let Some(cursor) = cursor.filter(|cursor| !cursor.is_empty()) {
        params.insert("cursor".to_owned(), Value::String(cursor));
    }
    if let Some(limit) = limit {
        params.insert("limit".to_owned(), Value::from(limit));
    }
    Value::Object(params)
}

fn thread_resume_params(thread_id: String, cwd: Option<String>, model: Option<String>) -> Value {
    let mut params =
        serde_json::Map::from_iter([("threadId".to_owned(), Value::String(thread_id))]);
    if let Some(cwd) = cwd.filter(|cwd| !cwd.trim().is_empty()) {
        params.insert("cwd".to_owned(), Value::String(cwd));
    }
    if let Some(model) = model.filter(|model| !model.trim().is_empty()) {
        params.insert("model".to_owned(), Value::String(model));
    }
    Value::Object(params)
}

fn handle_server_message(
    message: Value,
    stdin: &mut ChildStdin,
    state: &mut WorkerState,
    events: &EventBacklog,
    repaint: &egui::Context,
) {
    if message.get("method").is_some() && message.get("id").is_some() {
        handle_server_request(message, stdin, state, events, repaint);
    } else if message.get("method").is_some() {
        handle_notification(message, state, events, repaint);
    } else if message.get("id").is_some() {
        handle_response(message, stdin, state, events, repaint);
    } else {
        emit_transport_error(events, repaint, "protocol_error:unknown_message".to_owned());
    }
}

fn handle_response(
    mut message: Value,
    stdin: &mut ChildStdin,
    state: &mut WorkerState,
    events: &EventBacklog,
    repaint: &egui::Context,
) {
    let Some(id) = message.get("id") else { return };
    let Some(pending) = state.pending.remove(&rpc_key(id)) else {
        return;
    };
    if let Some(error) = message.get("error") {
        let text = rpc_error_text(error);
        match pending {
            PendingRequest::Initialize => {
                emit_transport_error(events, repaint, text.clone());
                for session_id in &state.known_sessions {
                    emit_session(
                        events,
                        repaint,
                        session_id.clone(),
                        AgentSessionEvent::Failed {
                            message: format!("Codex App Server 초기화 실패: {text}"),
                        },
                    );
                }
                state.stop_requested = true;
            }
            PendingRequest::StartThread { session_id, .. }
            | PendingRequest::StartTurn { session_id }
            | PendingRequest::Interrupt { session_id } => {
                emit_session(
                    events,
                    repaint,
                    session_id,
                    AgentSessionEvent::Failed { message: text },
                );
            }
            PendingRequest::SteerTurn { session_id, .. } => {
                emit_session(
                    events,
                    repaint,
                    session_id,
                    AgentSessionEvent::ControlError { message: text },
                );
            }
            PendingRequest::ResumeThread {
                session_id, reply, ..
            } => {
                send_rpc_reply(reply, repaint, Err(anyhow::anyhow!(text.clone())));
                emit_session(
                    events,
                    repaint,
                    session_id,
                    AgentSessionEvent::Failed { message: text },
                );
            }
            PendingRequest::ListThreads { reply }
            | PendingRequest::ReadThread { reply, .. }
            | PendingRequest::ArchiveThread { reply, .. }
            | PendingRequest::ReadRateLimits { reply } => {
                send_rpc_reply(reply, repaint, Err(anyhow::anyhow!(text)));
            }
            PendingRequest::ListModels { reply } => {
                send_typed_reply(reply, repaint, Err(anyhow::anyhow!(text)));
            }
            PendingRequest::ListSkills { reply } => {
                send_typed_reply(reply, repaint, Err(anyhow::anyhow!(text)));
            }
        }
        return;
    }
    let result = message
        .get_mut("result")
        .map(Value::take)
        .unwrap_or(Value::Null);
    match pending {
        PendingRequest::Initialize => {
            state.initialized = true;
            if write_message(stdin, &json!({"method": "initialized", "params": {}})).is_err() {
                emit_transport_error(
                    events,
                    repaint,
                    "transport_error:initialized_send_failed".to_owned(),
                );
                return;
            }
            let starts = std::mem::take(&mut state.queued_starts);
            for start in starts {
                if start_thread(
                    stdin,
                    state,
                    start.session_id.clone(),
                    start.prompt,
                    start.cwd,
                    start.model,
                    start.effort,
                    start.skills,
                )
                .is_err()
                {
                    emit_session(
                        events,
                        repaint,
                        start.session_id,
                        AgentSessionEvent::Failed {
                            message: "request_failed:thread_start".to_owned(),
                        },
                    );
                }
            }
            let queued_commands = std::mem::take(&mut state.queued_rpc_commands);
            for command in queued_commands {
                handle_client_command(command, stdin, state, events, repaint);
            }
            for session_id in &state.known_sessions {
                emit_session(
                    events,
                    repaint,
                    session_id.clone(),
                    AgentSessionEvent::ConnectionReady,
                );
            }
        }
        PendingRequest::StartThread {
            session_id,
            prompt,
            cwd,
            model,
            effort,
            skills,
        } => {
            let Some(thread_id) = result.pointer("/thread/id").and_then(Value::as_str) else {
                emit_session(
                    events,
                    repaint,
                    session_id,
                    AgentSessionEvent::Failed {
                        message: "thread/start 응답에 thread.id가 없습니다".to_owned(),
                    },
                );
                return;
            };
            if !valid_identifier(thread_id)
                || !state.can_retain_identifier_bytes(
                    thread_id
                        .len()
                        .saturating_add(session_id.len())
                        .saturating_mul(2),
                )
            {
                emit_session(
                    events,
                    repaint,
                    session_id,
                    AgentSessionEvent::Failed {
                        message: "resource_limit:thread_identifier".to_owned(),
                    },
                );
                return;
            }
            let thread_id = thread_id.to_owned();
            state
                .thread_to_session
                .insert(thread_id.clone(), session_id.clone());
            state
                .session_to_thread
                .insert(session_id.clone(), thread_id.clone());
            emit_session(
                events,
                repaint,
                session_id.clone(),
                AgentSessionEvent::ThreadStarted {
                    thread_id: thread_id.clone(),
                },
            );
            if let Some(status) = state.buffered_thread_statuses.remove(&thread_id) {
                emit_session(
                    events,
                    repaint,
                    session_id.clone(),
                    AgentSessionEvent::ThreadStatusChanged { status },
                );
            }
            if start_turn_for_session(
                stdin,
                state,
                session_id.clone(),
                prompt,
                cwd,
                model,
                effort,
                skills,
            )
            .is_err()
            {
                emit_session(
                    events,
                    repaint,
                    session_id,
                    AgentSessionEvent::Failed {
                        message: "request_failed:turn_start".to_owned(),
                    },
                );
            }
        }
        PendingRequest::StartTurn { session_id } => {
            if let Some(turn_id) = result.pointer("/turn/id").and_then(Value::as_str) {
                let turn_id = turn_id.to_owned();
                if !state.insert_turn(&session_id, &turn_id) {
                    emit_session(
                        events,
                        repaint,
                        session_id,
                        AgentSessionEvent::Failed {
                            message: "resource_limit:turn_identifier".to_owned(),
                        },
                    );
                    return;
                }
                emit_session(
                    events,
                    repaint,
                    session_id,
                    AgentSessionEvent::TurnStarted { turn_id },
                );
            } else {
                emit_session(
                    events,
                    repaint,
                    session_id,
                    AgentSessionEvent::Failed {
                        message: "turn/start 응답에 turn.id가 없습니다".to_owned(),
                    },
                );
            }
        }
        PendingRequest::SteerTurn {
            session_id,
            expected_turn_id,
        } => {
            let response_turn_id = result.get("turnId").and_then(Value::as_str);
            if response_turn_id != Some(expected_turn_id.as_str()) {
                emit_session(
                    events,
                    repaint,
                    session_id,
                    AgentSessionEvent::ControlError {
                        message: format!(
                            "turn/steer 응답 ID 불일치: expected {expected_turn_id}, got {}",
                            response_turn_id.unwrap_or("missing")
                        ),
                    },
                );
            }
        }
        // Completion is confirmed by turn/completed, not this acknowledgement.
        PendingRequest::Interrupt { .. } => {}
        PendingRequest::ListThreads { reply } => {
            let response = validate_thread_list_result(&result).map(|()| result);
            send_rpc_reply(reply, repaint, response);
        }
        PendingRequest::ReadRateLimits { reply } => {
            let response = if result.is_object() {
                Ok(result)
            } else {
                Err(anyhow::anyhow!(
                    "account/rateLimits/read 응답이 객체가 아닙니다"
                ))
            };
            send_rpc_reply(reply, repaint, response);
        }
        PendingRequest::ReadThread { thread_id, reply } => {
            let response = validate_thread_result(&result, &thread_id).map(|()| result);
            send_rpc_reply(reply, repaint, response);
        }
        PendingRequest::ResumeThread {
            session_id,
            thread_id,
            reply,
        } => {
            if validate_thread_result(&result, &thread_id).is_err()
                || !valid_identifier(&thread_id)
                || !valid_identifier(&session_id)
                || !state.can_retain_identifier_bytes(
                    thread_id
                        .len()
                        .saturating_add(session_id.len())
                        .saturating_mul(2)
                        .saturating_add(session_id.len()),
                )
            {
                let message = "protocol_error:thread_resume_response".to_owned();
                send_rpc_reply(reply, repaint, Err(anyhow::anyhow!(message.clone())));
                emit_session(
                    events,
                    repaint,
                    session_id,
                    AgentSessionEvent::Failed { message },
                );
                return;
            }
            state.known_sessions.insert(session_id.clone());
            state
                .thread_to_session
                .insert(thread_id.clone(), session_id.clone());
            state
                .session_to_thread
                .insert(session_id.clone(), thread_id.clone());
            // Cross-channel ordering barrier: make the authoritative snapshot
            // observable before any mapping-dependent stream event. The UI
            // also polls replies before events, so a later delta/status cannot
            // be erased by an older resume snapshot on the next frame.
            send_rpc_reply(reply, repaint, Ok(result));
            emit_session(
                events,
                repaint,
                session_id.clone(),
                AgentSessionEvent::ThreadStarted {
                    thread_id: thread_id.clone(),
                },
            );
            if let Some(status) = state.buffered_thread_statuses.remove(&thread_id) {
                emit_session(
                    events,
                    repaint,
                    session_id,
                    AgentSessionEvent::ThreadStatusChanged { status },
                );
            }
        }
        PendingRequest::ArchiveThread { thread_id, reply } => {
            let response = if result.is_object() {
                Ok(result)
            } else {
                Err(anyhow::anyhow!("thread/archive 응답이 객체가 아닙니다"))
            };
            if response.is_ok()
                && let Some(session_id) = state.thread_to_session.remove(&thread_id)
            {
                state.session_to_thread.remove(&session_id);
                state.session_to_turn.remove(&session_id);
                state.known_sessions.remove(&session_id);
            }
            send_rpc_reply(reply, repaint, response);
        }
        PendingRequest::ListModels { reply } => {
            send_typed_reply(reply, repaint, parse_model_catalog(&result));
        }
        PendingRequest::ListSkills { reply } => {
            send_typed_reply(reply, repaint, parse_skill_catalog(&result));
        }
    }
}

fn send_rpc_reply(
    reply: SyncSender<anyhow::Result<Value>>,
    repaint: &egui::Context,
    result: anyhow::Result<Value>,
) {
    send_typed_reply(reply, repaint, result);
}

fn send_typed_reply<T>(
    reply: SyncSender<anyhow::Result<T>>,
    repaint: &egui::Context,
    result: anyhow::Result<T>,
) {
    let _ = reply.try_send(result);
    repaint.request_repaint();
}

fn parse_model_catalog(result: &Value) -> anyhow::Result<CodexModelCatalogPage> {
    let data = result
        .get("data")
        .and_then(Value::as_array)
        .context("model/list 응답에 data 배열이 없습니다")?;
    anyhow::ensure!(
        data.len() <= MAX_CATALOG_ITEMS,
        "resource_limit:model_catalog_items"
    );
    let mut models = Vec::with_capacity(data.len());
    let mut total_efforts = 0usize;
    for model in data {
        let effort_values = model
            .get("supportedReasoningEfforts")
            .and_then(Value::as_array)
            .context("model/list 모델에 supportedReasoningEfforts 배열이 없습니다")?;
        add_bounded_items(
            &mut total_efforts,
            effort_values.len(),
            MAX_REASONING_EFFORTS_PER_MODEL,
            MAX_CATALOG_ITEMS,
            "resource_limit:model_effort_items",
        )?;
        let efforts = effort_values
            .iter()
            .map(|effort| {
                Ok(CodexReasoningEffort {
                    reasoning_effort: required_string(effort, "reasoningEffort")?,
                    description: required_string(effort, "description")?,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        models.push(CodexModelInfo {
            id: required_string(model, "id")?,
            model: required_string(model, "model")?,
            display_name: required_string(model, "displayName")?,
            description: required_string(model, "description")?,
            is_default: model
                .get("isDefault")
                .and_then(Value::as_bool)
                .context("model/list 모델에 isDefault boolean이 없습니다")?,
            default_reasoning_effort: required_string(model, "defaultReasoningEffort")?,
            supported_reasoning_efforts: efforts,
        });
    }
    let next_cursor = match result.get("nextCursor") {
        None | Some(Value::Null) => None,
        Some(Value::String(cursor)) => Some(cursor.clone()),
        Some(_) => anyhow::bail!("model/list nextCursor 형식이 잘못되었습니다"),
    };
    Ok(CodexModelCatalogPage {
        data: models,
        next_cursor,
    })
}

fn parse_skill_catalog(result: &Value) -> anyhow::Result<Vec<CodexSkillInfo>> {
    let entries = result
        .get("data")
        .and_then(Value::as_array)
        .context("skills/list 응답에 data 배열이 없습니다")?;
    anyhow::ensure!(
        entries.len() <= MAX_SKILL_GROUPS,
        "resource_limit:skill_groups"
    );
    let mut catalog = Vec::new();
    for entry in entries {
        let cwd = required_string(entry, "cwd")?;
        let skills = entry
            .get("skills")
            .and_then(Value::as_array)
            .context("skills/list entry에 skills 배열이 없습니다")?;
        let mut total = catalog.len();
        add_bounded_items(
            &mut total,
            skills.len(),
            MAX_CATALOG_ITEMS,
            MAX_CATALOG_ITEMS,
            "resource_limit:skill_catalog_items",
        )?;
        for skill in skills {
            catalog.push(CodexSkillInfo {
                cwd: cwd.clone(),
                name: required_string(skill, "name")?,
                path: required_string(skill, "path")?,
                description: required_string(skill, "description")?,
                enabled: skill
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .context("skills/list skill에 enabled boolean이 없습니다")?,
                scope: required_string(skill, "scope")?,
            });
        }
    }
    Ok(catalog)
}

fn add_bounded_items(
    total: &mut usize,
    count: usize,
    per_container_max: usize,
    aggregate_max: usize,
    error_code: &'static str,
) -> anyhow::Result<()> {
    anyhow::ensure!(count <= per_container_max, error_code);
    let next = total.saturating_add(count);
    anyhow::ensure!(next <= aggregate_max, error_code);
    *total = next;
    Ok(())
}

fn required_string(value: &Value, key: &str) -> anyhow::Result<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .with_context(|| format!("응답에 {key} string이 없습니다"))
}

fn validate_thread_list_result(result: &Value) -> anyhow::Result<()> {
    anyhow::ensure!(result.is_object(), "thread/list 응답이 객체가 아닙니다");
    anyhow::ensure!(
        result.get("data").is_some_and(Value::is_array),
        "thread/list 응답에 data 배열이 없습니다"
    );
    let data = result.get("data").and_then(Value::as_array).unwrap();
    anyhow::ensure!(
        data.len() <= MAX_THREAD_RESULT_ITEMS
            && json_container_items_within(result, MAX_THREAD_RESULT_ITEMS),
        "resource_limit:thread_list_items"
    );
    if let Some(cursor) = result.get("nextCursor") {
        anyhow::ensure!(
            cursor.is_null() || cursor.is_string(),
            "thread/list nextCursor 형식이 잘못되었습니다"
        );
    }
    Ok(())
}

fn validate_thread_result(result: &Value, expected_thread_id: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        json_container_items_within(result, MAX_THREAD_RESULT_ITEMS),
        "resource_limit:thread_result_items"
    );
    let thread_id = result
        .pointer("/thread/id")
        .and_then(Value::as_str)
        .context("thread 응답에 thread.id가 없습니다")?;
    anyhow::ensure!(
        thread_id == expected_thread_id,
        "thread 응답 ID 불일치: expected {expected_thread_id}, got {thread_id}"
    );
    Ok(())
}

fn json_container_items_within(value: &Value, max_items: usize) -> bool {
    fn visit(value: &Value, items: &mut usize, max_items: usize) -> bool {
        match value {
            Value::Array(values) => {
                *items = items.saturating_add(values.len());
                if *items > max_items {
                    return false;
                }
                values.iter().all(|value| visit(value, items, max_items))
            }
            Value::Object(values) => values.values().all(|value| visit(value, items, max_items)),
            _ => true,
        }
    }

    visit(value, &mut 0, max_items)
}

fn item_projection_within_limits(item: &Value) -> bool {
    item.get("id")
        .and_then(Value::as_str)
        .is_some_and(valid_identifier)
        && json_container_items_within(item, MAX_CATALOG_ITEMS)
}

fn handle_notification(
    message: Value,
    state: &mut WorkerState,
    events: &EventBacklog,
    repaint: &egui::Context,
) {
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        return;
    };
    let params = message.get("params").unwrap_or(&Value::Null);
    match method {
        "thread/status/changed" => {
            let Some(thread_id) = params.get("threadId").and_then(Value::as_str) else {
                return;
            };
            if !valid_identifier(thread_id) {
                return;
            }
            let Some(status) = params.get("status").and_then(AgentThreadStatus::from_codex) else {
                return;
            };
            if let Some(session_id) = state.thread_to_session.get(thread_id).cloned() {
                emit_session(
                    events,
                    repaint,
                    session_id,
                    AgentSessionEvent::ThreadStatusChanged { status },
                );
            } else if state.buffered_thread_statuses.contains_key(thread_id)
                || state.buffered_thread_statuses.len() < MAX_BUFFERED_THREAD_STATUSES
            {
                state
                    .buffered_thread_statuses
                    .insert(thread_id.to_owned(), status);
            }
        }
        "thread/started" => {
            let Some(thread_id) = params.pointer("/thread/id").and_then(Value::as_str) else {
                return;
            };
            if let Some(session_id) = state.thread_to_session.get(thread_id).cloned() {
                emit_session(
                    events,
                    repaint,
                    session_id,
                    AgentSessionEvent::ThreadStarted {
                        thread_id: thread_id.to_owned(),
                    },
                );
            }
        }
        "turn/started" => {
            let Some((session_id, turn_id)) = session_and_turn(params, state) else {
                return;
            };
            if !state.insert_turn(&session_id, &turn_id) {
                emit_transport_error(events, repaint, "resource_limit:turn_identifier".to_owned());
                return;
            }
            emit_session(
                events,
                repaint,
                session_id,
                AgentSessionEvent::TurnStarted { turn_id },
            );
        }
        "item/started" => {
            let Some(session_id) = session_for_params(params, state) else {
                return;
            };
            let Some(raw_item) = params.get("item") else {
                return;
            };
            if !item_projection_within_limits(raw_item) {
                emit_transport_error(events, repaint, "resource_limit:item_projection".to_owned());
                state.stop_requested = true;
                return;
            }
            let Some(item) = AgentItem::from_codex(raw_item) else {
                return;
            };
            emit_session(
                events,
                repaint,
                session_id,
                AgentSessionEvent::ItemStarted { item },
            );
        }
        "item/completed" => {
            let Some(session_id) = session_for_params(params, state) else {
                return;
            };
            let Some(raw_item) = params.get("item") else {
                return;
            };
            if !item_projection_within_limits(raw_item) {
                emit_transport_error(events, repaint, "resource_limit:item_projection".to_owned());
                state.stop_requested = true;
                return;
            }
            let Some(item) = AgentItem::from_codex(raw_item) else {
                return;
            };
            emit_session(
                events,
                repaint,
                session_id,
                AgentSessionEvent::ItemCompleted { item },
            );
        }
        "item/agentMessage/delta"
        | "item/plan/delta"
        | "item/reasoning/summaryTextDelta"
        | "item/reasoning/textDelta"
        | "item/commandExecution/outputDelta" => {
            let Some(session_id) = session_for_params(params, state) else {
                return;
            };
            let Some(item_id) = params.get("itemId").and_then(Value::as_str) else {
                return;
            };
            let Some(delta) = params.get("delta").and_then(Value::as_str) else {
                return;
            };
            emit_session(
                events,
                repaint,
                session_id,
                AgentSessionEvent::ItemDelta {
                    item_id: item_id.to_owned(),
                    delta: delta.to_owned(),
                },
            );
        }
        "turn/completed" => {
            let Some(session_id) = session_for_params(params, state) else {
                return;
            };
            let status = params
                .pointer("/turn/status")
                .or_else(|| params.get("status"))
                .and_then(Value::as_str)
                .unwrap_or("completed")
                .to_owned();
            state.session_to_turn.remove(&session_id);
            emit_session(
                events,
                repaint,
                session_id,
                AgentSessionEvent::TurnCompleted { status },
            );
        }
        "serverRequest/resolved" => {
            let Some(session_id) = session_for_params(params, state) else {
                return;
            };
            let Some(request_id) = params.get("requestId") else {
                return;
            };
            let request_key = rpc_key(request_id);
            state.server_requests.remove(&request_key);
            emit_session(
                events,
                repaint,
                session_id,
                AgentSessionEvent::ApprovalResolved { request_key },
            );
        }
        _ => {}
    }
}

fn handle_server_request(
    message: Value,
    stdin: &mut ChildStdin,
    state: &mut WorkerState,
    events: &EventBacklog,
    repaint: &egui::Context,
) {
    let Some(id) = message.get("id").cloned() else {
        return;
    };
    let method = message
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let params = message.get("params").unwrap_or(&Value::Null);
    let kind = match method {
        "item/commandExecution/requestApproval" => Some(AgentApprovalKind::CommandExecution),
        "item/fileChange/requestApproval" => Some(AgentApprovalKind::FileChange),
        _ => None,
    };
    let Some(kind) = kind else {
        let _ = write_server_error(stdin, id, -32601, "unsupported_request");
        emit_transport_error(
            events,
            repaint,
            "protocol_error:unsupported_request".to_owned(),
        );
        return;
    };
    let Some(thread_id) = params.get("threadId").and_then(Value::as_str) else {
        let _ = write_server_error(stdin, id, -32602, "invalid_request");
        return;
    };
    let Some(session_id) = state.thread_to_session.get(thread_id).cloned() else {
        let _ = write_server_error(stdin, id, -32602, "unknown_thread");
        return;
    };
    let Some(turn_id) = params.get("turnId").and_then(Value::as_str) else {
        let _ = write_server_error(stdin, id, -32602, "invalid_request");
        return;
    };
    let Some(item_id) = params.get("itemId").and_then(Value::as_str) else {
        let _ = write_server_error(stdin, id, -32602, "invalid_request");
        return;
    };
    let request_key = rpc_key(&id);
    if !valid_identifier(thread_id)
        || !valid_identifier(&session_id)
        || !valid_identifier(turn_id)
        || !valid_identifier(item_id)
        || !valid_identifier(&request_key)
    {
        let _ = write_server_error(stdin, id, -32602, "identifier_limit");
        return;
    }
    if state.server_requests.len() >= MAX_SERVER_REQUESTS {
        let _ = write_server_error(stdin, id, -32000, REQUEST_BACKPRESSURE_ERROR);
        emit_transport_error(events, repaint, REQUEST_BACKPRESSURE_ERROR.to_owned());
        return;
    };
    let additional_identifier_bytes = request_key
        .len()
        .saturating_mul(2)
        .saturating_add(session_id.len());
    if !state.can_retain_identifier_bytes(additional_identifier_bytes) {
        let _ = write_server_error(stdin, id, -32000, "identifier_backpressure");
        return;
    }
    state.server_requests.insert(
        request_key.clone(),
        ServerRequest {
            session_id: session_id.clone(),
            id: id.clone(),
        },
    );
    let accepted = emit_session(
        events,
        repaint,
        session_id,
        AgentSessionEvent::ApprovalRequested {
            approval: AgentApproval {
                request_key: request_key.clone(),
                kind,
                thread_id: thread_id.to_owned(),
                turn_id: turn_id.to_owned(),
                item_id: item_id.to_owned(),
                reason: params
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                command: params
                    .get("command")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                cwd: params.get("cwd").and_then(Value::as_str).map(str::to_owned),
            },
        },
    );
    if !accepted {
        state.server_requests.remove(&request_key);
        let _ = write_server_error(stdin, id, -32000, EVENT_BACKPRESSURE_ERROR);
    }
}

fn write_server_error(
    stdin: &mut ChildStdin,
    id: Value,
    code: i64,
    message: &'static str,
) -> anyhow::Result<()> {
    write_message(
        stdin,
        &json!({"id": id, "error": {"code": code, "message": message}}),
    )
}

fn session_for_params(params: &Value, state: &WorkerState) -> Option<AgentSessionId> {
    let thread_id = params.get("threadId").and_then(Value::as_str)?;
    if !valid_identifier(thread_id) {
        return None;
    }
    state.thread_to_session.get(thread_id).cloned()
}

fn session_and_turn(params: &Value, state: &WorkerState) -> Option<(AgentSessionId, String)> {
    let session_id = session_for_params(params, state)?;
    let turn_id = params.pointer("/turn/id").and_then(Value::as_str)?;
    if !valid_identifier(turn_id) {
        return None;
    }
    let turn_id = turn_id.to_owned();
    Some((session_id, turn_id))
}

fn write_message(stdin: &mut ChildStdin, message: &Value) -> anyhow::Result<()> {
    serde_json::to_writer(&mut *stdin, message)?;
    stdin.write_all(b"\n")?;
    stdin.flush()?;
    Ok(())
}

fn rpc_key(id: &Value) -> String {
    serde_json::to_string(id).unwrap_or_else(|_| "null".to_owned())
}

fn rpc_error_text(error: &Value) -> String {
    let _ = error;
    "protocol_error:rpc_error".to_owned()
}

fn emit(events: &EventBacklog, repaint: &egui::Context, event: CodexAppServerEvent) -> bool {
    let accepted = events.push(event);
    repaint.request_repaint();
    accepted
}

fn emit_session(
    events: &EventBacklog,
    repaint: &egui::Context,
    session_id: AgentSessionId,
    event: AgentSessionEvent,
) -> bool {
    emit(
        events,
        repaint,
        CodexAppServerEvent::Session { session_id, event },
    )
}

fn emit_transport_error(events: &EventBacklog, repaint: &egui::Context, message: String) -> bool {
    emit(
        events,
        repaint,
        CodexAppServerEvent::TransportError { message },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_server_args_기본은_오버라이드_없이_고정_argv다() {
        assert_eq!(
            codex_app_server_args(None, false).unwrap(),
            vec!["app-server", "--listen", "stdio://"]
        );
    }

    #[test]
    fn app_server_args_oss는_내장_ollama_프로바이더를_지정한다() {
        assert_eq!(
            codex_app_server_args(Some(&CodexLlmOverride::Oss), false).unwrap(),
            vec![
                "app-server",
                "--listen",
                "stdio://",
                "-c",
                "model_provider=ollama",
            ]
        );
    }

    #[test]
    fn app_server_args_custom은_deppy_local_프로바이더를_정의한다() {
        let args = codex_app_server_args(
            Some(&CodexLlmOverride::Custom {
                base_url: "http://localhost:11434/v1".to_owned(),
                wire: CodexLlmWire::Responses,
            }),
            false,
        )
        .unwrap();
        assert_eq!(
            args,
            vec![
                "app-server",
                "--listen",
                "stdio://",
                "-c",
                "model_provider=deppy_local",
                "-c",
                "model_providers.deppy_local.name=deppy_local",
                "-c",
                "model_providers.deppy_local.base_url=http://localhost:11434/v1",
                "-c",
                "model_providers.deppy_local.wire_api=responses",
            ]
        );
    }

    #[test]
    fn app_server_args_custom은_키_존재_시_env_key를_선언한다() {
        let args = codex_app_server_args(
            Some(&CodexLlmOverride::Custom {
                base_url: "http://localhost:11434/v1".to_owned(),
                wire: CodexLlmWire::Responses,
            }),
            true,
        )
        .unwrap();
        assert_eq!(
            args,
            vec![
                "app-server",
                "--listen",
                "stdio://",
                "-c",
                "model_provider=deppy_local",
                "-c",
                "model_providers.deppy_local.name=deppy_local",
                "-c",
                "model_providers.deppy_local.base_url=http://localhost:11434/v1",
                "-c",
                "model_providers.deppy_local.wire_api=responses",
                "-c",
                "model_providers.deppy_local.env_key=DEPPY_LLM_API_KEY",
            ]
        );
        // 키 값 자체는 어떤 경우에도 argv에 나타나지 않는다 (env로만 주입).
        assert!(args.iter().all(|arg| !arg.contains("sk-")));
    }

    #[test]
    fn app_server_args_custom이_아니면_키가_있어도_env_key를_만들지_않는다() {
        for llm_override in [None, Some(&CodexLlmOverride::Oss)] {
            let args = codex_app_server_args(llm_override, true).unwrap();
            assert!(
                args.iter().all(|arg| !arg.contains("env_key")),
                "{llm_override:?}에서 env_key가 나오면 안 된다"
            );
        }
    }

    #[test]
    fn app_server_args_custom은_공백_제어문자_base_url을_거부한다() {
        for bad in [
            "",
            "   ",
            "http://a b/v1",
            "http://a\tb",
            "http://a\nb",
            "http://a\u{7}b",
        ] {
            assert!(
                codex_app_server_args(
                    Some(&CodexLlmOverride::Custom {
                        base_url: bad.to_owned(),
                        wire: CodexLlmWire::Responses,
                    }),
                    false,
                )
                .is_err(),
                "{bad:?}는 거부되어야 한다"
            );
        }
        // 앞뒤 공백은 trim 후 통과.
        let args = codex_app_server_args(
            Some(&CodexLlmOverride::Custom {
                base_url: "  http://localhost:8000/v1  ".to_owned(),
                wire: CodexLlmWire::Responses,
            }),
            false,
        )
        .unwrap();
        assert!(
            args.contains(
                &"model_providers.deppy_local.base_url=http://localhost:8000/v1".to_owned()
            )
        );
    }

    #[test]
    fn validate_llm_api_key는_공백_제어문자를_거부한다() {
        for bad in ["", "   ", "sk a", "sk\tb", "sk\nb", "sk\u{7}b"] {
            assert!(
                validate_llm_api_key(bad).is_err(),
                "{bad:?}는 거부되어야 한다"
            );
        }
        // 앞뒤 공백(붙여넣기 잔여물)은 trim 후 통과.
        assert_eq!(
            validate_llm_api_key("  sk-test-123  ").unwrap(),
            "sk-test-123"
        );
    }

    #[test]
    fn llm_override_from_config_변환과_검증() {
        assert_eq!(
            codex_llm_override_from_config(None, None, None).unwrap(),
            None
        );
        assert_eq!(
            codex_llm_override_from_config(Some("oss"), None, None).unwrap(),
            Some(CodexLlmOverride::Oss)
        );
        // wire 미지정/chat → Chat(기본, 변환 프록시 경유), responses → 직결.
        for wire in [None, Some("chat")] {
            assert_eq!(
                codex_llm_override_from_config(Some("custom"), Some("http://h:1/v1"), wire)
                    .unwrap(),
                Some(CodexLlmOverride::Custom {
                    base_url: "http://h:1/v1".to_owned(),
                    wire: CodexLlmWire::Chat,
                })
            );
        }
        assert_eq!(
            codex_llm_override_from_config(
                Some("custom"),
                Some("http://h:1/v1"),
                Some("responses")
            )
            .unwrap(),
            Some(CodexLlmOverride::Custom {
                base_url: "http://h:1/v1".to_owned(),
                wire: CodexLlmWire::Responses,
            })
        );
        // custom인데 base URL이 없거나 잘못되면 조용한 폴백 대신 에러.
        assert!(codex_llm_override_from_config(Some("custom"), None, None).is_err());
        assert!(codex_llm_override_from_config(Some("custom"), Some("a b"), None).is_err());
        // 미지 wire도 에러 (config 로드 정규화가 막지만 spawn 경계 fail-closed).
        assert!(
            codex_llm_override_from_config(Some("custom"), Some("http://h:1/v1"), Some("nope"))
                .is_err()
        );
        // 미지 프로바이더도 에러 (config 로드 정규화가 막지만 spawn 경계 fail-closed).
        assert!(codex_llm_override_from_config(Some("nope"), None, None).is_err());
    }

    #[test]
    fn app_server_args는_chat_wire_직결_조립을_거부한다() {
        // Chat wire는 spawn의 프록시 치환 후(Responses+프록시 주소)에만 argv가 된다 —
        // upstream 직결 argv가 새면 codex가 /v1/responses 404를 만나는 오설정이다.
        let error = codex_app_server_args(
            Some(&CodexLlmOverride::Custom {
                base_url: "http://h:1/v1".to_owned(),
                wire: CodexLlmWire::Chat,
            }),
            false,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("변환 프록시"));
    }

    #[test]
    fn initialize_frame_uses_json_rpc_without_the_jsonrpc_header() {
        let options = CodexAppServerOptions::default();
        let mut state = WorkerState {
            next_request_id: 1,
            ..Default::default()
        };
        let id = state.request_id();
        let frame = json!({
            "method": "initialize",
            "id": id,
            "params": {"clientInfo": {
                "name": options.client_name,
                "title": options.client_title,
                "version": options.client_version,
            }}
        });

        assert_eq!(frame.get("jsonrpc"), None);
        assert_eq!(frame["method"], "initialize");
        assert_eq!(frame["id"], 1);
    }

    #[test]
    fn structured_delta_notification_requires_thread_mapping() {
        let mut state = WorkerState::default();
        state
            .thread_to_session
            .insert("thread-1".to_owned(), "session-1".to_owned());
        let params = json!({
            "threadId": "thread-1",
            "turnId": "turn-1",
            "itemId": "item-1",
            "delta": "hello"
        });

        assert_eq!(
            session_for_params(&params, &state).as_deref(),
            Some("session-1")
        );
        assert_eq!(params["itemId"], "item-1");
    }

    #[test]
    fn approval_response_preserves_the_server_request_id_type() {
        let id = Value::String("request-42".to_owned());
        let frame = json!({"id": id, "result": {"decision": "accept"}});
        assert_eq!(frame["id"], "request-42");
        assert_eq!(frame["result"]["decision"], "accept");
    }

    #[test]
    fn thread_status_notification_emits_authoritative_session_event() {
        let mut state = WorkerState::default();
        state
            .thread_to_session
            .insert("thread-1".to_owned(), "session-1".to_owned());
        let events = EventBacklog::default();

        handle_notification(
            json!({
                "method": "thread/status/changed",
                "params": {
                    "threadId": "thread-1",
                    "status": {
                        "type": "active",
                        "activeFlags": ["waitingOnUserInput"]
                    }
                }
            }),
            &mut state,
            &events,
            &egui::Context::default(),
        );

        assert_eq!(
            events.drain().pop().unwrap(),
            CodexAppServerEvent::Session {
                session_id: "session-1".to_owned(),
                event: AgentSessionEvent::ThreadStatusChanged {
                    status: AgentThreadStatus::Active {
                        waiting_on_approval: false,
                        waiting_on_user_input: true,
                    },
                },
            }
        );
    }

    #[test]
    fn early_thread_status_is_buffered_until_session_mapping_exists() {
        let mut state = WorkerState::default();
        let events = EventBacklog::default();
        handle_notification(
            json!({
                "method": "thread/status/changed",
                "params": {
                    "threadId": "thread-early",
                    "status": {"type": "idle"}
                }
            }),
            &mut state,
            &events,
            &egui::Context::default(),
        );

        assert!(events.drain().is_empty());
        assert_eq!(
            state.buffered_thread_statuses.get("thread-early"),
            Some(&AgentThreadStatus::Idle)
        );
    }

    #[test]
    fn unknown_thread_status_variant_is_ignored_without_transport_failure() {
        let mut state = WorkerState::default();
        state
            .thread_to_session
            .insert("thread-1".to_owned(), "session-1".to_owned());
        let events = EventBacklog::default();
        handle_notification(
            json!({
                "method": "thread/status/changed",
                "params": {
                    "threadId": "thread-1",
                    "status": {"type": "futureStatus", "futureField": true}
                }
            }),
            &mut state,
            &events,
            &egui::Context::default(),
        );

        assert!(events.drain().is_empty());
    }

    #[test]
    fn thread_list_params_use_stable_app_server_filters_and_cursor() {
        let params = thread_list_params(Some("cursor-2".to_owned()), Some(40), false);
        assert_eq!(params["sourceKinds"], json!(["appServer"]));
        assert_eq!(params["archived"], false);
        assert_eq!(params["sortKey"], "updated_at");
        assert_eq!(params["sortDirection"], "desc");
        assert_eq!(params["cursor"], "cursor-2");
        assert_eq!(params["limit"], 40);

        let first_page = thread_list_params(Some(String::new()), None, true);
        assert_eq!(first_page["archived"], true);
        assert!(first_page.get("cursor").is_none());
        assert!(first_page.get("limit").is_none());
    }

    #[test]
    fn thread_resume_params_only_include_non_empty_overrides() {
        let params = thread_resume_params(
            "thread-1".to_owned(),
            Some("/repo".to_owned()),
            Some("gpt-test".to_owned()),
        );
        assert_eq!(
            params,
            json!({"threadId": "thread-1", "cwd": "/repo", "model": "gpt-test"})
        );

        let defaults = thread_resume_params(
            "thread-2".to_owned(),
            Some("  ".to_owned()),
            Some(String::new()),
        );
        assert_eq!(defaults, json!({"threadId": "thread-2"}));
    }

    #[test]
    fn history_response_parsers_accept_exact_shapes_and_reject_mismatches() {
        assert!(
            validate_thread_list_result(&json!({
                "data": [{"id": "thread-1"}],
                "nextCursor": "cursor-2"
            }))
            .is_ok()
        );
        assert!(validate_thread_list_result(&json!({"data": []})).is_ok());
        assert!(validate_thread_list_result(&json!({"data": {}})).is_err());
        assert!(validate_thread_list_result(&json!({"data": [], "nextCursor": 7})).is_err());

        let thread = json!({"thread": {"id": "thread-1", "turns": []}});
        assert!(validate_thread_result(&thread, "thread-1").is_ok());
        assert!(validate_thread_result(&thread, "thread-other").is_err());
        assert!(validate_thread_result(&json!({"thread": {}}), "thread-1").is_err());
    }

    #[test]
    fn stable_catalog_params_use_exact_field_names_and_omit_empty_cursor() {
        assert_eq!(
            model_list_params(Some("cursor-1".to_owned()), Some(50), false),
            json!({"cursor": "cursor-1", "limit": 50, "includeHidden": false})
        );
        assert_eq!(
            model_list_params(Some(String::new()), None, true),
            json!({"includeHidden": true})
        );
        assert_eq!(
            skills_list_params(vec!["/repo".to_owned(), "  ".to_owned()], true),
            json!({"cwds": ["/repo"], "forceReload": true})
        );
    }

    #[test]
    fn turn_start_and_steer_params_include_effort_and_exact_skill_input() {
        let skill = AgentSkillSelection {
            name: "review".to_owned(),
            path: "/repo/.codex/skills/review/SKILL.md".to_owned(),
        };
        let start = turn_start_params(
            "thread-1".to_owned(),
            "check this".to_owned(),
            Some("/repo".to_owned()),
            Some("gpt-test".to_owned()),
            Some("high".to_owned()),
            vec![skill.clone()],
        )
        .unwrap();
        assert_eq!(
            start,
            json!({
                "threadId": "thread-1",
                "input": [
                    {"type": "text", "text": "check this"},
                    {"type": "skill", "name": "review", "path": "/repo/.codex/skills/review/SKILL.md"}
                ],
                "cwd": "/repo",
                "model": "gpt-test",
                "effort": "high"
            })
        );
        assert!(start.get("reasoningEffort").is_none());
        assert!(start.get("threadSettings").is_none());

        assert_eq!(
            turn_steer_params(
                "thread-1".to_owned(),
                "turn-9".to_owned(),
                "prioritize tests".to_owned(),
                vec![skill],
            )
            .unwrap(),
            json!({
                "threadId": "thread-1",
                "expectedTurnId": "turn-9",
                "input": [
                    {"type": "text", "text": "prioritize tests"},
                    {"type": "skill", "name": "review", "path": "/repo/.codex/skills/review/SKILL.md"}
                ]
            })
        );
    }

    #[test]
    fn steer_requires_an_exact_active_turn_mapping() {
        let mut state = WorkerState::default();
        assert!(
            active_turn_ids(&state, "session-1")
                .unwrap_err()
                .to_string()
                .contains("thread")
        );

        state
            .session_to_thread
            .insert("session-1".to_owned(), "thread-1".to_owned());
        assert!(
            active_turn_ids(&state, "session-1")
                .unwrap_err()
                .to_string()
                .contains("실행 중")
        );

        state
            .session_to_turn
            .insert("session-1".to_owned(), "turn-1".to_owned());
        assert_eq!(
            active_turn_ids(&state, "session-1").unwrap(),
            ("thread-1".to_owned(), "turn-1".to_owned())
        );
        assert!(
            turn_steer_params(
                "thread-1".to_owned(),
                "turn-1".to_owned(),
                String::new(),
                Vec::new(),
            )
            .is_err()
        );
    }

    #[test]
    fn stable_model_and_skill_catalog_parsers_are_strict() {
        let models = parse_model_catalog(&json!({
            "data": [{
                "id": "model-id",
                "model": "gpt-test",
                "displayName": "GPT Test",
                "description": "test model",
                "isDefault": true,
                "defaultReasoningEffort": "medium",
                "supportedReasoningEfforts": [
                    {"reasoningEffort": "low", "description": "fast"},
                    {"reasoningEffort": "medium", "description": "balanced"}
                ]
            }],
            "nextCursor": "next-1"
        }))
        .unwrap();
        assert_eq!(models.data[0].model, "gpt-test");
        assert_eq!(models.data[0].default_reasoning_effort, "medium");
        assert_eq!(
            models.data[0].supported_reasoning_efforts[0].reasoning_effort,
            "low"
        );
        assert_eq!(models.next_cursor.as_deref(), Some("next-1"));
        assert!(
            parse_model_catalog(&json!({
                "data": [{
                    "id": "bad", "model": "bad", "displayName": "Bad",
                    "description": "bad", "isDefault": false,
                    "defaultReasoningEffort": "low",
                    "supportedReasoningEfforts": [{"effort": "low", "description": "wrong key"}]
                }]
            }))
            .is_err()
        );

        let skills = parse_skill_catalog(&json!({
            "data": [{
                "cwd": "/repo",
                "errors": [],
                "skills": [{
                    "name": "review", "path": "/skills/review/SKILL.md",
                    "description": "review code", "enabled": true, "scope": "repo"
                }]
            }]
        }))
        .unwrap();
        assert_eq!(skills[0].cwd, "/repo");
        assert_eq!(skills[0].scope, "repo");
        assert!(parse_skill_catalog(&json!({"data": [{"cwd": "/repo", "skills": {}}]})).is_err());
    }

    #[test]
    fn jsonl_reader_accepts_exact_cap_and_rejects_first_extra_byte() {
        let mut exact = vec![b'a'; MAX_STDOUT_LINE_BYTES];
        exact.push(b'\n');
        assert_eq!(
            read_line_limited(&mut std::io::Cursor::new(exact), MAX_STDOUT_LINE_BYTES)
                .unwrap()
                .unwrap()
                .len(),
            MAX_STDOUT_LINE_BYTES
        );

        let oversized = vec![b'b'; MAX_STDOUT_LINE_BYTES + 1];
        let error = read_line_limited(&mut std::io::Cursor::new(oversized), MAX_STDOUT_LINE_BYTES)
            .unwrap_err();
        assert_eq!(io_error_code(&error), "line_too_large");
        assert!(read_line_limited(&mut std::io::Cursor::new([0xff]), 1).is_err());
    }

    #[test]
    fn hostile_stdout_and_stderr_are_sanitized_before_queueing() {
        let (stdout_tx, stdout_rx) = mpsc::sync_channel(1);
        read_json_lines(std::io::Cursor::new(b"SECRET-not-json\n"), stdout_tx);
        assert_eq!(
            stdout_rx.recv().unwrap(),
            Incoming::StdoutError("protocol_error:invalid_json".to_owned())
        );

        let (stderr_tx, stderr_rx) = mpsc::sync_channel(1);
        read_stderr_lines(std::io::Cursor::new(b"SECRET-stderr\n"), stderr_tx);
        assert!(matches!(stderr_rx.recv().unwrap(), Incoming::Stderr));
    }

    #[test]
    fn incoming_reader_disconnect_unblocks_a_stalled_producer() {
        let input = b"{}\n{}\n{}\n".to_vec();
        let (tx, rx) = mpsc::sync_channel(1);
        let reader = thread::spawn(move || read_json_lines(std::io::Cursor::new(input), tx));
        assert!(matches!(rx.recv().unwrap(), Incoming::Json(_)));
        drop(rx);
        reader.join().unwrap();
    }

    #[test]
    fn stdout_error_stops_command_acceptance_state() {
        let mut command = Command::new("sh");
        command
            .args(["-c", "cat >/dev/null"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        configure_process_group(&mut command);
        let mut child = ChildTreeGuard::new(command.spawn().unwrap());
        let mut stdin = child.child_mut().stdin.take().unwrap();
        let (tx, rx) = mpsc::sync_channel(1);
        tx.send(Incoming::StdoutError(
            "protocol_error:invalid_json".to_owned(),
        ))
        .unwrap();
        drop(tx);
        let mut state = WorkerState::default();
        drain_incoming(
            &rx,
            &mut stdin,
            &mut state,
            &EventBacklog::default(),
            &egui::Context::default(),
            &mut false,
        );
        assert!(state.stop_requested);
        child.terminate();
    }

    #[test]
    fn command_queue_is_nonblocking_at_cap_plus_one() {
        let (commands, stalled_receiver) = mpsc::sync_channel(COMMAND_QUEUE_CAPACITY);
        let client = CodexAppServerClient {
            commands,
            events: Arc::new(EventBacklog::default()),
            stop_requested: Arc::new(AtomicBool::new(false)),
            worker: None,
            llm_proxy: None,
        };
        for index in 0..COMMAND_QUEUE_CAPACITY {
            client
                .send(ClientCommand::Interrupt {
                    session_id: format!("session-{index}"),
                })
                .unwrap();
        }
        let error = client
            .send(ClientCommand::Interrupt {
                session_id: "session-overflow".to_owned(),
            })
            .unwrap_err();
        assert_eq!(error.to_string(), COMMAND_BACKPRESSURE_ERROR);
        drop(stalled_receiver);
    }

    #[test]
    fn command_bytes_and_items_reject_cap_plus_one() {
        let exact = ClientCommand::StartSession {
            session_id: "s".to_owned(),
            prompt: "x".repeat(MAX_CLIENT_COMMAND_BYTES - 1),
            cwd: None,
            model: None,
            effort: None,
            skills: Vec::new(),
        };
        assert_eq!(command_retained_bytes(&exact), MAX_CLIENT_COMMAND_BYTES);
        let oversized = ClientCommand::StartSession {
            session_id: "s".to_owned(),
            prompt: "x".repeat(MAX_CLIENT_COMMAND_BYTES),
            cwd: None,
            model: None,
            effort: None,
            skills: Vec::new(),
        };
        assert_eq!(
            command_retained_bytes(&oversized),
            MAX_CLIENT_COMMAND_BYTES + 1
        );
        let too_many_skills = ClientCommand::SteerTurn {
            session_id: "s".to_owned(),
            prompt: "x".to_owned(),
            skills: (0..=MAX_COMMAND_SKILLS)
                .map(|index| AgentSkillSelection {
                    name: format!("skill-{index}"),
                    path: format!("/skill/{index}"),
                })
                .collect(),
        };
        assert!(!command_item_count_is_valid(&too_many_skills));
    }

    fn delta_event(session_id: &str, item_id: &str, delta: String) -> CodexAppServerEvent {
        CodexAppServerEvent::Session {
            session_id: session_id.to_owned(),
            event: AgentSessionEvent::ItemDelta {
                item_id: item_id.to_owned(),
                delta,
            },
        }
    }

    #[test]
    fn adjacent_delta_coalescing_counts_identifier_overhead_exactly() {
        let session_id = "session";
        let item_id = "item";
        let backlog = EventBacklog::default();
        assert!(backlog.push(delta_event(session_id, item_id, "a".to_owned())));
        let remaining = MAX_EVENT_BYTES - session_id.len() - item_id.len() - 1;
        assert!(backlog.push(delta_event(session_id, item_id, "b".repeat(remaining))));
        let events = backlog.drain();
        assert_eq!(events.len(), 1);
        assert_eq!(event_retained_bytes(&events[0]), MAX_EVENT_BYTES);

        let full = EventBacklog::default();
        for _ in 0..EVENT_QUEUE_CAPACITY - 1 {
            assert!(full.push(CodexAppServerEvent::ConnectionStopped));
        }
        assert!(full.push(delta_event(session_id, item_id, "a".to_owned())));
        assert!(!full.push(delta_event(session_id, item_id, "b".repeat(remaining + 1))));
        let events = full.drain();
        assert_eq!(events.len(), EVENT_QUEUE_CAPACITY + 2);
        assert!(matches!(
            events.get(EVENT_QUEUE_CAPACITY),
            Some(CodexAppServerEvent::TransportError { message })
                if message == EVENT_BACKPRESSURE_ERROR
        ));
        assert!(matches!(
            events.last(),
            Some(CodexAppServerEvent::ConnectionStopped)
        ));
        assert!(full.drain().is_empty());
    }

    #[test]
    fn identifier_and_aggregate_retention_have_exact_caps() {
        assert!(valid_identifier(&"i".repeat(MAX_IDENTIFIER_BYTES)));
        assert!(!valid_identifier(&"i".repeat(MAX_IDENTIFIER_BYTES + 1)));

        let mut state = WorkerState::default();
        for index in 0..MAX_TRACKED_SESSIONS {
            let session = format!("session-{index}");
            let thread_id = format!("thread-{index}");
            state.known_sessions.insert(session.clone());
            state
                .thread_to_session
                .insert(thread_id.clone(), session.clone());
            state.session_to_thread.insert(session.clone(), thread_id);
            state
                .session_to_turn
                .insert(session, format!("turn-{index}"));
        }
        let retained = state.retained_identifier_bytes();
        assert!(retained < MAX_RETAINED_IDENTIFIER_BYTES);
        let remaining = MAX_RETAINED_IDENTIFIER_BYTES - retained;
        assert!(state.can_retain_identifier_bytes(remaining));
        assert!(!state.can_retain_identifier_bytes(remaining + 1));
    }

    #[test]
    fn nested_catalog_and_thread_items_have_exact_aggregate_caps() {
        let mut efforts = 0usize;
        for _ in 0..(MAX_CATALOG_ITEMS / MAX_REASONING_EFFORTS_PER_MODEL) {
            add_bounded_items(
                &mut efforts,
                MAX_REASONING_EFFORTS_PER_MODEL,
                MAX_REASONING_EFFORTS_PER_MODEL,
                MAX_CATALOG_ITEMS,
                "effort_limit",
            )
            .unwrap();
        }
        assert_eq!(efforts, MAX_CATALOG_ITEMS);
        assert!(
            add_bounded_items(
                &mut efforts,
                1,
                MAX_REASONING_EFFORTS_PER_MODEL,
                MAX_CATALOG_ITEMS,
                "effort_limit",
            )
            .is_err()
        );

        let exact = Value::Array(vec![Value::Null; MAX_THREAD_RESULT_ITEMS]);
        assert!(json_container_items_within(&exact, MAX_THREAD_RESULT_ITEMS));
        let oversized = Value::Array(vec![Value::Null; MAX_THREAD_RESULT_ITEMS + 1]);
        assert!(!json_container_items_within(
            &oversized,
            MAX_THREAD_RESULT_ITEMS
        ));

        let exact_groups = json!({
            "data": (0..MAX_SKILL_GROUPS)
                .map(|index| json!({"cwd": format!("/{index}"), "skills": []}))
                .collect::<Vec<_>>()
        });
        assert!(parse_skill_catalog(&exact_groups).is_ok());
        let too_many_groups = json!({
            "data": (0..=MAX_SKILL_GROUPS)
                .map(|index| json!({"cwd": format!("/{index}"), "skills": []}))
                .collect::<Vec<_>>()
        });
        assert!(parse_skill_catalog(&too_many_groups).is_err());
    }

    #[test]
    fn item_projection_rejects_nested_cap_plus_one_before_allocation() {
        let mut state = WorkerState::default();
        state
            .thread_to_session
            .insert("thread-1".to_owned(), "session-1".to_owned());
        let events = EventBacklog::default();
        handle_notification(
            json!({
                "method": "item/started",
                "params": {
                    "threadId": "thread-1",
                    "item": {
                        "id": "item-exact",
                        "type": "userMessage",
                        "content": vec![json!({"text": ""}); MAX_CATALOG_ITEMS]
                    }
                }
            }),
            &mut state,
            &events,
            &egui::Context::default(),
        );
        assert!(!state.stop_requested);
        assert!(matches!(
            events.drain().as_slice(),
            [CodexAppServerEvent::Session {
                event: AgentSessionEvent::ItemStarted { .. },
                ..
            }]
        ));

        handle_notification(
            json!({
                "method": "item/completed",
                "params": {
                    "threadId": "thread-1",
                    "item": {
                        "id": "item-over",
                        "type": "fileChange",
                        "changes": vec![json!({}); MAX_CATALOG_ITEMS + 1]
                    }
                }
            }),
            &mut state,
            &events,
            &egui::Context::default(),
        );
        assert!(state.stop_requested);
        assert!(matches!(
            events.drain().as_slice(),
            [CodexAppServerEvent::TransportError { message }]
                if message == "resource_limit:item_projection"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn repeated_process_group_shutdown_reaps_every_cycle() {
        for _ in 0..8 {
            let mut command = Command::new("sh");
            command
                .args(["-c", "sleep 30 & wait"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            configure_process_group(&mut command);
            let mut child = ChildTreeGuard::new(command.spawn().unwrap());
            let process_group = child.child_mut().id() as libc::pid_t;
            child.terminate();
            // SAFETY: signal 0 performs a read-only existence check.
            assert_ne!(unsafe { libc::killpg(process_group, 0) }, 0);
        }
    }

    #[cfg(unix)]
    #[test]
    fn exited_parent_is_detected_without_reap_while_descendant_holds_stdout() {
        let mut command = Command::new("sh");
        command
            .args(["-c", "sleep 30 & exit 0"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        configure_process_group(&mut command);
        let mut child = ChildTreeGuard::new(command.spawn().unwrap());
        let process_group = child.child_mut().id() as libc::pid_t;
        let inherited_stdout = child.child_mut().stdout.take().unwrap();
        let mut exited = false;
        for _ in 0..100 {
            if child.leader_exited_without_reap().unwrap() {
                exited = true;
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        assert!(
            exited,
            "parent must become waitable without closing descendant pipe"
        );
        child.terminate();
        drop(inherited_stdout);
        let mut group_gone = false;
        for _ in 0..100 {
            // SAFETY: signal 0 performs a read-only existence check.
            if unsafe { libc::killpg(process_group, 0) } != 0 {
                group_gone = true;
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        assert!(group_gone, "killed descendant group must be collected");
    }

    #[test]
    fn production_source_has_only_bounded_queue_and_guarded_process_paths() {
        let source = include_str!("codex_app_server.rs");
        let production = source.split("#[cfg(test)]").next().unwrap();
        assert!(!production.contains("mpsc::channel()"));
        assert!(!production.contains("discard_until_newline"));
        assert!(!production.contains(".expect(\"Codex App Server stdout reader"));
        assert!(!production.contains(".expect(\"Codex App Server stderr reader"));
        assert!(production.contains("mpsc::sync_channel(COMMAND_QUEUE_CAPACITY)"));
        assert!(production.contains("mpsc::sync_channel(INCOMING_QUEUE_CAPACITY)"));
        assert!(production.contains("ChildTreeGuard::new(child)"));
        assert!(production.contains("configure_process_group(&mut command)"));
        assert!(production.contains("libc::WNOWAIT"));
        assert!(production.contains("child.terminate();\n    drop(incoming_rx);"));
        assert!(production.contains("EVENT_BACKPRESSURE_ERROR"));
        assert!(!production.contains(".get(\"result\").cloned()"));
        let worker = production
            .split("fn worker_loop(")
            .nth(1)
            .unwrap()
            .split("fn spawn_stdout_reader")
            .next()
            .unwrap();
        assert!(!worker.contains("try_wait"));
    }
}
