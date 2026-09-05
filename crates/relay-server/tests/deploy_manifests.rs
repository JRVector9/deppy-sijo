//! 배포 매니페스트 회귀 잠금.
//!
//! 실제 좌표(도메인·레지스트리·TLS·자격증명)가 아직 없다는 사실 자체를 테스트로 고정한다.
//! 누군가 그럴듯한 값을 채워 넣으면 BLOCKED 상태가 조용히 PASS로 바뀔 수 있는데, 그러면
//! "배포 준비됨"이라는 잘못된 신호가 남는다.

const README: &str = include_str!("../../../deploy/relay/README.md");
const STAGING_ENV: &str = include_str!("../../../deploy/relay/staging/relay.env.example");
const PRODUCTION_ENV: &str = include_str!("../../../deploy/relay/production/relay.env.example");
const STAGING_UNIT: &str = include_str!("../../../deploy/relay/staging/relay-server.service");
const PRODUCTION_UNIT: &str = include_str!("../../../deploy/relay/production/relay-server.service");
const SERVER_SOURCE: &str = include_str!("../src/main.rs");

/// 서버가 읽는 환경변수는 두 환경 예제 모두에 이름이 있어야 한다.
#[test]
fn every_environment_variable_the_server_reads_is_documented_in_both_environments() {
    let mut names: Vec<&str> = SERVER_SOURCE
        .match_indices("std::env::var(\"")
        .map(|(index, needle)| {
            let rest = &SERVER_SOURCE[index + needle.len()..];
            &rest[..rest.find('"').expect("환경변수 이름이 닫히지 않았다")]
        })
        .collect();
    names.sort_unstable();
    names.dedup();
    assert!(
        names.contains(&"DEPPY_RELAY_ROUTES") && names.contains(&"DEPPY_RELAY_BIND"),
        "{names:?}"
    );

    for name in names {
        for (environment, contents) in [("staging", STAGING_ENV), ("production", PRODUCTION_ENV)] {
            assert!(
                contents.contains(name),
                "{environment} 예제에 {name}이(가) 빠졌다"
            );
        }
    }
}

/// 모든 값이 자리표시자다. 실제 자격증명이나 호스트가 들어오면 실패한다.
#[test]
fn the_examples_carry_placeholders_and_never_real_coordinates() {
    for (environment, contents) in [("staging", STAGING_ENV), ("production", PRODUCTION_ENV)] {
        for required in ["DEPPY_RELAY_BIND=REQUIRED-", "DEPPY_RELAY_ROUTES=REQUIRED-"] {
            assert!(contents.contains(required), "{environment}: {required}");
        }
        for forbidden in ["https://", "wss://", ".com", ".net", ".io", ".app", ".dev"] {
            assert!(
                !contents.contains(forbidden),
                "{environment} 예제에 실제 좌표처럼 보이는 '{forbidden}'이 들어왔다"
            );
        }
        // 16진 자격증명이 실제로 채워진 흔적.
        assert!(
            !contents
                .lines()
                .any(|line| line.starts_with("DEPPY_RELAY_ROUTES=")
                    && line.chars().filter(|c| c.is_ascii_hexdigit()).count() > 32),
            "{environment} 예제에 실제 자격증명이 커밋된 것 같다"
        );
    }
}

/// 기본 자격증명은 존재하지 않는다 — 서버 소스에도, 매니페스트에도.
#[test]
fn no_default_admission_credential_exists_anywhere() {
    assert!(
        SERVER_SOURCE.contains("기본 자격증명은 존재하지 않는다"),
        "기본값 금지 근거가 소스에서 사라졌다"
    );
    assert!(
        !SERVER_SOURCE.contains("unwrap_or_else(|_| \"DEPPY_RELAY_ROUTES"),
        "라우트 구성에 기본값이 생겼다"
    );
    for contents in [STAGING_ENV, PRODUCTION_ENV] {
        assert!(!contents.to_lowercase().contains("password"));
    }
}

/// TLS 종단은 edge의 몫이라는 사실이 매니페스트와 서버 양쪽에 남아 있어야 한다.
#[test]
fn tls_termination_stays_an_edge_responsibility() {
    for contents in [README, STAGING_UNIT, PRODUCTION_UNIT] {
        assert!(contents.contains("TLS"), "TLS 종단 책임 기술이 사라졌다");
    }
    assert!(SERVER_SOURCE.contains("TLS는 배포 edge가 종단한다"));
    // 서버가 직접 TLS를 들면 신뢰 경계가 바뀐다.
    for forbidden in ["rustls", "native_tls", "TlsAcceptor"] {
        assert!(!SERVER_SOURCE.contains(forbidden), "{forbidden}");
    }
}

/// systemd 유닛은 최소 권한이어야 하고 자격증명을 명령줄로 넘기면 안 된다.
#[test]
fn the_units_run_least_privileged_and_never_pass_credentials_on_the_command_line() {
    for (environment, unit) in [("staging", STAGING_UNIT), ("production", PRODUCTION_UNIT)] {
        for required in [
            "EnvironmentFile=",
            "DynamicUser=yes",
            "NoNewPrivileges=yes",
            "ProtectSystem=strict",
            "ProtectHome=yes",
            "RestrictAddressFamilies=AF_INET AF_INET6",
            "MemoryDenyWriteExecute=yes",
            // 스레드·fd 상한과 정지 여유. 코드의 접속 상한 이전에 OS가 먼저 막는다.
            "TasksMax=640",
            "LimitNOFILE=4096",
            "TimeoutStopSec=15",
            "SystemCallFilter=@system-service",
            "CapabilityBoundingSet=",
            "RestrictSUIDSGID=yes",
        ] {
            assert!(unit.contains(required), "{environment}: {required}");
        }
        let exec = unit
            .lines()
            .find(|line| line.starts_with("ExecStart="))
            .expect("ExecStart");
        assert_eq!(
            exec, "ExecStart=/usr/local/bin/relay-server",
            "{environment}: 자격증명은 인자가 아니라 EnvironmentFile로만 들어간다"
        );
    }
}

/// BLOCKED 상태와 그 이유가 문서에 남아 있어야 한다. 좌표가 생기면 이 테스트를 함께
/// 고쳐야 하고, 그때가 상태를 바꾸는 유일한 시점이다.
#[test]
fn the_readme_records_every_unresolved_coordinate_as_blocked() {
    assert!(README.contains("BLOCKED — PASS 아님"));
    for coordinate in [
        "Mac 승인 자격증명 provisioning/회전 주체",
        "스테이징/프로덕션 도메인",
        "DNS 소유자",
        "TLS edge",
        "신뢰 Origin",
        "컨테이너/아티팩트 레지스트리",
        "GitHub 환경·시크릿 이름",
        "배포 자격증명",
    ] {
        assert!(README.contains(coordinate), "{coordinate}");
    }
    assert_eq!(
        README.matches("| BLOCKED |").count(),
        8,
        "해결되지 않은 좌표 수가 바뀌었으면 상태표도 함께 갱신해야 한다"
    );
    assert!(
        README.contains("단일 인스턴스"),
        "v1 단일 인스턴스 제약이 사라졌다"
    );
}
