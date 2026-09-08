//! 신뢰 셸 배포 매니페스트 회귀 잠금.
//!
//! `deploy/relay/`의 데이터 평면 잠금과 같은 이유로 존재한다: 실제 좌표(도메인·CDN·TLS·
//! 자격증명)가 아직 없다는 사실 **자체**를 테스트로 고정한다. 누군가 그럴듯한 값을 채워
//! 넣으면 BLOCKED가 조용히 PASS로 바뀌고, 그러면 "배포 준비됨"이라는 잘못된 신호가 남는다.

const README: &str = include_str!("../../../deploy/relay-shell/README.md");
const STAGING_ENV: &str = include_str!("../../../deploy/relay-shell/staging/shell.env.example");
const PRODUCTION_ENV: &str =
    include_str!("../../../deploy/relay-shell/production/shell.env.example");
const STAGING_HEADERS: &str = include_str!("../../../deploy/relay-shell/staging/headers.conf");
const PRODUCTION_HEADERS: &str =
    include_str!("../../../deploy/relay-shell/production/headers.conf");
const BUILD_SCRIPT: &str = include_str!("../../../web/relay-shell/build.sh");
const WORKFLOW: &str = include_str!("../../../.github/workflows/relay-shell-release.yml");
const CRYPTO_MODULE: &str = include_str!("../../../web/relay-shell/relay-crypto.js");
const SHELL_MODULE: &str = include_str!("../../../web/relay-shell/relay-shell.js");

/// `export const NAME = <정수>;` 한 줄을 읽는다.
fn js_const(source: &str, name: &str) -> u16 {
    let needle = format!("export const {name} = ");
    let rest = source
        .split_once(&needle)
        .unwrap_or_else(|| panic!("{name}을 찾지 못했다"))
        .1;
    rest[..rest.find(';').expect("선언이 닫히지 않았다")]
        .trim()
        .parse()
        .unwrap_or_else(|error| panic!("{name}이 정수가 아니다: {error}"))
}

fn environments() -> [(&'static str, &'static str, &'static str); 2] {
    [
        ("staging", STAGING_ENV, STAGING_HEADERS),
        ("production", PRODUCTION_ENV, PRODUCTION_HEADERS),
    ]
}

/// `shell.env.example`에서 `NAME=value`의 값을 읽는다. 주석 줄은 건너뛴다.
fn env_value<'a>(contents: &'a str, name: &str) -> Option<&'a str> {
    contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .find_map(|line| line.strip_prefix(name)?.strip_prefix('='))
}

/// `headers.conf`에서 헤더 값을 읽는다.
fn header_value<'a>(contents: &'a str, name: &str) -> Option<&'a str> {
    contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .find_map(|line| Some(line.strip_prefix(name)?.strip_prefix(':')?.trim()))
}

/// RFC 2606/6761 예약 이름만 자리표시자다. `build.sh`의 `is_placeholder_host`와 같은 판정.
fn is_placeholder_host(host: &str) -> bool {
    [".invalid", ".example", ".test", ".localhost"]
        .iter()
        .any(|suffix| host.ends_with(suffix))
        || host == "example.com"
        || host.ends_with(".example.com")
}

fn host_of(origin: &str) -> &str {
    let rest = origin.split_once("://").map_or(origin, |(_, rest)| rest);
    rest.split(['/', ':']).next().unwrap_or(rest)
}

/// `build.sh`가 읽는 환경변수는 두 환경 예제 모두에 이름이 있어야 한다.
#[test]
fn every_environment_variable_the_build_reads_is_documented_in_both_environments() {
    let mut names: Vec<&str> = BUILD_SCRIPT
        .match_indices("${")
        .map(|(index, needle)| {
            let rest = &BUILD_SCRIPT[index + needle.len()..];
            let end = rest
                .find(|character: char| !character.is_ascii_uppercase() && character != '_')
                .unwrap_or(rest.len());
            &rest[..end]
        })
        .filter(|name| !name.is_empty())
        .collect();
    names.sort_unstable();
    names.dedup();
    assert!(
        names.contains(&"SHELL_ORIGIN") && names.contains(&"RELAY_ORIGIN"),
        "{names:?}"
    );

    for name in names {
        for (environment, contents, _) in environments() {
            assert!(
                contents.contains(name),
                "{environment} 예제에 {name}이(가) 빠졌다"
            );
        }
    }
}

/// 두 오리진 모두 예약 이름이어야 한다. 실제 도메인이 들어오는 순간 이 테스트가 깨지고,
/// 그때가 README 상태표를 함께 고쳐야 하는 시점이다.
#[test]
fn the_examples_carry_placeholders_and_never_real_coordinates() {
    for (environment, contents, _) in environments() {
        for name in ["SHELL_ORIGIN", "RELAY_ORIGIN"] {
            let origin = env_value(contents, name)
                .unwrap_or_else(|| panic!("{environment}에 {name}이 없다"));
            assert!(
                is_placeholder_host(host_of(origin)),
                "{environment}의 {name}이 실제 좌표처럼 보인다: {origin} — \
                 배포가 열렸다면 deploy/relay-shell/README.md의 상태표를 먼저 고쳐라"
            );
            assert!(
                !origin.ends_with('/'),
                "{environment}의 {name}에 끝 슬래시가 있다: {origin}"
            );
        }
        assert!(
            env_value(contents, "SHELL_ORIGIN")
                .is_some_and(|origin| origin.starts_with("https://")),
            "{environment}의 셸 오리진은 https만 허용한다"
        );
        assert!(
            env_value(contents, "RELAY_ORIGIN").is_some_and(|origin| origin.starts_with("wss://")),
            "{environment}의 Relay 오리진은 wss만 허용한다"
        );
    }
}

/// 신뢰하는 셸과 신뢰하지 않는 데이터 평면은 **다른 오리진**이다. 같아지면 Relay가
/// 실행 가능한 코드를 서비스하게 되고 그 순간 신뢰 경계가 사라진다.
#[test]
fn the_trusted_shell_and_the_untrusted_relay_never_share_a_host() {
    for (environment, contents, _) in environments() {
        let shell = host_of(env_value(contents, "SHELL_ORIGIN").expect("셸 오리진"));
        let relay = host_of(env_value(contents, "RELAY_ORIGIN").expect("Relay 오리진"));
        assert_ne!(
            shell, relay,
            "{environment}에서 셸과 Relay가 같은 호스트를 쓴다"
        );
    }
}

/// 두 환경은 독립적으로 롤백 가능해야 한다 — 오리진도 산출물 경로도 공유하지 않는다.
#[test]
fn the_two_environments_stay_independently_rollbackable() {
    let staging_shell = env_value(STAGING_ENV, "SHELL_ORIGIN").expect("staging 셸 오리진");
    let production_shell = env_value(PRODUCTION_ENV, "SHELL_ORIGIN").expect("production 셸 오리진");
    assert_ne!(staging_shell, production_shell);

    let staging_relay = env_value(STAGING_ENV, "RELAY_ORIGIN").expect("staging Relay 오리진");
    let production_relay =
        env_value(PRODUCTION_ENV, "RELAY_ORIGIN").expect("production Relay 오리진");
    assert_ne!(staging_relay, production_relay);

    let staging_dist = env_value(STAGING_ENV, "RELAY_SHELL_DIST").expect("staging 산출물 경로");
    let production_dist =
        env_value(PRODUCTION_ENV, "RELAY_SHELL_DIST").expect("production 산출물 경로");
    assert_ne!(
        staging_dist, production_dist,
        "두 환경이 같은 산출물 경로를 쓰면 한쪽 빌드가 다른 쪽을 덮어쓴다"
    );
}

/// 응답 헤더 CSP는 그 환경의 Relay 오리진 **하나만** connect-src로 지명한다.
#[test]
fn the_response_header_csp_names_only_its_own_relay_origin() {
    for (environment, contents, headers) in environments() {
        let relay = env_value(contents, "RELAY_ORIGIN").expect("Relay 오리진");
        let csp = header_value(headers, "Content-Security-Policy")
            .unwrap_or_else(|| panic!("{environment} headers.conf에 CSP가 없다"));

        let connect = csp
            .split(';')
            .map(str::trim)
            .find_map(|directive| directive.strip_prefix("connect-src "))
            .unwrap_or_else(|| panic!("{environment} CSP에 connect-src가 없다"));
        assert_eq!(
            connect.split_whitespace().collect::<Vec<_>>(),
            vec![relay],
            "{environment}의 connect-src는 자기 Relay 오리진 하나여야 한다"
        );

        for required in [
            "default-src 'none'",
            "object-src 'none'",
            "base-uri 'none'",
            "frame-ancestors 'none'",
            "form-action 'none'",
            "require-trusted-types-for 'script'",
        ] {
            assert!(
                csp.contains(required),
                "{environment} CSP에 {required}이(가) 없다"
            );
        }
        let scripting = csp
            .split(';')
            .map(str::trim)
            .filter(|directive| {
                directive.starts_with("script-src") || directive.starts_with("worker-src")
            })
            .collect::<Vec<_>>();
        for forbidden in ["unsafe-inline", "unsafe-eval", "*", "data:", "http:"] {
            assert!(
                !scripting
                    .iter()
                    .any(|directive| directive.contains(forbidden)),
                "{environment}의 실행 지시자가 {forbidden}을 허용한다: {scripting:?}"
            );
        }
    }
}

/// 응답 헤더 CSP는 `build.sh`가 매니페스트에 적는 CSP와 **한 글자도** 달라선 안 된다.
/// 둘이 갈라지면 아티팩트가 약속한 정책과 호스트가 실제로 붙이는 정책이 달라진다.
#[test]
fn the_header_csp_matches_the_policy_the_build_records_in_its_manifest() {
    let template = BUILD_SCRIPT
        .split_once("    csp=\"")
        .expect("build.sh에 csp= 할당이 없다")
        .1;
    let template = &template[..template.find('"').expect("csp 문자열이 닫히지 않았다")];
    assert!(
        template.contains("$RELAY_ORIGIN"),
        "build.sh의 CSP가 Relay 오리진을 변수로 두지 않았다"
    );

    for (environment, contents, headers) in environments() {
        let relay = env_value(contents, "RELAY_ORIGIN").expect("Relay 오리진");
        let expected = template.replace("$RELAY_ORIGIN", relay);
        assert_eq!(
            header_value(headers, "Content-Security-Policy"),
            Some(expected.as_str()),
            "{environment} headers.conf의 CSP가 build.sh와 어긋났다"
        );
    }
}

/// 진입 문서와 서비스워커가 캐시되면 옛 셸이 새 프로토콜을 조용히 약화시킨 채 살아남는다.
#[test]
fn the_headers_forbid_caching_the_entry_document_and_service_worker() {
    for (environment, _, headers) in environments() {
        for path in ["/index.html", "/sw.js"] {
            assert!(
                headers
                    .lines()
                    .any(|line| line.contains(path) && line.contains("no-store")),
                "{environment} headers.conf가 {path}의 no-store를 적지 않았다"
            );
        }
        for required in [
            "X-Content-Type-Options",
            "Referrer-Policy",
            "Strict-Transport-Security",
            "X-Frame-Options",
        ] {
            assert!(
                header_value(headers, required).is_some(),
                "{environment} headers.conf에 {required}이(가) 없다"
            );
        }
    }
}

/// 셸이 받아 주는 프로토콜 버전 창을 Rust 와이어 상수에 묶는다.
///
/// 셸은 `MIN_PROTOCOL_VERSION..=PROTOCOL_VERSION`만 받고 협상하지 않는다. 그 창이 Rust
/// `relay_protocol::PROTOCOL_VERSION`과 갈라지면, 사이드카 다이제스트가 다 맞는 **유효한**
/// 옛 아티팩트가 새 프로토콜을 조용히 약화시킨 채 살아남는다. 서명 없는 다이제스트
/// 매니페스트만으로는 그 다운그레이드를 막지 못하므로, 창 자체를 여기서 고정한다.
#[test]
fn the_shell_version_window_stays_tied_to_the_rust_wire_constant() {
    let shipped = js_const(CRYPTO_MODULE, "PROTOCOL_VERSION");
    assert_eq!(
        shipped,
        relay_protocol::PROTOCOL_VERSION,
        "셸 크립토 모듈의 프로토콜 버전이 Rust 와이어 상수와 다르다"
    );

    let minimum = js_const(SHELL_MODULE, "MIN_PROTOCOL_VERSION");
    assert!(
        minimum <= shipped,
        "최소 허용 버전 {minimum}이 셸이 말하는 버전 {shipped}보다 높다 — 셸이 자기 자신을 거절한다"
    );

    // 빌드가 이 두 값을 매니페스트에 적는다. 읽는 이름이 바뀌면 매니페스트가 조용히 비고,
    // 배포 검증이 버전 없는 아티팩트를 통과시키게 된다.
    assert!(
        BUILD_SCRIPT.contains("PROTOCOL_VERSION") && BUILD_SCRIPT.contains("MIN_PROTOCOL_VERSION"),
        "build.sh가 프로토콜 버전 창을 매니페스트에 기록해야 한다"
    );
}

/// README 상태표의 BLOCKED 항목 수가 줄면 함께 고쳐야 한다. 표만 조용히 바뀌는 것을 막는다.
#[test]
fn the_readme_records_every_unresolved_coordinate_as_blocked() {
    let blocked = README.matches("| BLOCKED |").count();
    assert_eq!(
        blocked, 9,
        "BLOCKED 좌표 수가 바뀌었다. 실제로 좌표가 정해졌다면 이 테스트와 배포 워크플로를 \
         함께 고쳐라 — 표만 바꾸는 것은 차단을 지우는 게 아니라 숨기는 것이다"
    );
    assert!(
        README.contains("BLOCKED — PASS 아님"),
        "README가 BLOCKED를 PASS와 구분해 적어야 한다"
    );
}

/// 워크플로는 아티팩트까지만 만들고 publish에서 **일부러 실패**한다. 그리고 도메인을
/// 스스로 알고 있으면 안 된다 — 오리진은 환경 예제에서만 온다.
#[test]
fn the_release_workflow_refuses_to_publish_and_invents_no_coordinates() {
    assert!(
        WORKFLOW.contains("Publish (BLOCKED)") && WORKFLOW.contains("exit 1"),
        "publish 단계는 명시적으로 실패해야 한다"
    );
    assert!(
        WORKFLOW.contains("build.sh verify"),
        "빌드한 아티팩트를 자기 매니페스트로 되대조해야 한다"
    );
    for invented in ["https://", "wss://"] {
        assert!(
            !WORKFLOW.contains(invented),
            "워크플로가 오리진을 직접 적었다: {invented} — 환경 예제에서만 읽어야 한다"
        );
    }
}
