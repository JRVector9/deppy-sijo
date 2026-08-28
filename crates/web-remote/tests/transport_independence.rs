//! 두 전송(loopback/Tailscale, Relay)이 서로의 수명에 손대지 않는다는 계약.
//!
//! Task 4 Step 1이 요구하는 성질이다: Relay만 켜는 배치가 loopback 리스너를 세우지 않아야 하고,
//! 한 전송을 끄거나 실패시켜도 다른 전송의 대시보드 코어가 살아 있어야 한다.
//! 공개 API만 쓴다 — 앱이 실제로 조립할 수 있는 모양인지도 함께 확인하는 셈이다.

use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::time::{Duration, Instant};

use web_remote::session_core::SessionCore;
use web_remote::{ServeOptions, WebRemoteServer};

fn options() -> ServeOptions {
    ServeOptions {
        token: "transport-independence-token".to_owned(),
        allowed_host: None,
        repository: None,
        vapid: None,
        uploads_dir: None,
    }
}

fn loopback() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, 0))
}

/// 브리지가 아직 살아 있는가 — 알림을 바꾸고 대시보드가 다시 조립되는지로 확인한다.
fn bridge_is_live(core: &SessionCore, marker: &str) -> bool {
    let guard = core.dashboard().register_connection();
    let before = core.dashboard().dash_build_count();
    core.dashboard().set_notice(Some(marker.to_owned()));
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if core.dashboard().dash_build_count() > before {
            drop(guard);
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(guard);
    false
}

/// Relay만 켜는 배치. 리스너를 하나도 세우지 않고 코어가 선다.
#[test]
fn a_relay_only_arrangement_needs_no_loopback_listener() {
    let core = SessionCore::spawn(None);
    assert!(bridge_is_live(&core, "relay-only"));
    core.shutdown();
}

/// Tailscale만 켜는 배치. 서버가 자기 코어를 만들고, 서버를 끄면 그 코어도 멈춘다.
#[test]
fn a_server_that_created_its_own_core_stops_it_on_shutdown() {
    let server = WebRemoteServer::serve(loopback(), options()).unwrap();
    let core = server.core();
    assert!(bridge_is_live(&core, "tailscale-only"));

    server.shutdown();
    assert!(
        !bridge_is_live(&core, "after-shutdown"),
        "서버가 소유한 코어는 서버와 함께 멈춘다"
    );
}

/// 둘 다 켜는 배치. 공유 코어는 서버를 꺼도 살아남는다 — Tailscale을 끄는 것만으로 Relay의
/// 대시보드가 죽으면 두 전송이 독립이라는 계약이 깨진다.
#[test]
fn stopping_the_loopback_server_leaves_a_shared_core_running() {
    let core = SessionCore::spawn(None);
    let server = WebRemoteServer::serve_with_core(loopback(), options(), core.clone()).unwrap();
    assert!(bridge_is_live(&core, "both-enabled"));

    server.shutdown();
    assert!(
        bridge_is_live(&core, "relay-survives"),
        "공유 코어는 서버 종료에 영향받지 않는다"
    );

    core.shutdown();
    assert!(!bridge_is_live(&core, "owner-stopped"));
}

/// 공유 코어를 쓰는 서버가 **시작에 실패해도** 코어는 멀쩡해야 한다. 한 전송의 실패가
/// 다른 전송을 끌고 내려가면 안 된다.
#[test]
fn a_failed_server_start_does_not_disturb_a_shared_core() {
    let core = SessionCore::spawn(None);

    // 비-loopback 평문 bind는 거부된다 — 이 크레이트의 기존 계약이다.
    let refused = WebRemoteServer::serve_with_core(
        SocketAddr::from(([192, 0, 2, 1], 0)),
        options(),
        core.clone(),
    );
    assert!(refused.is_err(), "비-loopback 평문 bind는 거부된다");

    // 이미 쓰이는 포트로도 실패시킨다.
    let occupied = TcpListener::bind(loopback()).unwrap();
    let taken = occupied.local_addr().unwrap();
    let refused = WebRemoteServer::serve_with_core(taken, options(), core.clone());
    assert!(refused.is_err(), "이미 점유된 포트 bind는 실패한다");
    drop(occupied);

    assert!(
        bridge_is_live(&core, "survives-failure"),
        "서버 시작 실패가 공유 코어를 멈추면 안 된다"
    );
    core.shutdown();
}

/// 서버를 여러 번 세웠다 내려도 공유 코어는 계속 같은 것이다 — 재시작이 Relay 쪽 상태를
/// 갈아엎지 않는다.
#[test]
fn restarting_the_loopback_server_reuses_the_same_shared_core() {
    let core = SessionCore::spawn(None);
    for round in 0..3 {
        let server = WebRemoteServer::serve_with_core(loopback(), options(), core.clone()).unwrap();
        assert!(bridge_is_live(&core, &format!("round-{round}")));
        assert!(
            std::ptr::eq(
                std::sync::Arc::as_ptr(&server.core()),
                std::sync::Arc::as_ptr(&core)
            ),
            "서버는 공유받은 코어를 그대로 쓴다"
        );
        server.shutdown();
    }
    assert!(bridge_is_live(&core, "after-restarts"));
    core.shutdown();
}
