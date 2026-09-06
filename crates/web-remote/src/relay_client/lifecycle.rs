//! Relay 클라이언트 수명주기 — I/O 없음.
//!
//! 소켓도 시계도 DNS도 없다. 엔드포인트 정책, 재접속 백오프, 그리고 "언제 다시 붙어도 되는가"를
//! 결정하는 상태 기계만 있다. 그래서 자격증명 회전·취소·인증 실패 뒤의 동작을 실제 네트워크
//! 없이 결정적으로 검증할 수 있다.
//!
//! 가장 중요한 규칙: **인증 실패나 취소 뒤에는 사용자가 명시적으로 다시 시도하기 전까지 절대
//! 재접속하지 않는다.** 자동 재시도는 폐기된 자격증명으로 무한히 문을 두드리는 것과 같다.

use std::time::Duration;

/// 프로덕션 Relay 엔드포인트. 모바일 셸의 CSP와 공유하는 **릴리스 상수**다.
///
/// 아직 `None`이다 — 도메인·DNS·TLS edge 소유자가 정해지지 않았다(`deploy/relay/README.md`의
/// 상태표). 그럴듯한 호스트명을 지어 넣으면 그 순간 BLOCKED가 조용히 PASS로 바뀌므로,
/// 좌표가 실제로 생길 때까지 비워 둔다. 그때 이 상수 하나만 채우면 된다.
pub const PRODUCTION_RELAY_ENDPOINT: Option<&str> = None;

/// 엔드포인트 최대 길이. 파싱 전에 먼저 자른다.
pub const MAX_ENDPOINT_BYTES: usize = 255;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EndpointError {
    NotAssigned,
    TooLong { bytes: usize },
    NotWss,
    MissingHost,
    HostIsAddressLiteral,
    HostIsLoopback,
    CredentialsInUrl,
    QueryOrFragment,
    InvalidPort,
    InvalidHost,
}

impl std::fmt::Display for EndpointError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAssigned => formatter.write_str(
                "Relay 프로덕션 엔드포인트가 아직 배정되지 않았다 (deploy/relay/README.md: BLOCKED)",
            ),
            Self::TooLong { bytes } => write!(formatter, "Relay 엔드포인트가 너무 길다({bytes})"),
            Self::NotWss => formatter.write_str("Relay 엔드포인트는 wss://만 허용한다"),
            Self::MissingHost => formatter.write_str("Relay 엔드포인트에 호스트가 없다"),
            Self::HostIsAddressLiteral => {
                formatter.write_str("Relay 엔드포인트는 IP 리터럴을 허용하지 않는다")
            }
            Self::HostIsLoopback => {
                formatter.write_str("Relay 엔드포인트는 loopback을 허용하지 않는다")
            }
            Self::CredentialsInUrl => {
                formatter.write_str("Relay 엔드포인트 URL에 자격증명을 넣을 수 없다")
            }
            Self::QueryOrFragment => {
                formatter.write_str("Relay 엔드포인트에 질의/프래그먼트를 넣을 수 없다")
            }
            Self::InvalidPort => formatter.write_str("Relay 엔드포인트 포트가 올바르지 않다"),
            Self::InvalidHost => formatter.write_str("Relay 엔드포인트 호스트가 올바르지 않다"),
        }
    }
}

impl std::error::Error for EndpointError {}

/// 검증을 통과한 Relay 엔드포인트. 워커를 띄우기 **전에** 이 값이 만들어져야 한다.
#[derive(Clone, PartialEq, Eq)]
pub struct RelayEndpoint {
    url: String,
    host: String,
    port: u16,
}

impl RelayEndpoint {
    /// 릴리스 상수에서 만든다. 좌표가 아직 없으면 실패한다 — 기본 엔드포인트는 없다.
    pub fn production() -> Result<Self, EndpointError> {
        Self::parse(PRODUCTION_RELAY_ENDPOINT.ok_or(EndpointError::NotAssigned)?)
    }

    /// 정책 검사. 순서가 곧 보안이다 — 길이를 먼저 자르고, 그다음 스킴, 그다음 구조를 본다.
    ///
    /// `wss://`만 허용한다. 평문 `ws://`는 물론이고 IP 리터럴·loopback·URL 내 자격증명·
    /// 질의·프래그먼트를 모두 거부한다. IP 리터럴을 막는 이유는 인증서 호스트명 검증이
    /// 의미를 갖는 대상이 DNS 이름이기 때문이고, 질의/프래그먼트를 막는 이유는 그 자리가
    /// 티켓 같은 비밀이 새기 가장 쉬운 곳이기 때문이다.
    pub fn parse(raw: &str) -> Result<Self, EndpointError> {
        let raw = raw.trim();
        if raw.len() > MAX_ENDPOINT_BYTES {
            return Err(EndpointError::TooLong { bytes: raw.len() });
        }
        let rest = raw.strip_prefix("wss://").ok_or(EndpointError::NotWss)?;
        if rest.contains('?') || rest.contains('#') {
            return Err(EndpointError::QueryOrFragment);
        }
        if rest.contains('@') {
            return Err(EndpointError::CredentialsInUrl);
        }
        // 경로는 허용하지만 권한부만 떼어 검사한다.
        let authority = rest.split('/').next().unwrap_or_default();
        if authority.is_empty() {
            return Err(EndpointError::MissingHost);
        }
        if authority.starts_with('[') {
            // IPv6 리터럴.
            return Err(EndpointError::HostIsAddressLiteral);
        }

        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => (
                host,
                port.parse::<u16>()
                    .map_err(|_| EndpointError::InvalidPort)?,
            ),
            None => (authority, 443u16),
        };
        if port == 0 {
            return Err(EndpointError::InvalidPort);
        }
        if host.is_empty() {
            return Err(EndpointError::MissingHost);
        }
        if host.split('.').all(|label| {
            !label.is_empty() && label.chars().all(|character| character.is_ascii_digit())
        }) {
            return Err(EndpointError::HostIsAddressLiteral);
        }
        let lowercase = host.to_ascii_lowercase();
        if lowercase == "localhost" || lowercase.ends_with(".localhost") {
            return Err(EndpointError::HostIsLoopback);
        }
        // 진짜 DNS 이름만 통과시킨다. 라벨은 1..=63바이트이고, 하이픈은 라벨 안에만 올 수
        // 있다(양끝 하이픈은 DNS에서 유효하지 않다).
        if !lowercase.contains('.') {
            return Err(EndpointError::InvalidHost);
        }
        let labels: Vec<&str> = lowercase.split('.').collect();
        if labels.iter().any(|label| {
            label.is_empty()
                || label.len() > 63
                || label.starts_with('-')
                || label.ends_with('-')
                || !label
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '-')
        }) {
            return Err(EndpointError::InvalidHost);
        }

        Ok(Self {
            url: raw.to_owned(),
            host: lowercase,
            port,
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub const fn port(&self) -> u16 {
        self.port
    }
}

/// URL 자체가 비밀은 아니지만, 로그에 원문을 흘리지 않는 습관을 타입으로 굳힌다.
impl std::fmt::Debug for RelayEndpoint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RelayEndpoint")
            .field("host", &self.host)
            .field("port", &self.port)
            .finish()
    }
}

/// 상한이 있는 지수 백오프. 지터는 주입받아 결정적으로 검증한다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BackoffPolicy {
    pub initial: Duration,
    pub maximum: Duration,
    pub multiplier: u32,
}

impl Default for BackoffPolicy {
    fn default() -> Self {
        Self {
            initial: Duration::from_secs(1),
            maximum: Duration::from_secs(60),
            multiplier: 2,
        }
    }
}

impl BackoffPolicy {
    /// `attempt`번째 재시도의 대기 시간. `jitter_ratio`는 0.0..=1.0이며 대기의 마지막 1/4을
    /// 흔든다 — 여러 Mac이 동시에 끊겼을 때 같은 순간에 몰려 돌아오지 않게 한다.
    pub fn delay(&self, attempt: u32, jitter_ratio: f64) -> Duration {
        let ratio = jitter_ratio.clamp(0.0, 1.0);
        let base = self
            .initial
            .checked_mul(self.multiplier.saturating_pow(attempt.min(16)))
            .unwrap_or(self.maximum)
            .min(self.maximum);
        let base_millis = base.as_millis() as u64;
        // 하한 75% + 지터 25%. 상한을 넘지 않는다.
        let floor = base_millis / 4 * 3;
        let span = base_millis - floor;
        Duration::from_millis(floor + (span as f64 * ratio) as u64)
    }
}

/// 왜 멈췄는가. 자동 재시도를 해서는 안 되는 이유들이 여기 모인다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HaltReason {
    /// 사용자가 껐다.
    Disabled,
    /// 서버가 자격증명을 거부했다.
    AuthenticationFailed,
    /// 이 기기의 인가가 취소됐다.
    Revoked,
    /// 앱이 종료 중이다.
    ShuttingDown,
    /// 엔드포인트 정책을 통과하지 못했다.
    EndpointRejected,
}

impl HaltReason {
    /// 사용자가 명시적으로 다시 시도하면 풀 수 있는가.
    ///
    /// 종료 중에는 아무것도 되살리지 않는다. 나머지는 사용자가 스위치를 다시 켜거나
    /// 새 페어링을 하면 풀린다.
    pub const fn is_user_recoverable(self) -> bool {
        !matches!(self, Self::ShuttingDown)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelayState {
    /// 꺼져 있거나, 자동 재시도를 해서는 안 되는 이유로 멈춰 있다.
    Halted(HaltReason),
    /// 다음 시도를 기다리는 중.
    Backoff { attempt: u32, until: u64 },
    /// 접속 시도 중.
    Connecting,
    /// 붙어 있음.
    Connected,
}

/// 워커가 따르는 단 하나의 수명주기. 이 값이 "지금 붙어도 되는가"의 유일한 근거다.
#[derive(Debug)]
pub struct RelayLifecycle {
    state: RelayState,
    backoff: BackoffPolicy,
    attempt: u32,
}

impl RelayLifecycle {
    /// 기본은 **꺼짐**이다. 사용자가 켜기 전에는 아무 소켓도 열리지 않는다.
    pub fn new(backoff: BackoffPolicy) -> Self {
        Self {
            state: RelayState::Halted(HaltReason::Disabled),
            backoff,
            attempt: 0,
        }
    }

    pub const fn state(&self) -> RelayState {
        self.state
    }

    pub const fn attempt(&self) -> u32 {
        self.attempt
    }

    /// 지금 소켓을 열어도 되는가. 워커는 매 바퀴 이것만 묻는다.
    pub const fn may_connect(&self, now: u64) -> bool {
        match self.state {
            RelayState::Halted(_) | RelayState::Connecting | RelayState::Connected => false,
            RelayState::Backoff { until, .. } => now >= until,
        }
    }

    /// 사용자가 켰다. 멈춰 있던 이유가 사용자로 풀 수 있는 것이면 즉시 시도한다.
    pub fn enable(&mut self, now: u64) -> bool {
        match self.state {
            RelayState::Halted(reason) if reason.is_user_recoverable() => {
                self.attempt = 0;
                self.state = RelayState::Backoff {
                    attempt: 0,
                    until: now,
                };
                true
            }
            _ => false,
        }
    }

    pub fn disable(&mut self) {
        self.halt(HaltReason::Disabled);
    }

    pub fn shutdown(&mut self) {
        self.state = RelayState::Halted(HaltReason::ShuttingDown);
        self.attempt = 0;
    }

    /// 되돌릴 수 없는 정지. 종료 중에는 다른 이유로 덮어쓰지 않는다.
    pub fn halt(&mut self, reason: HaltReason) {
        if matches!(self.state, RelayState::Halted(HaltReason::ShuttingDown)) {
            return;
        }
        self.state = RelayState::Halted(reason);
        self.attempt = 0;
    }

    /// 접속을 시작한다. **백오프 기한을 넘기지 못하면 시작하지 않는다** — 기한 확인과
    /// 상태 전이를 한 함수로 묶어야, 확인만 하고 전이는 그냥 하는 사용법이 생기지 않는다.
    pub fn begin_connect(&mut self, now: u64) -> bool {
        if !self.may_connect(now) {
            return false;
        }
        self.state = RelayState::Connecting;
        true
    }

    pub fn connected(&mut self) {
        if matches!(self.state, RelayState::Connecting) {
            self.state = RelayState::Connected;
            self.attempt = 0;
        }
    }

    /// 전송 실패. 재시도해도 되는 종류이므로 백오프로 물러난다.
    pub fn transport_failed(&mut self, now: u64, jitter_ratio: f64) -> Duration {
        if matches!(self.state, RelayState::Halted(_)) {
            return Duration::ZERO;
        }
        let delay = self.backoff.delay(self.attempt, jitter_ratio);
        self.attempt = self.attempt.saturating_add(1);
        self.state = RelayState::Backoff {
            attempt: self.attempt,
            until: now.saturating_add(delay.as_secs().max(1)),
        };
        delay
    }

    /// 자격증명이 거부됐다. **자동 재시도 금지** — 사용자가 다시 켜야 한다.
    pub fn authentication_failed(&mut self) {
        self.halt(HaltReason::AuthenticationFailed);
    }

    /// 이 기기가 취소됐다. 마찬가지로 자동 재시도 금지.
    pub fn revoked(&mut self) {
        self.halt(HaltReason::Revoked);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000;

    #[test]
    fn there_is_no_default_production_endpoint_yet() {
        assert_eq!(PRODUCTION_RELAY_ENDPOINT, None);
        assert_eq!(RelayEndpoint::production(), Err(EndpointError::NotAssigned));
    }

    #[test]
    fn only_a_wss_dns_endpoint_passes_the_policy() {
        let endpoint = RelayEndpoint::parse("wss://relay.example.test").unwrap();
        assert_eq!(endpoint.host(), "relay.example.test");
        assert_eq!(endpoint.port(), 443);
        assert_eq!(endpoint.url(), "wss://relay.example.test");

        let with_port = RelayEndpoint::parse("wss://relay.example.test:8443/socket").unwrap();
        assert_eq!(with_port.port(), 8443);
        assert_eq!(with_port.host(), "relay.example.test");
    }

    #[test]
    fn every_unsafe_endpoint_shape_is_refused() {
        for (raw, expected) in [
            ("ws://relay.example.test", EndpointError::NotWss),
            ("https://relay.example.test", EndpointError::NotWss),
            ("wss://", EndpointError::MissingHost),
            ("wss://127.0.0.1", EndpointError::HostIsAddressLiteral),
            ("wss://[::1]", EndpointError::HostIsAddressLiteral),
            ("wss://localhost", EndpointError::HostIsLoopback),
            ("wss://app.localhost", EndpointError::HostIsLoopback),
            (
                "wss://user:pass@relay.example.test",
                EndpointError::CredentialsInUrl,
            ),
            (
                "wss://relay.example.test?ticket=abc",
                EndpointError::QueryOrFragment,
            ),
            (
                "wss://relay.example.test#ticket",
                EndpointError::QueryOrFragment,
            ),
            ("wss://relay.example.test:0", EndpointError::InvalidPort),
            (
                "wss://relay.example.test:notaport",
                EndpointError::InvalidPort,
            ),
            ("wss://relay", EndpointError::InvalidHost),
            ("wss://relay..example.test", EndpointError::InvalidHost),
            ("wss://relay.example.test.", EndpointError::InvalidHost),
            ("wss://relay_example.test", EndpointError::InvalidHost),
            ("wss://-relay.example.test", EndpointError::InvalidHost),
            ("wss://relay-.example.test", EndpointError::InvalidHost),
        ] {
            assert_eq!(RelayEndpoint::parse(raw), Err(expected.clone()), "{raw}");
        }

        let long = format!("wss://{}.test", "a".repeat(MAX_ENDPOINT_BYTES));
        assert!(matches!(
            RelayEndpoint::parse(&long),
            Err(EndpointError::TooLong { .. })
        ));

        // 라벨 하나가 64바이트면 DNS 이름이 아니다(전체 길이 상한에는 걸리지 않는다).
        let long_label = format!("wss://{}.test", "a".repeat(64));
        assert_eq!(
            RelayEndpoint::parse(&long_label),
            Err(EndpointError::InvalidHost)
        );
        // 63바이트 라벨은 유효하다.
        let legal_label = format!("wss://{}.test", "a".repeat(63));
        assert!(RelayEndpoint::parse(&legal_label).is_ok());
    }

    /// 엔드포인트 원문을 로그에 흘리지 않는다.
    #[test]
    fn the_endpoint_debug_output_shows_only_host_and_port() {
        let endpoint = RelayEndpoint::parse("wss://relay.example.test:8443/socket").unwrap();
        let rendered = format!("{endpoint:?}");
        assert!(rendered.contains("relay.example.test") && rendered.contains("8443"));
        assert!(!rendered.contains("/socket"));
    }

    #[test]
    fn relay_starts_disabled_and_opens_no_socket_until_enabled() {
        let mut lifecycle = RelayLifecycle::new(BackoffPolicy::default());
        assert_eq!(lifecycle.state(), RelayState::Halted(HaltReason::Disabled));
        assert!(!lifecycle.may_connect(NOW));

        assert!(lifecycle.enable(NOW));
        assert!(lifecycle.may_connect(NOW));
    }

    #[test]
    fn backoff_grows_geometrically_stays_capped_and_carries_jitter() {
        let policy = BackoffPolicy::default();
        let no_jitter: Vec<u64> = (0..8)
            .map(|attempt| policy.delay(attempt, 0.0).as_millis() as u64)
            .collect();
        assert!(
            no_jitter.windows(2).all(|pair| pair[1] >= pair[0]),
            "{no_jitter:?}"
        );
        assert!(
            no_jitter
                .iter()
                .all(|delay| *delay <= policy.maximum.as_millis() as u64),
            "{no_jitter:?}"
        );
        assert_eq!(
            *no_jitter.last().unwrap(),
            policy.maximum.as_millis() as u64 / 4 * 3,
            "상한에 도달한 뒤에는 더 자라지 않는다"
        );

        // 지터는 대기의 마지막 1/4만 흔들고 상한을 넘지 않는다.
        for attempt in 0..8 {
            let low = policy.delay(attempt, 0.0);
            let high = policy.delay(attempt, 1.0);
            assert!(low <= high);
            assert!(high <= policy.maximum);
            assert!(low >= policy.initial.min(policy.maximum) / 4 * 3);
        }
    }

    #[test]
    fn a_transport_failure_backs_off_and_a_success_resets_the_attempt_count() {
        let mut lifecycle = RelayLifecycle::new(BackoffPolicy::default());
        lifecycle.enable(NOW);

        assert!(lifecycle.begin_connect(NOW));
        let first = lifecycle.transport_failed(NOW, 0.0);
        assert_eq!(lifecycle.attempt(), 1);
        assert!(!lifecycle.may_connect(NOW));

        assert!(lifecycle.begin_connect(NOW + 3_600));
        let second = lifecycle.transport_failed(NOW + 3_600, 0.0);
        assert!(second >= first);
        assert_eq!(lifecycle.attempt(), 2);

        assert!(lifecycle.begin_connect(NOW + 7_200));
        lifecycle.connected();
        assert_eq!(lifecycle.state(), RelayState::Connected);
        assert_eq!(lifecycle.attempt(), 0, "성공은 백오프를 되돌린다");
    }

    /// 폐기된 자격증명으로 무한히 문을 두드리지 않는다.
    #[test]
    fn authentication_failure_and_revocation_never_reconnect_on_their_own() {
        for (name, halt) in [
            ("auth", HaltReason::AuthenticationFailed),
            ("revoked", HaltReason::Revoked),
        ] {
            let mut lifecycle = RelayLifecycle::new(BackoffPolicy::default());
            lifecycle.enable(NOW);
            assert!(lifecycle.begin_connect(NOW));
            match halt {
                HaltReason::AuthenticationFailed => lifecycle.authentication_failed(),
                HaltReason::Revoked => lifecycle.revoked(),
                _ => unreachable!(),
            }

            assert_eq!(lifecycle.state(), RelayState::Halted(halt), "{name}");
            // 시간이 아무리 지나도 스스로 돌아오지 않는다.
            for elapsed in [1, 60, 3_600, 86_400] {
                assert!(!lifecycle.may_connect(NOW + elapsed), "{name} +{elapsed}");
            }
            // 전송 실패 통지가 와도 백오프로 되살아나지 않는다.
            assert_eq!(lifecycle.transport_failed(NOW, 0.0), Duration::ZERO);
            assert_eq!(lifecycle.state(), RelayState::Halted(halt), "{name}");

            // 사용자가 명시적으로 다시 켜야만 풀린다.
            assert!(lifecycle.enable(NOW + 10), "{name}");
            assert!(lifecycle.may_connect(NOW + 10), "{name}");
        }
    }

    /// 백오프 기한을 넘기지 않으면 접속을 시작할 수 없다. 확인과 전이가 한 함수라서
    /// "확인은 했지만 전이는 그냥" 하는 사용법이 애초에 생기지 않는다.
    #[test]
    fn a_connection_cannot_start_before_the_backoff_deadline() {
        let mut lifecycle = RelayLifecycle::new(BackoffPolicy::default());
        lifecycle.enable(NOW);
        assert!(lifecycle.begin_connect(NOW));

        let delay = lifecycle.transport_failed(NOW, 0.0);
        assert!(delay > Duration::ZERO);
        assert!(
            !lifecycle.begin_connect(NOW),
            "실패 직후 즉시 다시 붙으면 백오프가 무의미하다"
        );
        assert!(matches!(lifecycle.state(), RelayState::Backoff { .. }));

        let RelayState::Backoff { until, .. } = lifecycle.state() else {
            panic!("백오프 상태여야 한다");
        };
        assert!(!lifecycle.begin_connect(until - 1));
        assert!(lifecycle.begin_connect(until));
        assert_eq!(lifecycle.state(), RelayState::Connecting);
    }

    #[test]
    fn shutdown_is_final_and_no_other_transition_revives_it() {
        let mut lifecycle = RelayLifecycle::new(BackoffPolicy::default());
        lifecycle.enable(NOW);
        assert!(lifecycle.begin_connect(NOW));
        lifecycle.connected();
        lifecycle.shutdown();

        assert_eq!(
            lifecycle.state(),
            RelayState::Halted(HaltReason::ShuttingDown)
        );
        assert!(!lifecycle.begin_connect(NOW + 1));
        lifecycle.connected();
        lifecycle.halt(HaltReason::Disabled);
        assert_eq!(
            lifecycle.state(),
            RelayState::Halted(HaltReason::ShuttingDown),
            "종료는 다른 이유로 덮이지 않는다"
        );
        assert!(!lifecycle.enable(NOW + 1), "종료 뒤에는 켜지지 않는다");
        assert!(!lifecycle.may_connect(NOW + 86_400));
    }

    #[test]
    fn disabling_stops_reconnection_immediately_even_mid_backoff() {
        let mut lifecycle = RelayLifecycle::new(BackoffPolicy::default());
        lifecycle.enable(NOW);
        assert!(lifecycle.begin_connect(NOW));
        lifecycle.transport_failed(NOW, 0.0);
        assert!(matches!(lifecycle.state(), RelayState::Backoff { .. }));

        lifecycle.disable();
        assert_eq!(lifecycle.state(), RelayState::Halted(HaltReason::Disabled));
        assert!(!lifecycle.may_connect(NOW + 86_400));
    }

    /// 이 모듈에는 소켓도 TLS도 없다 — 그래서 위 성질들이 네트워크 없이 결정적이다.
    #[test]
    fn the_lifecycle_module_performs_no_io() {
        let source = include_str!("lifecycle.rs");
        let production = source.split("\n#[cfg(test)]\nmod tests {").next().unwrap();
        for forbidden in [
            "TcpStream",
            "tungstenite",
            "rustls",
            "std::thread",
            "SystemTime",
            "Instant",
        ] {
            assert!(!production.contains(forbidden), "{forbidden}");
        }
    }
}
