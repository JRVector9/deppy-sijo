//! 전송 계층 계약 잠금.
//!
//! `main.rs`는 실제 소켓 없이는 단위 테스트할 수 없다. 그래서 소켓 없이도 검증 가능한
//! 성질 — 어떤 상한을 언제 적용하는가, 종료 전에 무엇을 반드시 써 내는가 — 을 소스 법칙으로
//! 고정한다. 이 성질들은 직접 리뷰가 짚은 실제 결함에서 나왔고, 회귀하면 조용히 되돌아간다.

const TRANSPORT: &str = include_str!("../src/main.rs");

/// 입장 전 연결은 헤더+자격증명 크기만큼만 읽을 수 있다.
///
/// DRLY 선점검은 WebSocket 메시지가 다 모인 뒤에야 돌 수 있으므로, 입장 전 상한을 조여
/// 두지 않으면 아무 자격도 증명하지 않은 상대가 최대 프레임 크기의 읽기 버퍼를 잡게 만들 수
/// 있다. 연결 상한(`max_connections`)과 곱해지면 그대로 메모리 압박이 된다.
#[test]
fn an_unadmitted_peer_can_only_make_us_buffer_one_admission_frame() {
    assert!(
        TRANSPORT.contains(
            "const PRE_ADMISSION_MESSAGE_BYTES: usize = HEADER_BYTES + ADMISSION_CREDENTIAL_BYTES;"
        ),
        "입장 전 상한 상수가 사라졌다"
    );

    let accept = section(TRANSPORT, "fn serve(", "fn pump(");
    assert!(
        accept.contains("config.max_message_size = Some(PRE_ADMISSION_MESSAGE_BYTES)")
            && accept.contains("config.max_frame_size = Some(PRE_ADMISSION_MESSAGE_BYTES)"),
        "accept 시점 상한이 입장 전 크기가 아니다"
    );
    assert!(
        !accept.contains("Some(MAX_FRAME_BYTES)"),
        "입장 전 연결에 정규 상한을 주면 안 된다"
    );
}

/// 정규 상한으로 올리는 일은 코어가 입장을 인정한 뒤에만 일어난다.
#[test]
fn the_full_frame_budget_is_granted_only_after_the_core_admits_the_connection() {
    let pump = section(TRANSPORT, "fn pump(", "fn closes_me(");
    let admitted = pump
        .find("connection_is_admitted(key)")
        .expect("입장 확인이 사라졌다");
    let raised = pump
        .find("config.max_message_size = Some(MAX_FRAME_BYTES)")
        .expect("정규 상한 상향이 사라졌다");
    assert!(
        admitted < raised,
        "코어가 입장을 인정하기 전에 상한을 올리면 안 된다"
    );
}

/// 거절 코드는 절단 전에 실제로 소켓에 나가야 한다. 채널에만 넣고 빠져나오면 상대는
/// 이유 없는 종료만 본다.
#[test]
fn a_rejection_is_written_to_the_socket_before_the_connection_closes() {
    let pump = section(TRANSPORT, "fn pump(", "/// 이 연결로 향하는 프레임만");
    let closings = pump.matches("if closing {").count();
    assert!(closings >= 2, "종료 분기 수가 바뀌었다: {closings}");
    assert_eq!(
        pump.matches("flush_self(socket, &actions, key);").count(),
        closings,
        "모든 종료 분기가 자기 앞으로 온 프레임을 먼저 써 내야 한다"
    );

    let flush = section(TRANSPORT, "fn flush_self(", "fn ip_bytes(");
    assert!(
        flush.contains("*connection == key"),
        "다른 연결의 프레임까지 이 소켓으로 쓰면 안 된다"
    );
    assert!(flush.contains("socket.flush()"), "flush가 빠졌다");
}

/// 코어 락을 쥔 채 소켓에 쓰지 않는다. 느린 소비자 하나가 서버 전체의 판정을 막으면 안 된다.
#[test]
fn the_core_lock_is_never_held_across_a_socket_write() {
    let dispatch = section(TRANSPORT, "fn dispatch(", "fn ip_bytes(");
    for forbidden in ["socket.send", "socket.write", "socket.flush"] {
        assert!(
            !dispatch.contains(forbidden),
            "dispatch는 소켓에 직접 쓰지 않는다: {forbidden}"
        );
    }
}

/// 읽기와 쓰기 **양쪽**에 시한이 있어야 한다. 쓰기 시한이 없으면 수신 윈도가 막힌 상대에게
/// `send`가 무한정 걸리고, 그 스레드는 종료 지시도 못 보고 `connection_closed`도 못 불러
/// 코어의 정리 대기 예산이 영원히 안 풀린다.
#[test]
fn both_directions_carry_a_deadline_so_a_stalled_peer_cannot_pin_a_worker() {
    assert!(
        TRANSPORT.contains("const WRITE_DEADLINE: Duration"),
        "쓰기 시한 상수가 사라졌다"
    );
    let serve = section(TRANSPORT, "fn serve(", "fn pump(");
    let read = serve
        .find("set_read_timeout(Some(POLL_INTERVAL))")
        .expect("읽기 시한이 사라졌다");
    let write = serve
        .find("set_write_timeout(Some(WRITE_DEADLINE))")
        .expect("쓰기 시한이 사라졌다");
    let accept = serve
        .find("accept_with_config")
        .expect("핸드셰이크가 사라졌다");
    assert!(
        read < accept && write < accept,
        "핸드셰이크도 읽고 쓴다 — 시한은 accept 이전에 걸려야 한다"
    );
}

/// 끊기로 판정된 상대에게는 밀린 백로그를 쓰지 않고 버린다. 코어가 그만큼을 "정리 대기"
/// 예산으로 붙잡고 있으므로, 실제로 버려야 예산이 풀린다.
#[test]
fn a_departing_peers_backlog_is_discarded_rather_than_written() {
    let pump = section(TRANSPORT, "fn pump(", "fn closes_me(");
    let discarded = pump.find("drop(pending);").expect("백로그 폐기가 사라졌다");
    let written = pump
        .find("socket.send(Message::Binary(frame.into()))")
        .expect("정상 발신 경로가 사라졌다");
    assert!(
        discarded < written,
        "종료 판정을 확인하기 전에 백로그를 써 버리면 안 된다"
    );
}

/// 코어에 "닫혔다"고 알리기 **전에** 채널에 남은 바이트를 버려야 한다. 순서가 뒤집히면
/// 전체 예산이 실제보다 먼저 풀린다.
#[test]
fn the_outbound_backlog_is_freed_before_the_core_is_told_the_connection_closed() {
    let serve = section(TRANSPORT, "fn serve(", "fn pump(");
    let dropped = serve
        .find("drop(receiver);")
        .expect("수신단 폐기가 사라졌다");
    let confirmed = serve
        .find(".connection_closed(key, unix_now())")
        .expect("종료 통지가 사라졌다");
    assert!(
        dropped < confirmed,
        "남은 바이트를 버리기 전에 예산을 풀면 안 된다"
    );
}

/// 절단 지시는 채널에만 넣으면 안 된다. 1바이트씩 흘려 넣어 `read()`를 붙잡은 상대는 채널을
/// 영영 확인하지 않는다 — 소켓을 직접 닫아야 `read()`가 오류로 돌아온다.
#[test]
fn a_disconnect_closes_the_socket_so_a_pinned_read_returns() {
    assert!(
        TRANSPORT.contains("HashMap<ConnectionKey, (Sender<Outbound>, TcpStream)>"),
        "registry가 소켓 복제본을 들고 있어야 한다"
    );
    let serve = section(TRANSPORT, "fn serve(", "fn pump(");
    assert!(
        serve.contains("stream.try_clone()"),
        "절단용 소켓 복제본이 사라졌다"
    );
    let dispatch = section(TRANSPORT, "fn dispatch(", "fn ip_bytes(");
    let close = dispatch
        .find("RelayAction::Disconnect { connection, code }")
        .expect("Disconnect 분기");
    let shutdown = dispatch[close..]
        .find("socket.shutdown(Shutdown::Both)")
        .expect("절단 시 소켓을 닫아야 한다");
    assert!(shutdown < 600);
}

/// 스레드는 핸드셰이크 **전에** 상한을 받는다. 코어의 연결 상한은 핸드셰이크 뒤에야
/// 적용되므로, 그 전 단계에서 스레드가 무한정 생기는 것은 accept 루프가 막아야 한다.
#[test]
fn worker_threads_are_capped_before_they_are_spawned() {
    let main = section(TRANSPORT, "fn main()", "fn route_verifiers_from_env(");
    let cap = main
        .find("WorkerSlot::try_acquire(max_workers)")
        .expect("스레드 상한 확보");
    let spawn = main
        .find("std::thread::spawn(move ||")
        .expect("스레드 생성");
    assert!(cap < spawn, "상한을 넘으면 스레드를 아예 만들지 않는다");
    assert!(
        TRANSPORT.contains("impl Drop for WorkerSlot"),
        "panic으로 죽어도 자리를 돌려줘야 한다"
    );
}

/// SIGTERM은 정지 깃발이 되고, accept 루프는 그 깃발을 볼 수 있어야 한다. 차단 accept는
/// 깃발을 볼 기회가 없어 `systemctl stop`이 프레임 중간에 프로세스를 죽인다.
#[test]
fn shutdown_signals_are_observed_by_a_non_blocking_accept_loop() {
    // rustfmt가 줄을 나누므로 공백을 지운 뒤 본다.
    let compact: String = TRANSPORT.chars().filter(|c| !c.is_whitespace()).collect();
    for signal in ["SIGTERM", "SIGINT"] {
        assert!(
            compact.contains(&format!(
                "libc::signal(libc::{signal},request_shutdownas*const()aslibc::sighandler_t,)"
            )),
            "{signal} 핸들러가 설치돼야 한다"
        );
    }
    let main = section(TRANSPORT, "fn main()", "fn route_verifiers_from_env(");
    assert!(main.contains(".set_nonblocking(true)"), "비차단 accept");
    assert!(
        main.contains("!SHUTDOWN_REQUESTED.load(Ordering::SeqCst)"),
        "accept 루프가 정지 깃발을 확인해야 한다"
    );
    // 루프를 빠져나온 뒤에는 코어 shutdown → dispatch → join 순서다.
    let after = main.find(".shutdown(unix_now())").expect("코어 종료");
    let join = main[after..].find("worker.join()").expect("스레드 join");
    assert!(join > 0);
}

fn section<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
    let from = source.find(start).unwrap_or_else(|| panic!("{start}"));
    let to = source[from..].find(end).unwrap_or_else(|| panic!("{end}"));
    &source[from..from + to]
}
