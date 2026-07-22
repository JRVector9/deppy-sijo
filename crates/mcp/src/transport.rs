//! stdio JSON-RPC 2.0 transport (설계문서 §1.5, 기준 스펙 2025-11-25).
//! MCP stdio: client가 server를 subprocess로 실행하고 newline-delimited
//! JSON-RPC를 주고받는다. server stdout에는 valid MCP message만 허용된다 —
//! 위반 라인이 나오면 그 즉시 읽기를 중단하고 연결을 오염(poison) 처리한다
//! (PR-15 완료 기준: stdout valid MCP only).
//! stderr는 별도 thread에서 캡처해 RedactionService로 redact 후 보관한다
//! (PR-15 완료 기준: stderr capture/redaction).

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TryRecvError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use secret::RedactionService;
use serde_json::{Value, json};

use crate::http::{McpDeliveryUnknown, McpServerResponseError};
use crate::limits::MAX_RAW_MCP_RESPONSE_BYTES;
use crate::metrics::{ThreadGuard, ThreadKind};
use crate::sensitive::SensitiveBytes;

/// stdout 한 줄 최대 길이 — newline 없는 무한 스트림으로 인한 메모리 폭주 방지
const MAX_LINE_BYTES: usize = MAX_RAW_MCP_RESPONSE_BYTES;
/// stdin으로 보내는 JSON-RPC 한 줄 최대 길이. 큰 tools/call payload는 pipe를
/// 채워 blocking write가 될 수 있으므로 transport 경계에서 한 번 더 제한한다.
pub(crate) const MAX_WRITE_LINE_BYTES: usize = 64 * 1024;
/// redacted stderr 보관 상한 (초과분은 버리고 truncation 표식만 남긴다)
const STDERR_LOG_CAP: usize = 256 * 1024;
/// 위반 라인을 에러 메시지에 실을 때 최대 문자 수 (redact 후에도 길이는 제한)
const VIOLATION_SNIPPET_CHARS: usize = 200;

/// redacted stderr 로그 버퍼 — stderr thread가 채우고 stderr_log()가 읽는다.
type SharedStderrLog = Arc<Mutex<Vec<u8>>>;

/// Process ownership shared with a cloneable cancellation handle. The request thread never holds
/// this lock while waiting for stdout, so another thread can kill and reap a hung stdio backend
/// without first acquiring `&mut McpConnection`.
#[derive(Clone)]
pub(crate) struct StdioCancellationHandle {
    process: Arc<Mutex<Option<Child>>>,
}

impl std::fmt::Debug for StdioCancellationHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("StdioCancellationHandle")
    }
}

impl StdioCancellationHandle {
    pub(crate) fn cancel(&self) -> Option<std::process::ExitStatus> {
        let mut process = self.process.lock().expect("MCP stdio process lock");
        let mut child = process.take()?;
        kill_and_reap(&mut child)
    }

    #[cfg(test)]
    fn is_reaped(&self) -> bool {
        self.process
            .lock()
            .expect("MCP stdio process lock")
            .is_none()
    }
}

/// stdout reader thread → 요청자에게 전달되는 이벤트.
/// 채널 disconnect는 EOF(서버 종료 또는 stdout 닫힘)를 뜻한다 — pty crate와 동일 규약.
enum ReaderEvent {
    /// JSON-RPC 검증을 통과한 메시지
    Message(Value),
    /// stdout 프로토콜 위반 — 이 이벤트 이후 reader는 더 읽지 않는다
    Violation(String),
}

/// Distinguishes deterministic failures before any request byte can be written from failures
/// where `write_all` may already have delivered a prefix (or the full request) to the server.
enum WriteLineFailure {
    BeforeIo(anyhow::Error),
    MaybeSent(anyhow::Error),
}

impl WriteLineFailure {
    fn into_error(self) -> anyhow::Error {
        match self {
            Self::BeforeIo(error) | Self::MaybeSent(error) => error,
        }
    }
}

struct StdioWiring {
    stdin: ChildStdin,
    events: Receiver<ReaderEvent>,
    stderr_log: SharedStderrLog,
    stdout_thread: JoinHandle<()>,
    stderr_thread: JoinHandle<()>,
}

/// local stdio MCP 서버 하나와의 JSON-RPC 연결.
/// 요청은 단일 스레드(호출측)에서 순차 실행을 전제한다 — v0 discovery flow(§1.5).
#[derive(Debug)]
pub(crate) struct StdioClient {
    process: Arc<Mutex<Option<Child>>>,
    stdin: Option<ChildStdin>,
    events: Option<Receiver<ReaderEvent>>,
    stdout_thread: Option<JoinHandle<()>>,
    stderr_thread: Option<JoinHandle<()>>,
    next_id: u64,
    request_timeout: Duration,
    stderr_log: SharedStderrLog,
    /// 위반 발생 후 이 연결은 재사용 금지 (stdout 신뢰 불가)
    violation: Option<String>,
    #[cfg(test)]
    write_drop_observer: Option<Arc<std::sync::atomic::AtomicBool>>,
}

impl StdioClient {
    /// MCP 서버 subprocess spawn + stdout/stderr reader thread 기동.
    /// scoped env는 여기에만 적용한다. inherit_env=false면 parent env를 지우고 명시 env만 주입한다.
    #[cfg(test)]
    pub(crate) fn spawn(
        command: &str,
        args: &[String],
        env: &[(String, String)],
        inherit_env: bool,
        redaction: &RedactionService,
        request_timeout: Duration,
    ) -> anyhow::Result<Self> {
        Self::spawn_with_secret_env(
            command,
            args,
            env,
            &[],
            inherit_env,
            redaction,
            request_timeout,
        )
    }

    pub(crate) fn spawn_with_secret_env(
        command: &str,
        args: &[String],
        plain_env: &[(String, String)],
        secret_env: &[(String, secret::SecretString)],
        inherit_env: bool,
        redaction: &RedactionService,
        request_timeout: Duration,
    ) -> anyhow::Result<Self> {
        let mut builder = Command::new(command);
        if !inherit_env {
            builder.env_clear();
        }
        builder
            .args(args)
            .envs(plain_env.iter().map(|(key, value)| (key, value)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in secret_env {
            builder.env(key, value.expose());
        }
        // 자체 process group으로 분리 — command가 wrapper 셸이어도 종료 시
        // grandchild까지 killpg로 정리할 수 있게 한다 (pty crate의 killpg 관행).
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut builder, 0);
        let mut child = builder
            .spawn()
            .with_context(|| format!("MCP 서버 실행 실패: {command}"))?;
        // spawn 이후 배선(pipe 인수/thread 기동)이 실패하면 child가 StdioClient에
        // 들어가기 전이므로 Drop 정리가 없다 — 여기서 직접 kill + reap 한다.
        match Self::wire(&mut child, redaction) {
            Ok(wiring) => Ok(Self {
                process: Arc::new(Mutex::new(Some(child))),
                stdin: Some(wiring.stdin),
                events: Some(wiring.events),
                stdout_thread: Some(wiring.stdout_thread),
                stderr_thread: Some(wiring.stderr_thread),
                next_id: 1,
                request_timeout,
                stderr_log: wiring.stderr_log,
                violation: None,
                #[cfg(test)]
                write_drop_observer: None,
            }),
            Err(error) => {
                kill_and_reap(&mut child);
                Err(error)
            }
        }
    }

    /// pipe 인수 + stdout/stderr reader thread 기동.
    /// 실패 시 child 정리는 호출측(spawn)이 담당한다.
    fn wire(child: &mut Child, redaction: &RedactionService) -> anyhow::Result<StdioWiring> {
        let stdin = child.stdin.take().context("MCP 서버 stdin pipe 없음")?;
        let stdout = child.stdout.take().context("MCP 서버 stdout pipe 없음")?;
        let mut stderr = child.stderr.take().context("MCP 서버 stderr pipe 없음")?;

        // stdout reader: 라인 → JSON-RPC 검증 → 이벤트 채널.
        // bounded 채널: 소비가 느리면 reader가 send에서 블록 → 표준 backpressure.
        let (tx, rx) = sync_channel(64);
        let stdout_redaction = redaction.clone();
        let stdout_thread = std::thread::Builder::new()
            .name("mcp-stdout".into())
            .spawn(move || {
                let _thread = ThreadGuard::enter(ThreadKind::StdioStdout);
                read_stdout(stdout, tx, stdout_redaction);
            })
            .context("mcp stdout thread 생성 실패")?;

        // stderr reader: chunk 단위 캡처 → redact → 보관.
        // 라인이 아닌 chunk 단위로 StreamRedactor에 넣어 chunk 경계 secret도 잡는다.
        // 알려진 한계: StreamRedactor의 carry cap(16KiB)보다 긴 secret이 chunk
        // 경계에 걸리면 앞부분이 배출될 수 있다 — corpus 확장은 PR-22(secret crate).
        let stderr_log = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&stderr_log);
        let mut redactor = redaction.stream_redactor();
        let stderr_thread = std::thread::Builder::new()
            .name("mcp-stderr".into())
            .spawn(move || {
                let _thread = ThreadGuard::enter(ThreadKind::StdioStderr);
                let mut buf = [0u8; 8192];
                loop {
                    match stderr.read(&mut buf) {
                        Ok(0) | Err(_) => break, // EOF
                        Ok(n) => append_capped(&log, redactor.redact_chunk(&buf[..n])),
                    }
                }
                // 스트림 종료 — redactor carry에 남은 꼬리를 마지막 검사 후 배출
                append_capped(&log, redactor.flush());
            })
            .context("mcp stderr thread 생성 실패")?;

        Ok(StdioWiring {
            stdin,
            events: rx,
            stderr_log,
            stdout_thread,
            stderr_thread,
        })
    }

    /// JSON-RPC request 전송 → 같은 id의 response 대기 (result 반환).
    pub(crate) fn request(&mut self, method: &str, params: Value) -> anyhow::Result<Value> {
        if let Some(violation) = &self.violation {
            bail!("{violation}");
        }
        // 요청 전에 이미 도착해 있는 response는 존재하지 않는 요청에 대한 응답이다 —
        // 서버가 미래 id를 선점해 가짜 응답을 심는 것을 막는다 (상관 위반 처리).
        // 전송 이후 도착분은 실제 응답과 구별 불가능하므로 여기까지가 검사 한계.
        self.drain_unsolicited()?;
        let id = self.next_id;
        self.next_id += 1;
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        match self.write_line(msg) {
            Ok(()) => {}
            Err(WriteLineFailure::BeforeIo(error)) => {
                return Err(error).with_context(|| format!("{method} 요청 전송 실패"));
            }
            Err(WriteLineFailure::MaybeSent(error)) if method == "tools/call" => {
                drop(error);
                tracing::debug!(
                    "tools/call stdio write outcome unknown; automatic retry forbidden"
                );
                return Err(anyhow::Error::new(McpDeliveryUnknown { status: None }));
            }
            Err(error) => {
                return Err(error.into_error()).with_context(|| format!("{method} 요청 전송 실패"));
            }
        }

        let response = self.wait_response(id, method);
        if method != "tools/call" {
            return response;
        }
        match response {
            Ok(value) => Ok(value),
            Err(error) if error.downcast_ref::<McpServerResponseError>().is_some() => Err(error),
            Err(error) => {
                drop(error);
                tracing::debug!(
                    "tools/call stdio response outcome unknown; automatic retry forbidden"
                );
                Err(anyhow::Error::new(McpDeliveryUnknown { status: None }))
            }
        }
    }

    /// JSON-RPC notification 전송 (응답 없음).
    pub(crate) fn notify(&mut self, method: &str, params: Value) -> anyhow::Result<()> {
        if let Some(violation) = &self.violation {
            bail!("{violation}");
        }
        let msg = json!({"jsonrpc": "2.0", "method": method, "params": params});
        self.write_line(msg)
            .map_err(WriteLineFailure::into_error)
            .with_context(|| format!("{method} notification 전송 실패"))
    }

    /// 지금까지 캡처된 redacted stderr (lossy UTF-8).
    pub(crate) fn stderr_log(&self) -> String {
        let log = self.stderr_log.lock().expect("stderr log lock");
        String::from_utf8_lossy(&log).into_owned()
    }

    /// 요청 전 대기열 정리: outstanding 요청이 없는 시점에 도착한 response는
    /// 상관 위반으로 오염 처리하고, server발 request/notification은 무시한다.
    fn drain_unsolicited(&mut self) -> anyhow::Result<()> {
        loop {
            let events = self.events.as_ref().context("MCP stdout이 이미 닫힘")?;
            match events.try_recv() {
                Ok(ReaderEvent::Message(value)) => {
                    if value.get("method").is_some() {
                        debug_unsupported_server_message(&value, true);
                        continue;
                    }
                    let desc =
                        "stdout 프로토콜 위반: outstanding 요청이 없는데 response 수신".to_owned();
                    self.violation = Some(desc.clone());
                    bail!("{desc}");
                }
                Ok(ReaderEvent::Violation(desc)) => {
                    self.violation = Some(desc.clone());
                    bail!("{desc}");
                }
                // Disconnected(서버 종료)는 이후 write/wait 단계가 에러로 알린다
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return Ok(()),
            }
        }
    }

    /// 한 줄 JSON-RPC 전송. stdin write도 timeout을 적용한다 — MCP 서버가 stdin을
    /// 읽지 않거나 큰 payload로 pipe가 차도 호출 thread가 영구 block되지 않아야 한다.
    fn write_line(&mut self, msg: Value) -> Result<(), WriteLineFailure> {
        let mut line = SensitiveBytes::from_json(msg)
            .map_err(|error| WriteLineFailure::BeforeIo(anyhow::Error::new(error)))?;
        line.push(b'\n');
        if line.as_slice().len() > MAX_WRITE_LINE_BYTES {
            return Err(WriteLineFailure::BeforeIo(anyhow::anyhow!(
                "MCP 요청 크기 초과: {} bytes (max {MAX_WRITE_LINE_BYTES})",
                line.as_slice().len()
            )));
        }
        #[cfg(test)]
        if let Some(observer) = self.write_drop_observer.take() {
            line.observe_zeroized_drop(observer);
        }
        let mut stdin = self
            .stdin
            .take()
            .ok_or_else(|| WriteLineFailure::BeforeIo(anyhow::anyhow!("stdin이 이미 닫힘")))?;
        let (tx, rx) = sync_channel(1);
        let writer = std::thread::Builder::new()
            .name("mcp-stdin-write".into())
            .spawn(move || {
                let _thread = ThreadGuard::enter(ThreadKind::StdioWriter);
                let result = stdin
                    .write_all(line.as_slice())
                    .and_then(|()| stdin.flush());
                drop(line);
                let _ = tx.send(result.map(|()| stdin));
            })
            .map_err(|error| {
                WriteLineFailure::BeforeIo(
                    anyhow::Error::new(error).context("mcp stdin write thread 생성 실패"),
                )
            })?;
        match rx.recv_timeout(self.request_timeout) {
            Ok(Ok(stdin)) => {
                let _ = writer.join();
                self.stdin = Some(stdin);
                Ok(())
            }
            Ok(Err(e)) => {
                let _ = writer.join();
                Err(WriteLineFailure::MaybeSent(e.into()))
            }
            Err(RecvTimeoutError::Timeout) => {
                let status = self.terminate();
                let _ = writer.join();
                Err(WriteLineFailure::MaybeSent(anyhow::anyhow!(
                    "MCP stdin write timeout ({:?}, exit: {status:?})",
                    self.request_timeout
                )))
            }
            Err(RecvTimeoutError::Disconnected) => {
                let _ = writer.join();
                Err(WriteLineFailure::MaybeSent(anyhow::anyhow!(
                    "MCP stdin write thread 종료"
                )))
            }
        }
    }

    /// id가 일치하는 response를 기다린다 (request/response 상관).
    /// 요청은 한 번에 하나만 outstanding이므로(v0 순차 flow) 다른 id의 response는
    /// 존재하지 않는 요청에 대한 응답 — JSON-RPC 상관 규칙 위반으로 오염 처리한다.
    fn wait_response(&mut self, id: u64, method: &str) -> anyhow::Result<Value> {
        let deadline = Instant::now() + self.request_timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let status = self.terminate();
                bail!(
                    "{method} 응답 timeout ({:?}, exit: {status:?})",
                    self.request_timeout
                );
            }
            let event = self
                .events
                .as_ref()
                .context("MCP stdout이 이미 닫힘")?
                .recv_timeout(remaining);
            match event {
                Ok(ReaderEvent::Message(value)) => {
                    if value.get("method").is_some() {
                        // server발 메시지. notification은 무시하되, id가 있는
                        // request(ping 등)는 응답을 기다리며 교착할 수 있으므로
                        // method-not-found로 회신한다 (codex 리뷰 반영).
                        // 원문은 로그에 싣지 않는다 — params에 secret 가능 (§7).
                        if let Some(request_id) = value.get("id").cloned() {
                            let reply = serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": request_id,
                                "error": {"code": -32601, "message": "method not found"},
                            });
                            let _ = self.write_line(reply);
                        }
                        debug_unsupported_server_message(&value, false);
                        continue;
                    }
                    // response — 유일한 outstanding 요청의 id와 일치해야 한다
                    if value.get("id").and_then(Value::as_u64) == Some(id) {
                        return unwrap_response(value, method);
                    }
                    let desc =
                        format!("stdout 프로토콜 위반: 요청하지 않은 id의 response (기대 id {id})");
                    self.violation = Some(desc.clone());
                    bail!("{desc}");
                }
                Ok(ReaderEvent::Violation(desc)) => {
                    self.violation = Some(desc.clone());
                    bail!("{desc}");
                }
                Err(RecvTimeoutError::Timeout) => {
                    let status = self.terminate();
                    bail!(
                        "{method} 응답 timeout ({:?}, exit: {status:?})",
                        self.request_timeout
                    );
                }
                Err(RecvTimeoutError::Disconnected) => {
                    // stdout이 닫혔다 — 서버가 죽었거나 죽어가는 중. 이 자리에서
                    // 그룹까지 정리한다. 순서가 핵심: killpg → reap.
                    // reap 전(zombie) PID는 커널이 재사용하지 않으므로 killpg가 안전하고,
                    // wrapper가 남긴 grandchild도 그룹 정리로 함께 끝난다 (codex 리뷰 2건 동시 해소)
                    let status = self.terminate();
                    bail!("MCP 서버가 {method} 응답 전에 종료됨 (exit: {status:?})");
                }
            }
        }
    }

    pub(crate) fn cancel(&mut self) {
        self.terminate();
    }

    pub(crate) fn cancellation_handle(&self) -> StdioCancellationHandle {
        StdioCancellationHandle {
            process: Arc::clone(&self.process),
        }
    }

    fn terminate(&mut self) -> Option<std::process::ExitStatus> {
        drop(self.stdin.take());
        let status = self.cancellation_handle().cancel();
        // A reader may be blocked on the bounded channel. Drop the receiver before
        // joining so the send fails, then wait for both pipe-owning threads to exit.
        drop(self.events.take());
        if let Some(thread) = self.stdout_thread.take() {
            let _ = thread.join();
        }
        if let Some(thread) = self.stderr_thread.take() {
            let _ = thread.join();
        }
        status
    }
}

fn debug_unsupported_server_message(value: &Value, ignored: bool) {
    let method = value.get("method").and_then(Value::as_str).unwrap_or("?");
    if ignored {
        tracing::debug!(method = %method, "server발 MCP 메시지 무시 (v0 미지원)");
    } else {
        tracing::debug!(method = %method, "server발 MCP 메시지 (v0 미지원)");
    }
}

impl Drop for StdioClient {
    /// 연결을 버릴 때 서버 프로세스를 정리한다 (pty crate와 동일 규약).
    /// reader thread들은 pipe EOF로 스스로 끝난다.
    fn drop(&mut self) {
        self.terminate();
    }
}

/// 서버 process group 전체를 SIGKILL 후 direct child를 reap한다.
/// spawn에서 process_group(0)으로 분리했으므로 pgid == child pid.
/// killpg(그룹 정리) → kill → wait(reap) 순서 고정. reap 전에는 PID가
/// 재사용되지 않으므로 이 순서에서만 killpg가 안전하다 — 호출측은 reap 이후
/// 이미 공유 process slot에서 꺼낸 child에는 다시 호출하면 안 된다. wait 결과를 돌려준다.
fn kill_and_reap(child: &mut Child) -> Option<std::process::ExitStatus> {
    #[cfg(unix)]
    unsafe {
        libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        // process group이 없는 Windows에서 grandchild(wrapper가 띄운 실제 서버)까지
        // 트리로 정리한다 (codex 리뷰 반영 — credential env를 가진 orphan 방지)
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &child.id().to_string(), "/T", "/F"])
            .output();
    }
    let _ = child.kill();
    child.wait().ok() // zombie 방지 reap
}

/// response에서 result를 꺼낸다. error response는 에러로 변환.
fn unwrap_response(mut value: Value, method: &str) -> anyhow::Result<Value> {
    let obj = value.as_object_mut().context("응답이 JSON object가 아님")?;
    if let Some(err) = obj.get("error") {
        let _ = method;
        return Err(anyhow::Error::new(McpServerResponseError::from_code(
            err.get("code").and_then(Value::as_i64),
        )));
    }
    obj.remove("result")
        .with_context(|| format!("{method} 응답에 result 없음"))
}

/// stdout reader 본체. 위반을 만나면 Violation을 보내고 즉시 중단한다 —
/// 위반 이후의 stdout은 신뢰할 수 없으므로 파싱을 계속하지 않는다(거부).
fn read_stdout(stdout: ChildStdout, tx: SyncSender<ReaderEvent>, redaction: RedactionService) {
    let mut reader = BufReader::new(stdout);
    loop {
        let line = match read_line_capped(&mut reader, MAX_LINE_BYTES) {
            LineRead::Line(line) => line,
            LineRead::Eof | LineRead::Io => break, // 채널 drop으로 EOF 전파
            LineRead::TooLong => {
                let _ = tx.send(ReaderEvent::Violation(format!(
                    "stdout 프로토콜 위반: 라인 길이 상한({MAX_LINE_BYTES} bytes) 초과"
                )));
                break;
            }
        };
        match validate_jsonrpc(&line) {
            Ok(value) => {
                if tx.send(ReaderEvent::Message(value)).is_err() {
                    break; // 수신측이 사라짐
                }
            }
            Err(reason) => {
                let _ = tx.send(ReaderEvent::Violation(violation_desc(
                    &line, &reason, &redaction,
                )));
                break;
            }
        }
    }
}

/// 위반 라인을 에러 메시지에 싣기 전에 redact + 길이 제한한다 —
/// 서버가 stdout에 실수로 흘린 secret이 에러 문자열로 전파되지 않도록.
fn violation_desc(line: &[u8], reason: &str, redaction: &RedactionService) -> String {
    let mut redactor = redaction.stream_redactor();
    let mut redacted = redactor.redact_chunk(line);
    redacted.extend(redactor.flush());
    let text = String::from_utf8_lossy(&redacted);
    let snippet: String = text.chars().take(VIOLATION_SNIPPET_CHARS).collect();
    format!("stdout 프로토콜 위반 ({reason}): {snippet}")
}

/// stdout에 허용되는 message인지 검증 (§1.5 stdout protocol strictness).
/// JSON-RPC 2.0 단일 메시지만 허용: request/notification(method) 또는
/// response(id + result xor error). batch 배열은 2025 스펙에서 금지 — 거부.
fn validate_jsonrpc(line: &[u8]) -> Result<Value, String> {
    let text = std::str::from_utf8(line).map_err(|_| "UTF-8 아님".to_owned())?;
    let value: Value =
        serde_json::from_str(text).map_err(|error| format!("JSON 파싱 실패: {error}"))?;
    let Some(obj) = value.as_object() else {
        return Err("JSON object가 아님 (batch 배열 금지)".to_owned());
    };
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err("jsonrpc 필드가 \"2.0\"이 아님".to_owned());
    }
    let has_method = obj.get("method").is_some_and(Value::is_string);
    let has_result = obj.contains_key("result");
    let error_field = obj.get("error");
    let has_error = error_field.is_some();
    // error response의 error는 {code: number, message: string} object여야 한다
    let error_shape_ok = error_field.is_none_or(|err| {
        err.get("code").is_some_and(Value::is_number)
            && err.get("message").is_some_and(Value::is_string)
    });
    let valid = if has_method {
        // request/notification — result/error 동반 금지, id가 있으면 string|number
        !has_result && !has_error && obj.get("id").is_none_or(|id| is_valid_id(id, false))
    } else {
        // response — id 필수(string|number, error response만 null 허용),
        // result와 error 중 정확히 하나
        (has_result ^ has_error)
            && error_shape_ok
            && obj.get("id").is_some_and(|id| is_valid_id(id, has_error))
    };
    if valid {
        Ok(value)
    } else {
        Err("JSON-RPC 메시지 형태가 아님".to_owned())
    }
}

/// JSON-RPC id 타입 검사 — string 또는 number.
/// null은 파싱 불가 요청에 대한 error response에서만 허용된다 (JSON-RPC 2.0).
fn is_valid_id(id: &Value, allow_null: bool) -> bool {
    // number id는 클라이언트 상관 경로(as_u64)가 처리 가능한 비음수 정수만 허용 —
    // 음수/분수 id가 "valid stdout"으로 통과한 뒤 상관 위반이 되는 것 방지 (codex 리뷰)
    id.is_string() || id.as_u64().is_some() || (allow_null && id.is_null())
}

enum LineRead {
    Line(Vec<u8>),
    Eof,
    TooLong,
    Io,
}

/// newline까지 한 줄을 읽되 max를 넘으면 중단한다 (버퍼링 전에 상한 검사).
fn read_line_capped(reader: &mut impl BufRead, max: usize) -> LineRead {
    let mut line = Vec::new();
    loop {
        let buf = match reader.fill_buf() {
            Ok(buf) => buf,
            Err(_) => return LineRead::Io,
        };
        if buf.is_empty() {
            // EOF — newline 없이 끝난 마지막 라인도 메시지로 취급
            return if line.is_empty() {
                LineRead::Eof
            } else {
                LineRead::Line(line)
            };
        }
        if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            line.extend_from_slice(&buf[..pos]);
            reader.consume(pos + 1);
            return if line.len() > max {
                LineRead::TooLong
            } else {
                LineRead::Line(line)
            };
        }
        line.extend_from_slice(buf);
        let n = buf.len();
        reader.consume(n);
        if line.len() > max {
            return LineRead::TooLong;
        }
    }
}

/// redacted stderr chunk를 상한까지만 보관한다.
fn append_capped(log: &Mutex<Vec<u8>>, chunk: Vec<u8>) {
    if chunk.is_empty() {
        return;
    }
    let mut log = log.lock().expect("stderr log lock");
    if log.len() >= STDERR_LOG_CAP {
        return; // 이미 상한 — 이후는 버림 (truncation 표식은 최초 1회에 남았다)
    }
    let room = STDERR_LOG_CAP - log.len();
    if chunk.len() > room {
        log.extend_from_slice(&chunk[..room]);
        log.extend_from_slice(b"\n[stderr truncated]");
    } else {
        log.extend_from_slice(&chunk);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use secret::RedactionService;
    use std::io::Cursor;
    use std::sync::{Arc, Mutex};
    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id, Record};
    use tracing::{Event, Metadata, Subscriber};

    #[cfg(unix)]
    fn scripted_stdio(script: &str, timeout: Duration) -> StdioClient {
        StdioClient::spawn(
            "/bin/sh",
            &["-c".to_owned(), script.to_owned()],
            &[],
            true,
            &RedactionService::new(),
            timeout,
        )
        .unwrap()
    }

    #[cfg(unix)]
    fn assert_delivery_unknown(script: &str, timeout: Duration, params: Value) {
        let mut client = scripted_stdio(script, timeout);
        let observer = Arc::new(std::sync::atomic::AtomicBool::new(false));
        client.write_drop_observer = Some(Arc::clone(&observer));
        let error = client.request("tools/call", params).unwrap_err();
        assert!(
            error.downcast_ref::<McpDeliveryUnknown>().is_some(),
            "unexpected error: {error:#}"
        );
        assert!(observer.load(std::sync::atomic::Ordering::Acquire));
    }

    #[cfg(unix)]
    #[test]
    fn tools_call_success_zeroizes_serialized_write_body() {
        let mut client = scripted_stdio(
            "IFS= read -r _request; printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"isError\":false}}'",
            Duration::from_secs(1),
        );
        let observer = Arc::new(std::sync::atomic::AtomicBool::new(false));
        client.write_drop_observer = Some(Arc::clone(&observer));
        client
            .request(
                "tools/call",
                json!({"name":"ok","arguments":{"secret":"value"}}),
            )
            .unwrap();
        assert!(observer.load(std::sync::atomic::Ordering::Acquire));
    }

    #[cfg(unix)]
    #[test]
    fn tools_call_timeout_after_write_is_delivery_unknown() {
        assert_delivery_unknown(
            "IFS= read -r _request; sleep 30",
            Duration::from_millis(50),
            json!({"name":"slow","arguments":{}}),
        );
    }

    #[cfg(unix)]
    #[test]
    fn tools_call_eof_after_write_is_delivery_unknown() {
        assert_delivery_unknown(
            "IFS= read -r _request; exit 0",
            Duration::from_secs(1),
            json!({"name":"exit","arguments":{}}),
        );
    }

    #[cfg(unix)]
    #[test]
    fn tools_call_protocol_violation_after_write_is_delivery_unknown() {
        assert_delivery_unknown(
            "IFS= read -r _request; printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":999,\"result\":{}}'",
            Duration::from_secs(1),
            json!({"name":"wrong-id","arguments":{}}),
        );
    }

    #[cfg(unix)]
    #[test]
    fn tools_call_partial_write_or_eof_is_delivery_unknown() {
        let payload = "x".repeat(60 * 1024);
        assert_delivery_unknown(
            "dd bs=1 count=8 >/dev/null 2>/dev/null; exit 0",
            Duration::from_secs(1),
            json!({"name":"partial","arguments":{"payload":payload}}),
        );
    }

    #[cfg(unix)]
    #[test]
    fn tools_call_external_cancel_after_write_is_delivery_unknown() {
        let mut client = scripted_stdio("IFS= read -r _request; sleep 30", Duration::from_secs(2));
        let cancellation = client.cancellation_handle();
        let cancel = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            cancellation.cancel();
        });

        let error = client
            .request("tools/call", json!({"name":"cancel","arguments":{}}))
            .unwrap_err();
        cancel.join().unwrap();
        assert!(error.downcast_ref::<McpDeliveryUnknown>().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn tools_call_pre_io_size_rejection_stays_failed() {
        let mut client = scripted_stdio("sleep 30", Duration::from_secs(1));
        let payload = "x".repeat(MAX_WRITE_LINE_BYTES);
        let error = client
            .request(
                "tools/call",
                json!({"name":"too-large","arguments":{"payload":payload}}),
            )
            .unwrap_err();
        assert!(error.downcast_ref::<McpDeliveryUnknown>().is_none());
        assert!(format!("{error:#}").contains("요청 크기 초과"));
    }

    #[cfg(unix)]
    #[test]
    fn tools_call_jsonrpc_error_is_known_failed_not_unknown() {
        let mut client = scripted_stdio(
            "IFS= read -r _request; printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32000,\"message\":\"failed\"}}'",
            Duration::from_secs(1),
        );
        let error = client
            .request("tools/call", json!({"name":"known","arguments":{}}))
            .unwrap_err();
        assert!(error.downcast_ref::<McpDeliveryUnknown>().is_none());
        assert_eq!(
            error
                .downcast_ref::<McpServerResponseError>()
                .and_then(|error| error.code()),
            Some(-32000)
        );
    }

    #[cfg(unix)]
    #[test]
    fn explicit_cancel_kills_reaps_and_joins_stdio_resources() {
        let started = Instant::now();
        let mut client = StdioClient::spawn(
            "/bin/sh",
            &["-c".to_owned(), "sleep 30".to_owned()],
            &[],
            true,
            &RedactionService::new(),
            Duration::from_secs(5),
        )
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let active = crate::metrics::transport_metrics();
            if active.stdio_stdout_threads >= 1 && active.stdio_stderr_threads >= 1 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "stdio reader metrics not observed"
            );
            std::thread::yield_now();
        }

        client.cancel();

        assert!(client.cancellation_handle().is_reaped());
        assert!(client.stdin.is_none());
        assert!(client.events.is_none());
        assert!(client.stdout_thread.is_none());
        assert!(client.stderr_thread.is_none());
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn jsonrpc_검증() {
        // 허용: response(result/error), notification, server request
        assert!(validate_jsonrpc(br#"{"jsonrpc":"2.0","id":1,"result":{}}"#).is_ok());
        assert!(
            validate_jsonrpc(br#"{"jsonrpc":"2.0","id":1,"error":{"code":-1,"message":"x"}}"#)
                .is_ok()
        );
        assert!(validate_jsonrpc(br#"{"jsonrpc":"2.0","method":"notifications/message"}"#).is_ok());
        assert!(validate_jsonrpc(br#"{"jsonrpc":"2.0","id":9,"method":"ping"}"#).is_ok());
        // 거부: 평문, jsonrpc 누락, batch 배열, result+error 동시, id만
        assert!(validate_jsonrpc(b"plain text").is_err());
        assert!(validate_jsonrpc(br#"{"id":1,"result":{}}"#).is_err());
        assert!(validate_jsonrpc(br#"[{"jsonrpc":"2.0","id":1,"result":{}}]"#).is_err());
        assert!(validate_jsonrpc(br#"{"jsonrpc":"2.0","id":1,"result":{},"error":{}}"#).is_err());
        assert!(validate_jsonrpc(br#"{"jsonrpc":"2.0","id":1}"#).is_err());
        assert!(validate_jsonrpc(&[0xff, 0xfe]).is_err()); // UTF-8 아님
        assert!(validate_jsonrpc(b"").is_err()); // 빈 라인도 메시지가 아니다
    }

    #[test]
    fn jsonrpc_id_타입_검증() {
        // string id는 허용, object/배열 id는 거부
        assert!(validate_jsonrpc(br#"{"jsonrpc":"2.0","id":"abc","result":{}}"#).is_ok());
        assert!(validate_jsonrpc(br#"{"jsonrpc":"2.0","id":{},"result":{}}"#).is_err());
        assert!(validate_jsonrpc(br#"{"jsonrpc":"2.0","id":[1],"result":{}}"#).is_err());
        // null id는 error response에서만 허용
        assert!(
            validate_jsonrpc(
                br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse"}}"#
            )
            .is_ok()
        );
        assert!(validate_jsonrpc(br#"{"jsonrpc":"2.0","id":null,"result":{}}"#).is_err());
        assert!(validate_jsonrpc(br#"{"jsonrpc":"2.0","id":true,"method":"ping"}"#).is_err());
    }

    #[test]
    fn jsonrpc_error_형태_검증() {
        // error는 {code: number, message: string} object만 허용
        assert!(validate_jsonrpc(br#"{"jsonrpc":"2.0","id":1,"error":"oops"}"#).is_err());
        assert!(
            validate_jsonrpc(br#"{"jsonrpc":"2.0","id":1,"error":{"code":"x","message":"y"}}"#)
                .is_err()
        );
        assert!(validate_jsonrpc(br#"{"jsonrpc":"2.0","id":1,"error":{"code":-1}}"#).is_err());
    }

    #[test]
    fn 라인_읽기와_상한() {
        let mut reader = Cursor::new(&b"abc\ndef"[..]);
        assert!(matches!(
            read_line_capped(&mut reader, 100),
            LineRead::Line(line) if line == b"abc"
        ));
        // newline 없는 마지막 라인도 반환
        assert!(matches!(
            read_line_capped(&mut reader, 100),
            LineRead::Line(line) if line == b"def"
        ));
        assert!(matches!(read_line_capped(&mut reader, 100), LineRead::Eof));

        let mut reader = Cursor::new(&b"aaaaaaaaaa\n"[..]);
        assert!(matches!(
            read_line_capped(&mut reader, 4),
            LineRead::TooLong
        ));
    }

    #[derive(Clone)]
    struct CaptureSubscriber {
        fields: Arc<Mutex<Vec<String>>>,
    }

    impl Subscriber for CaptureSubscriber {
        fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, _span: &Attributes<'_>) -> Id {
            Id::from_u64(1)
        }

        fn record(&self, _span: &Id, _values: &Record<'_>) {}

        fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

        fn event(&self, event: &Event<'_>) {
            let mut visitor = CaptureVisitor {
                fields: Arc::clone(&self.fields),
            };
            event.record(&mut visitor);
        }

        fn enter(&self, _span: &Id) {}

        fn exit(&self, _span: &Id) {}
    }

    struct CaptureVisitor {
        fields: Arc<Mutex<Vec<String>>>,
    }

    impl Visit for CaptureVisitor {
        fn record_str(&mut self, field: &Field, value: &str) {
            self.fields
                .lock()
                .unwrap()
                .push(format!("{}={}", field.name(), value));
        }

        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.fields
                .lock()
                .unwrap()
                .push(format!("{}={value:?}", field.name()));
        }
    }

    #[test]
    fn unsolicited_debug_log는_params_전체를_기록하지_않는다() {
        let fields = Arc::new(Mutex::new(Vec::new()));
        let subscriber = CaptureSubscriber {
            fields: Arc::clone(&fields),
        };
        let value = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/message",
            "params": {
                "Authorization": "Bearer sk-should-not-appear"
            }
        });

        tracing::subscriber::with_default(subscriber, || {
            debug_unsupported_server_message(&value, true);
        });

        let captured = fields.lock().unwrap().join("\n");
        assert!(
            captured.contains("method=notifications/message"),
            "{captured}"
        );
        assert!(!captured.contains("sk-should-not-appear"), "{captured}");
        assert!(!captured.contains("Authorization"), "{captured}");
    }
}
