//! Remote Transport Skeleton (설계문서 PR-19, §8.2).
//! localhost-only attach — InProcessRuntimeClient와 **같은 명령/이벤트 모델**을
//! newline-delimited JSON으로 loopback TCP에 실어 나른다.
//! public remote는 아직 아니다: bind는 127.0.0.1 고정, attach는 loopback만 허용.
//! (원격 인증/TLS/delta 스트림은 v1+ — §8.2 "Viewport는 terminal delta로 대체되는 자리")

use std::io::{BufRead, BufReader, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use anyhow::{Context, bail};
use deppy_core::SessionId;

use crate::client::{RuntimeCommandSink, RuntimeEventReceiver, RuntimeEventStream};
use crate::command::RuntimeCommand;
use crate::event::RuntimeEvent;
use crate::in_process::InProcessRuntimeClient;

/// 이벤트 pump 폴링 주기 — worker의 output batch와 별개인 전송 주기.
const PUMP_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);

/// in-process worker를 loopback TCP로 노출하는 서버.
/// 클라이언트는 순차 처리(스켈레톤) — 접속당 reader(명령 수신)와
/// pump(이벤트 송신) 스레드가 하나씩 붙는다.
pub struct RemoteRuntimeServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    accept_thread: Option<JoinHandle<()>>,
    backend: Arc<InProcessRuntimeClient>,
    /// 현재 처리 중인 접속 — shutdown이 reader를 깨울 수 있게 보관
    active_conn: Arc<Mutex<Option<TcpStream>>>,
}

impl RemoteRuntimeServer {
    /// 127.0.0.1에만 bind한다 (localhost-only는 함수 형태로 보장 — 주소를 받지 않는다).
    /// port 0이면 OS가 할당하고 [`Self::local_addr`]로 확인한다.
    pub fn serve(backend: InProcessRuntimeClient, port: u16) -> anyhow::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", port)).context("remote 서버 bind 실패")?;
        let addr = listener.local_addr()?;
        let backend = Arc::new(backend);
        let stop = Arc::new(AtomicBool::new(false));
        let active_conn: Arc<Mutex<Option<TcpStream>>> = Arc::default();

        let accept_backend = Arc::clone(&backend);
        let accept_stop = Arc::clone(&stop);
        let accept_conn = Arc::clone(&active_conn);
        let accept_thread = std::thread::Builder::new()
            .name("remote-accept".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    if accept_stop.load(Ordering::SeqCst) {
                        break;
                    }
                    match stream {
                        Ok(stream) => {
                            // 스켈레톤: 한 번에 한 클라이언트 — 접속이 끝날 때까지 처리.
                            // shutdown이 유휴 reader를 깨울 수 있게 소켓을 먼저 등록하고,
                            // 등록 후 stop을 재확인한다 — shutdown이 등록 직전에 지나갔으면
                            // 여기서 직접 닫는다 (등록/확인 순서로 race 창을 닫는다)
                            if let Ok(conn) = stream.try_clone() {
                                *accept_conn.lock().expect("active conn lock") = Some(conn);
                            }
                            if accept_stop.load(Ordering::SeqCst) {
                                let _ = stream.shutdown(Shutdown::Both);
                                accept_conn.lock().expect("active conn lock").take();
                                break;
                            }
                            serve_connection(stream, &accept_backend, &accept_stop);
                            accept_conn.lock().expect("active conn lock").take();
                        }
                        Err(e) => {
                            tracing::warn!("remote accept 실패: {e}");
                        }
                    }
                }
            })
            .context("remote accept thread 생성 실패")?;

        Ok(Self {
            addr,
            stop,
            accept_thread: Some(accept_thread),
            backend,
            active_conn,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// accept 루프를 멈추고 worker까지 동기 종료한다.
    pub fn shutdown(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // 유휴 접속의 reader를 깨운다 (소켓을 닫아 EOF) — 없으면 accept 대기 중
        if let Some(conn) = self.active_conn.lock().expect("active conn lock").take() {
            let _ = conn.shutdown(Shutdown::Both);
        }
        // blocking accept를 깨운다
        let _ = TcpStream::connect(self.addr);
        if let Some(handle) = self.accept_thread.take() {
            let _ = handle.join();
        }
        // 접속 스레드가 모두 끝났으면 유일 소유자 — worker 정리
        if let Ok(mut backend) = Arc::try_unwrap(self.backend) {
            backend.shutdown();
        }
    }
}

/// 한 클라이언트 접속을 처리한다: 이벤트 pump 스레드를 붙이고,
/// 이 스레드는 명령 라인을 읽어 worker로 넘긴다. 파싱 불가 라인은
/// 프로토콜 위반으로 접속을 끊는다 (mcp stdout 엄격성과 같은 태도).
fn serve_connection(
    stream: TcpStream,
    backend: &Arc<InProcessRuntimeClient>,
    stop: &Arc<AtomicBool>,
) {
    let receiver = backend.subscribe();
    let pump_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("remote stream clone 실패: {e}");
            return;
        }
    };
    let pump_stop = Arc::clone(stop);
    let pump = std::thread::Builder::new()
        .name("remote-pump".into())
        .spawn(move || {
            let mut stream = pump_stream;
            loop {
                if pump_stop.load(Ordering::SeqCst) {
                    break;
                }
                for event in receiver.drain() {
                    let json = match serde_json::to_string(&event) {
                        Ok(json) => json,
                        Err(e) => {
                            tracing::warn!("remote event 직렬화 실패: {e}");
                            continue;
                        }
                    };
                    if stream
                        .write_all(json.as_bytes())
                        .and_then(|()| stream.write_all(b"\n"))
                        .is_err()
                    {
                        return; // 클라이언트가 떠남
                    }
                }
                std::thread::sleep(PUMP_INTERVAL);
            }
        });
    let Ok(pump) = pump else {
        return;
    };

    let reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<RuntimeCommand>(&line) {
            Ok(command) => {
                if backend.send_command(command).is_err() {
                    break; // worker 종료됨
                }
            }
            Err(e) => {
                // 프로토콜 위반 — 이 접속을 신뢰하지 않는다
                tracing::warn!("remote 명령 파싱 실패, 접속 종료: {e}");
                break;
            }
        }
    }
    // reader 종료 → pump도 정리 (소켓을 닫아 write 에러로 끝낸다)
    let _ = stream.shutdown(Shutdown::Both);
    let _ = pump.join();
}

/// loopback의 RemoteRuntimeServer에 attach하는 클라이언트.
/// InProcessRuntimeClient와 같은 trait(RuntimeCommandSink/RuntimeEventStream)을
/// 구현한다 — UI 입장에서 교체 가능 (완료 기준).
pub struct RemoteRuntimeClient {
    writer: Mutex<TcpStream>,
    subscribers: Arc<Mutex<Vec<RemoteSubscriber>>>,
    reader_thread: Option<JoinHandle<()>>,
}

struct RemoteSubscriber {
    events: std::sync::mpsc::Sender<RuntimeEvent>,
    viewports: Arc<Mutex<std::collections::HashMap<SessionId, RuntimeEvent>>>,
}

impl RemoteRuntimeClient {
    /// loopback 주소에만 attach한다 (완료 기준: localhost-only attach).
    pub fn attach(addr: SocketAddr) -> anyhow::Result<Self> {
        if !addr.ip().is_loopback() {
            bail!("remote attach는 localhost만 허용합니다 (public remote는 v1+): {addr}");
        }
        let stream =
            TcpStream::connect(addr).with_context(|| format!("remote 서버 연결 실패: {addr}"))?;
        let subscribers: Arc<Mutex<Vec<RemoteSubscriber>>> = Arc::default();

        let reader_subscribers = Arc::clone(&subscribers);
        let reader_stream = stream.try_clone().context("remote stream clone 실패")?;
        let reader_thread = std::thread::Builder::new()
            .name("remote-events".into())
            .spawn(move || {
                let reader = BufReader::new(reader_stream);
                for line in reader.lines() {
                    let Ok(line) = line else { break };
                    if line.trim().is_empty() {
                        continue;
                    }
                    let event = match serde_json::from_str::<RuntimeEvent>(&line) {
                        Ok(event) => event,
                        Err(e) => {
                            tracing::warn!("remote 이벤트 파싱 실패, 수신 중단: {e}");
                            break;
                        }
                    };
                    dispatch(&reader_subscribers, event);
                }
            })
            .context("remote reader thread 생성 실패")?;

        Ok(Self {
            writer: Mutex::new(stream),
            subscribers,
            reader_thread: Some(reader_thread),
        })
    }
}

/// InProcess worker의 emit과 같은 분배·정리 규칙: Viewport는 세션별 최신본 slot,
/// 상태 이벤트는 채널. receiver가 drop된 구독자는 제거한다 — slot 경로는
/// Arc strong_count(receiver도 slot Arc를 쥔다), 채널 경로는 send 실패로 판별.
fn dispatch(subscribers: &Arc<Mutex<Vec<RemoteSubscriber>>>, event: RuntimeEvent) {
    subscribers
        .lock()
        .expect("remote subscribers lock")
        .retain(|subscriber| {
            if let RuntimeEvent::Viewport { session, .. } = &event {
                if Arc::strong_count(&subscriber.viewports) <= 1 {
                    return false;
                }
                subscriber
                    .viewports
                    .lock()
                    .expect("remote viewport slot lock")
                    .insert(*session, event.clone());
                true
            } else {
                subscriber.events.send(event.clone()).is_ok()
            }
        });
}

impl RuntimeCommandSink for RemoteRuntimeClient {
    fn send_command(&self, command: RuntimeCommand) -> anyhow::Result<()> {
        let json = serde_json::to_string(&command).context("remote 명령 직렬화 실패")?;
        let mut stream = self.writer.lock().expect("remote writer lock");
        stream
            .write_all(json.as_bytes())
            .and_then(|()| stream.write_all(b"\n"))
            .context("remote 명령 전송 실패")
    }
}

impl RuntimeEventStream for RemoteRuntimeClient {
    fn subscribe(&self) -> RuntimeEventReceiver {
        let (tx, rx) = channel();
        let viewports: Arc<Mutex<std::collections::HashMap<SessionId, RuntimeEvent>>> =
            Arc::default();
        self.subscribers
            .lock()
            .expect("remote subscribers lock")
            .push(RemoteSubscriber {
                events: tx,
                viewports: Arc::clone(&viewports),
            });
        RuntimeEventReceiver {
            events: rx,
            viewports,
        }
    }
}

impl crate::client::RuntimeClient for RemoteRuntimeClient {}

impl Drop for RemoteRuntimeClient {
    fn drop(&mut self) {
        if let Ok(stream) = self.writer.lock() {
            let _ = stream.shutdown(Shutdown::Both);
        }
        if let Some(handle) = self.reader_thread.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn init_mock_store() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            keyring_core::set_default_store(keyring_core::mock::Store::new().unwrap());
        });
    }

    fn test_backend(name: &str) -> InProcessRuntimeClient {
        init_mock_store();
        let logs = std::env::temp_dir().join(format!("deppy-remote-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&logs).unwrap();
        InProcessRuntimeClient::with_shell(
            5,
            Arc::new(secret::KeyringSecretStore),
            logs,
            secret::RedactionService::new(),
            pty::CommandSpec {
                program: "/bin/sh".into(),
                args: vec!["-c".into(), "echo remote-ok; sleep 5".into()],
                env: Vec::new(),
            },
        )
    }

    /// 조건이 참이 될 때까지 이벤트를 모으며 기다린다.
    fn wait_for(
        rx: &RuntimeEventReceiver,
        seen: &mut Vec<RuntimeEvent>,
        timeout: Duration,
        pred: impl Fn(&[RuntimeEvent]) -> bool,
    ) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            seen.extend(rx.drain());
            if pred(seen) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("이벤트 대기 시간 초과 — 수신: {}개", seen.len());
    }

    #[test]
    fn 서버는_loopback에만_bind() {
        let server = RemoteRuntimeServer::serve(test_backend("bind"), 0).unwrap();
        assert!(server.local_addr().ip().is_loopback());
        server.shutdown();
    }

    #[test]
    fn 비loopback_attach는_거부() {
        let Err(e) = RemoteRuntimeClient::attach("8.8.8.8:1".parse().unwrap()) else {
            panic!("비loopback attach가 성공하면 안 된다");
        };
        assert!(format!("{e:#}").contains("localhost"));
    }

    /// 완료 기준: InProcess와 같은 명령/이벤트 모델로 localhost attach.
    /// SpawnShell 명령이 TCP를 건너 worker에 닿고, ShellSpawned/MuxUpdated/
    /// Viewport 이벤트가 되돌아온다.
    #[test]
    fn attach_명령_이벤트_왕복() {
        let server = RemoteRuntimeServer::serve(test_backend("roundtrip"), 0).unwrap();
        let client = RemoteRuntimeClient::attach(server.local_addr()).unwrap();
        let rx = client.subscribe();

        client
            .send_command(RuntimeCommand::SpawnShell {
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
            })
            .unwrap();

        let mut seen = Vec::new();
        wait_for(&rx, &mut seen, Duration::from_secs(10), |events| {
            let spawned = events
                .iter()
                .any(|e| matches!(e, RuntimeEvent::ShellSpawned { .. }));
            let mux = events
                .iter()
                .any(|e| matches!(e, RuntimeEvent::MuxUpdated { .. }));
            let viewport = events.iter().any(|e| {
                matches!(e, RuntimeEvent::Viewport { snapshot, .. }
                    if snapshot.visible_cells.iter().map(|c| c.c).collect::<String>().contains("remote-ok"))
            });
            spawned && mux && viewport
        });

        // 이벤트 순서 계약: MuxUpdated가 ShellSpawned보다 먼저 (emit 순서 유지 확인)
        let mux_pos = seen
            .iter()
            .position(|e| matches!(e, RuntimeEvent::MuxUpdated { .. }))
            .unwrap();
        let spawned_pos = seen
            .iter()
            .position(|e| matches!(e, RuntimeEvent::ShellSpawned { .. }))
            .unwrap();
        assert!(mux_pos < spawned_pos);

        drop(client);
        server.shutdown();
    }

    /// 유휴 클라이언트가 붙어 있어도 shutdown은 블록되지 않는다 (codex 리뷰 회귀).
    #[test]
    fn 접속_유지_중_shutdown() {
        let server = RemoteRuntimeServer::serve(test_backend("idle-shutdown"), 0).unwrap();
        let client = RemoteRuntimeClient::attach(server.local_addr()).unwrap();
        // 접속이 accept돼 reader가 붙을 때까지 잠깐 대기
        std::thread::sleep(Duration::from_millis(200));
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&done);
        let handle = std::thread::spawn(move || {
            server.shutdown();
            flag.store(true, Ordering::SeqCst);
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "shutdown이 유휴 접속에 블록됨");
            std::thread::sleep(Duration::from_millis(20));
        }
        handle.join().unwrap();
        drop(client);
    }

    /// 프로토콜 위반(비-JSON 라인)은 접속 종료로 이어진다.
    #[test]
    fn 잘못된_명령_라인은_접속_종료() {
        let server = RemoteRuntimeServer::serve(test_backend("protocol"), 0).unwrap();
        let mut raw = TcpStream::connect(server.local_addr()).unwrap();
        raw.write_all(b"not-json\n").unwrap();
        // 서버가 접속을 닫으면 read가 EOF(0)로 끝난다
        raw.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut buf = [0u8; 256];
        let mut deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match std::io::Read::read(&mut raw, &mut buf) {
                Ok(0) => break, // EOF — 종료 확인
                Ok(_) => {}     // 종료 전에 pump가 보낸 이벤트일 수 있음 — 계속
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => break, // reset도 종료로 취급
            }
            assert!(Instant::now() < deadline, "접속 종료 대기 시간 초과");
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = &mut deadline;
        server.shutdown();
    }
}
