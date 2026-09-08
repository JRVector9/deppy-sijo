//! Relay 상태 기계 — I/O 없음.
//!
//! 소켓도 시계도 없다. 모든 입력은 `connection_opened`/`frame_received`/`connection_closed`/
//! `tick`/`queue_flushed`이고, 모든 출력은 [`RelayAction`] 목록이다. 그래서 속도 제한, 큐
//! 상한, 느린 소비자 절단, 시한 만료를 실제 네트워크 없이 결정적으로 검증할 수 있다.
//!
//! 상태 기계는 하나뿐이다:
//!
//! ```text
//! 연결 수립 → AwaitingAdmission ──DesktopAdmission(자격증명 일치)──▶ Desktop(라우트 소유)
//!                     │           ──DeviceAdmission(1회용 핸들)────▶ Device(라우트 합류)
//!                     └────────── 그 밖의 모든 프레임 / 시한 초과 ──▶ 절단
//! ```
//!
//! 자원(라우트·티켓·큐)은 **입장 판정 이후에만** 할당된다. 자격증명이 틀리거나 속도 제한에
//! 걸린 상대는 라우트도 큐도 얻지 못한다.

use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::fmt;

use relay_protocol::{
    AdmissionCredential, ConnectionId, DecodeError, FrameType, RejectionCode, RelayFrame, RouteId,
};

/// 서버가 배정하는 연결 키. 상대가 고르는 값이 아니므로 충돌시킬 수 없다.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConnectionKey(u64);

impl ConnectionKey {
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

/// 한 라우트를 소유할 자격을 증명하는 검증자. Mac 승인 자격증명의 provisioning/회전 주체는
/// 아직 정해지지 않았다(계획 Task 3 Step 2) — 그래서 여기서는 값을 주입만 받는다.
#[derive(Clone, Copy)]
pub struct RouteVerifier {
    route: RouteId,
    credential: AdmissionCredential,
}

impl RouteVerifier {
    pub const fn new(route: RouteId, credential: AdmissionCredential) -> Self {
        Self { route, credential }
    }

    pub const fn route(&self) -> RouteId {
        self.route
    }
}

impl fmt::Debug for RouteVerifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RouteVerifier")
            .field("route", &self.route)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RelayLimits {
    pub max_routes: usize,
    pub max_connections: usize,
    pub max_connections_per_ip: usize,
    pub max_tickets_per_route: usize,
    pub max_queue_frames: usize,
    pub max_queue_bytes: usize,
    /// 서버 전체 발신 예약의 상한. 연결당 상한만 두면
    /// `max_connections * max_queue_bytes` 만큼 자랄 수 있어 사실상 무제한이다.
    pub max_total_queue_bytes: usize,
    /// 속도 제한 창 표의 상한. 출발지 IP를 바꿔 가며 실패시키면 이 표가 끝없이 자란다.
    pub max_admission_windows: usize,
    pub admission_attempts_per_window: u32,
    pub admission_window_secs: u64,
    pub idle_timeout_secs: u64,
    pub handshake_timeout_secs: u64,
    pub ticket_ttl_secs: u64,
}

impl Default for RelayLimits {
    fn default() -> Self {
        Self {
            max_routes: 1024,
            // 연결 하나가 최대 프레임(1 MiB + 68바이트) 하나를 읽는 중일 수 있으므로
            // 이 값이 곧 전송 계층의 순간 읽기 버퍼 상한을 정한다.
            max_connections: 512,
            max_connections_per_ip: 16,
            max_tickets_per_route: 8,
            max_queue_frames: 64,
            max_queue_bytes: 2 * 1024 * 1024,
            max_total_queue_bytes: 64 * 1024 * 1024,
            max_admission_windows: 4096,
            admission_attempts_per_window: 8,
            admission_window_secs: 60,
            idle_timeout_secs: 60,
            handshake_timeout_secs: 10,
            ticket_ttl_secs: 300,
        }
    }
}

/// 연결을 아예 받지 않는 이유. 여기서 거절하면 큐도 라우트도 만들어지지 않는다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdmissionRefusal {
    TotalCapacity,
    PerIpCapacity,
    ShuttingDown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelayCoreError {
    UnknownConnection,
    DuplicateRoute,
    TooManyRoutes,
}

impl fmt::Display for RelayCoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownConnection => formatter.write_str("unknown relay connection"),
            Self::DuplicateRoute => formatter.write_str("duplicate relay route verifier"),
            Self::TooManyRoutes => formatter.write_str("relay route verifiers exceed the bound"),
        }
    }
}

impl std::error::Error for RelayCoreError {}

/// 전송 계층이 수행해야 할 일. 코어는 절대 직접 쓰지 않는다.
#[derive(Clone, PartialEq, Eq)]
pub enum RelayAction {
    Send {
        connection: ConnectionKey,
        frame: Vec<u8>,
    },
    Disconnect {
        connection: ConnectionKey,
        code: RejectionCode,
    },
}

/// 프레임 내용은 절대 렌더링하지 않는다 — 길이만 남긴다.
impl fmt::Debug for RelayAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Send { connection, frame } => formatter
                .debug_struct("Send")
                .field("connection", connection)
                .field("frame_bytes", &frame.len())
                .finish(),
            Self::Disconnect { connection, code } => formatter
                .debug_struct("Disconnect")
                .field("connection", connection)
                .field("code", code)
                .finish(),
        }
    }
}

/// 개수만 담는 관측값. 어떤 필드도 애플리케이션 바이트를 담지 않는다.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RelayStats {
    pub connections: usize,
    pub routes: usize,
    pub devices: usize,
    pub tickets: usize,
    pub queued_frames: usize,
    pub queued_bytes: usize,
    pub forwarded_frames: u64,
    pub rejected_admissions: u64,
    pub rate_limited_admissions: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConnectionState {
    AwaitingAdmission,
    Desktop { route: RouteId },
    Device { route: RouteId },
}

struct Connection {
    ip: [u8; 16],
    opened_at: u64,
    last_seen_at: u64,
    state: ConnectionState,
    /// 아직 전송되지 않은 프레임의 바이트 수. 프레임 수와 바이트 수를 **둘 다** 예약한다 —
    /// 프레임 수만 세면 거대한 레코드 몇 개로 메모리가 터진다.
    queued: VecDeque<usize>,
    queued_bytes: usize,
}

struct Ticket {
    handle: AdmissionCredential,
    published_at: u64,
}

struct ReconnectGrant {
    verifier: AdmissionCredential,
    published_at: u64,
    expires_at: u64,
}

struct Route {
    desktop: ConnectionKey,
    device: Option<ConnectionKey>,
    session: Option<ConnectionId>,
    tickets: Vec<Ticket>,
    grants: Vec<ReconnectGrant>,
    device_grant: Option<AdmissionCredential>,
    reconnect_ready: bool,
}

struct RateWindow {
    started_at: u64,
    attempts: u32,
}

pub struct RelayCore {
    limits: RelayLimits,
    verifiers: Vec<RouteVerifier>,
    connections: HashMap<ConnectionKey, Connection>,
    per_ip: HashMap<[u8; 16], usize>,
    routes: HashMap<RouteId, Route>,
    admission_windows: HashMap<[u8; 16], RateWindow>,
    next_key: u64,
    total_queued_bytes: usize,
    /// 코어는 끊기로 판정했지만 전송 계층이 아직 실제로 버리지 못한 바이트.
    ///
    /// 판정 즉시 예약을 풀어 버리면, 그 바이트가 아직 전송 채널에 살아 있는 동안 새 연결이
    /// 같은 예산을 다시 채운다. 그러면 프로세스가 실제로 들고 있는 양이 전체 상한을 넘는다.
    /// 그래서 `connection_closed`로 "정말 사라졌다"는 확인이 올 때까지 예산을 붙잡아 둔다.
    draining: HashMap<ConnectionKey, usize>,
    draining_bytes: usize,
    shutting_down: bool,
    forwarded_frames: u64,
    rejected_admissions: u64,
    rate_limited_admissions: u64,
}

impl fmt::Debug for RelayCore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RelayCore")
            .field("stats", &self.stats())
            .field("shutting_down", &self.shutting_down)
            .finish()
    }
}

impl RelayCore {
    pub fn new(limits: RelayLimits, verifiers: Vec<RouteVerifier>) -> Result<Self, RelayCoreError> {
        if verifiers.len() > limits.max_routes {
            return Err(RelayCoreError::TooManyRoutes);
        }
        for (index, verifier) in verifiers.iter().enumerate() {
            if verifiers[..index]
                .iter()
                .any(|earlier| earlier.route == verifier.route)
            {
                return Err(RelayCoreError::DuplicateRoute);
            }
        }
        Ok(Self {
            limits,
            verifiers,
            connections: HashMap::new(),
            per_ip: HashMap::new(),
            routes: HashMap::new(),
            admission_windows: HashMap::new(),
            next_key: 1,
            total_queued_bytes: 0,
            draining: HashMap::new(),
            draining_bytes: 0,
            shutting_down: false,
            forwarded_frames: 0,
            rejected_admissions: 0,
            rate_limited_admissions: 0,
        })
    }

    pub fn stats(&self) -> RelayStats {
        RelayStats {
            connections: self.connections.len(),
            routes: self.routes.len(),
            devices: self.route_device_count(),
            tickets: self.ticket_count(),
            queued_frames: self
                .connections
                .values()
                .map(|connection| connection.queued.len())
                .sum(),
            queued_bytes: self.queued_bytes(),
            forwarded_frames: self.forwarded_frames,
            rejected_admissions: self.rejected_admissions,
            rate_limited_admissions: self.rate_limited_admissions,
        }
    }

    pub fn connection_count(&self) -> usize {
        self.connections.len()
    }

    pub fn route_count(&self) -> usize {
        self.routes.len()
    }

    pub fn route_device_count(&self) -> usize {
        self.routes
            .values()
            .filter(|route| route.device.is_some())
            .count()
    }

    pub fn ticket_count(&self) -> usize {
        self.routes.values().map(|route| route.tickets.len()).sum()
    }

    pub fn admission_window_count(&self) -> usize {
        self.admission_windows.len()
    }

    /// 프로세스가 이 순간 실제로 붙잡고 있는 발신 바이트 — 예약분과 아직 버려지지 않은
    /// 정리 대기분을 합친 값이다.
    pub fn queued_bytes(&self) -> usize {
        self.total_queued_bytes + self.draining_bytes
    }

    /// 아직 전송 계층이 버렸다고 확인해 주지 않은 바이트.
    pub fn draining_bytes(&self) -> usize {
        self.draining_bytes
    }

    /// 이 연결이 입장 판정을 통과했는가. 전송 계층이 입장 전후로 읽기 상한을 달리 잡는 데 쓴다.
    pub fn connection_is_admitted(&self, connection: ConnectionKey) -> bool {
        self.connections
            .get(&connection)
            .is_some_and(|entry| !matches!(entry.state, ConnectionState::AwaitingAdmission))
    }

    /// 소켓 하나가 열렸다. 여기서 거절하면 라우트·티켓·큐 어느 것도 할당되지 않는다.
    pub fn connection_opened(
        &mut self,
        ip: [u8; 16],
        now: u64,
    ) -> Result<ConnectionKey, AdmissionRefusal> {
        if self.shutting_down {
            return Err(AdmissionRefusal::ShuttingDown);
        }
        if self.connections.len() >= self.limits.max_connections {
            return Err(AdmissionRefusal::TotalCapacity);
        }
        let per_ip = self.per_ip.entry(ip).or_insert(0);
        if *per_ip >= self.limits.max_connections_per_ip {
            return Err(AdmissionRefusal::PerIpCapacity);
        }
        *per_ip += 1;

        let key = ConnectionKey(self.next_key);
        self.next_key += 1;
        self.connections.insert(
            key,
            Connection {
                ip,
                opened_at: now,
                last_seen_at: now,
                state: ConnectionState::AwaitingAdmission,
                queued: VecDeque::new(),
                queued_bytes: 0,
            },
        );
        Ok(key)
    }

    /// 전송 계층이 이 연결로 `frames`개를 실제로 내보냈다. 예약을 그만큼 푼다.
    pub fn queue_flushed(&mut self, connection: ConnectionKey, frames: usize) {
        let Some(entry) = self.connections.get_mut(&connection) else {
            return;
        };
        for _ in 0..frames {
            let Some(bytes) = entry.queued.pop_front() else {
                break;
            };
            entry.queued_bytes -= bytes;
            self.total_queued_bytes -= bytes;
        }
    }

    pub fn frame_received(
        &mut self,
        connection: ConnectionKey,
        bytes: &[u8],
        now: u64,
    ) -> Result<Vec<RelayAction>, RelayCoreError> {
        if !self.connections.contains_key(&connection) {
            return Err(RelayCoreError::UnknownConnection);
        }
        let frame = match RelayFrame::decode(bytes) {
            Ok((frame, consumed)) if consumed == bytes.len() => frame,
            // 부분 프레임이나 한 번에 여러 프레임은 전송 계층이 이미 나눠 준다. 여기 오면
            // 프레이밍 계약 위반이다.
            Ok(_) => return Ok(self.reject(connection, RejectionCode::MalformedFrame)),
            Err(error) => return Ok(self.reject(connection, decode_rejection(error))),
        };

        if let Some(entry) = self.connections.get_mut(&connection) {
            entry.last_seen_at = now;
        }

        let state = self.connections[&connection].state;
        match (state, frame.frame_type()) {
            (ConnectionState::AwaitingAdmission, FrameType::DesktopAdmission) => {
                Ok(self.admit_desktop(connection, &frame, now))
            }
            (
                ConnectionState::AwaitingAdmission,
                FrameType::DeviceAdmission | FrameType::ReconnectAdmission,
            ) => Ok(self.admit_device(connection, &frame, now)),
            (ConnectionState::AwaitingAdmission, _) => {
                Ok(self.reject(connection, RejectionCode::MalformedFrame))
            }
            (
                ConnectionState::Desktop { route },
                FrameType::ReconnectPublish | FrameType::ReconnectRevoke | FrameType::ReconnectSync,
            ) => Ok(self.reconnect_control(connection, route, &frame, now)),
            (ConnectionState::Desktop { route }, FrameType::TicketPublish) => {
                Ok(self.publish_ticket(connection, route, &frame, now))
            }
            (ConnectionState::Desktop { route }, FrameType::TicketRevoke) => {
                Ok(self.revoke_ticket(connection, route, &frame))
            }
            (
                ConnectionState::Desktop { route } | ConnectionState::Device { route },
                FrameType::Heartbeat,
            ) => {
                if frame.route_id() != route {
                    return Ok(self.reject(connection, RejectionCode::RouteUnknown));
                }
                // 생존 신호는 홉 단위다 — 상대에게 전달하지 않는다.
                Ok(Vec::new())
            }
            (
                ConnectionState::Desktop { route } | ConnectionState::Device { route },
                FrameType::Hello | FrameType::Ciphertext | FrameType::Close,
            ) => Ok(self.forward(connection, route, &frame, bytes)),
            // 서버가 보내는 제어 프레임을 상대가 되쏘는 것은 계약 위반이다.
            _ => Ok(self.reject(connection, RejectionCode::MalformedFrame)),
        }
    }

    /// 전송 계층이 이 연결의 소켓과 발신 버퍼를 실제로 버렸다는 확인. 정리 대기 예산은
    /// 이 시점에야 풀린다.
    pub fn connection_closed(&mut self, connection: ConnectionKey, _now: u64) -> Vec<RelayAction> {
        if let Some(bytes) = self.draining.remove(&connection) {
            self.draining_bytes -= bytes;
        }
        let Some(entry) = self.connections.remove(&connection) else {
            return Vec::new();
        };
        self.total_queued_bytes -= entry.queued_bytes;
        self.release_ip(entry.ip);

        match entry.state {
            ConnectionState::AwaitingAdmission => Vec::new(),
            ConnectionState::Desktop { route } => {
                // 라우트 소유자가 사라지면 티켓과 합류한 기기까지 함께 정리한다.
                let Some(removed) = self.routes.remove(&route) else {
                    return Vec::new();
                };
                match removed.device {
                    Some(device) => {
                        let mut actions = vec![RelayAction::Disconnect {
                            connection: device,
                            code: RejectionCode::PeerDisconnected,
                        }];
                        actions.extend(self.drop_connection(device));
                        actions
                    }
                    None => Vec::new(),
                }
            }
            ConnectionState::Device { route } => {
                let Some(entry) = self.routes.get_mut(&route) else {
                    return Vec::new();
                };
                entry.device = None;
                entry.device_grant = None;
                entry.session = None;
                let desktop = entry.desktop;
                self.notify(
                    desktop,
                    FrameType::PeerLeft,
                    route,
                    ConnectionId::from_bytes([0; 16]),
                )
                .map_or_else(Vec::new, |action| vec![action])
            }
        }
    }

    /// 시한 만료 정리. 입장 전 연결은 더 짧은 핸드셰이크 시한을 받는다.
    pub fn tick(&mut self, now: u64) -> Vec<RelayAction> {
        self.expire_tickets(now);
        self.prune_admission_windows(now);

        let mut expired: Vec<ConnectionKey> = self
            .connections
            .iter()
            .filter(|(_, connection)| match connection.state {
                ConnectionState::AwaitingAdmission => {
                    now.saturating_sub(connection.opened_at) >= self.limits.handshake_timeout_secs
                }
                _ => now.saturating_sub(connection.last_seen_at) >= self.limits.idle_timeout_secs,
            })
            .map(|(key, _)| *key)
            .collect();
        expired.sort_unstable();

        let mut actions = Vec::new();
        for key in expired {
            // 앞선 연결을 정리하면서 연쇄로 이미 걷어낸 연결일 수 있다(데스크톱이 만료되면
            // 붙어 있던 기기도 함께 사라진다). 그때 두 번째 절단 지시를 내면 안 된다.
            if !self.connections.contains_key(&key) {
                continue;
            }
            actions.push(RelayAction::Disconnect {
                connection: key,
                code: RejectionCode::IdleTimeout,
            });
            actions.extend(self.drop_connection(key));
        }
        actions
    }

    /// 모든 연결을 정확히 한 번 닫고 상태를 비운다.
    pub fn shutdown(&mut self, _now: u64) -> Vec<RelayAction> {
        if self.shutting_down {
            return Vec::new();
        }
        self.shutting_down = true;
        let mut keys: Vec<ConnectionKey> = self.connections.keys().copied().collect();
        keys.sort_unstable();
        let actions = keys
            .into_iter()
            .map(|connection| RelayAction::Disconnect {
                connection,
                code: RejectionCode::ShuttingDown,
            })
            .collect();
        self.connections.clear();
        self.per_ip.clear();
        self.routes.clear();
        self.admission_windows.clear();
        self.total_queued_bytes = 0;
        self.draining.clear();
        self.draining_bytes = 0;
        actions
    }

    fn admit_desktop(
        &mut self,
        connection: ConnectionKey,
        frame: &RelayFrame<'_>,
        now: u64,
    ) -> Vec<RelayAction> {
        if let Some(limited) = self.rate_limit(connection, now) {
            return limited;
        }
        let Some(credential) = frame.admission_credential() else {
            return self.reject(connection, RejectionCode::MalformedFrame);
        };
        // 라우트가 없을 때와 자격증명이 틀릴 때를 같은 코드로 답한다 — 라우트 존재 여부를
        // 응답으로 구별할 수 없게 한다.
        let route = frame.route_id();
        // 조기 종료하지 않는다. `find`로 라우트를 먼저 찾으면 "그런 라우트가 없다"와
        // "라우트는 있는데 자격증명이 틀리다"가 걸린 시간으로 구별된다.
        let mut matched = false;
        for verifier in &self.verifiers {
            let route_matches = constant_time_eq_16(verifier.route.as_bytes(), route.as_bytes());
            let credential_matches = credential.matches(&verifier.credential);
            matched |= route_matches & credential_matches;
        }
        if !matched {
            self.rejected_admissions += 1;
            return self.reject(connection, RejectionCode::CredentialRejected);
        }
        if self.routes.contains_key(&route) {
            self.rejected_admissions += 1;
            return self.reject(connection, RejectionCode::RouteBusy);
        }
        if self.routes.len() >= self.limits.max_routes {
            self.rejected_admissions += 1;
            return self.reject(connection, RejectionCode::CapacityReached);
        }

        self.routes.insert(
            route,
            Route {
                desktop: connection,
                device: None,
                session: None,
                tickets: Vec::new(),
                grants: Vec::new(),
                device_grant: None,
                reconnect_ready: false,
            },
        );
        if let Some(entry) = self.connections.get_mut(&connection) {
            entry.state = ConnectionState::Desktop { route };
        }
        self.notify(
            connection,
            FrameType::Admitted,
            route,
            frame.connection_id(),
        )
        .map_or_else(Vec::new, |action| vec![action])
    }

    fn admit_device(
        &mut self,
        connection: ConnectionKey,
        frame: &RelayFrame<'_>,
        now: u64,
    ) -> Vec<RelayAction> {
        if let Some(limited) = self.rate_limit(connection, now) {
            return limited;
        }
        let Some(handle) = frame.admission_credential() else {
            return self.reject(connection, RejectionCode::MalformedFrame);
        };
        // 연결 id는 **기기가 정한다** — 이 헤더 값이 그대로 라우트 세션 id가 되고, 기기에는
        // `Admitted`로, Mac에는 `PeerJoined`로 되돌아가 세 당사자가 같은 값에 합의한다.
        // 전0은 계약상 "연결 없음"이라(Mac이 자기 입장 프레임에 쓰는 값) 세션 id가 될 수
        // 없다. 받아 주면 Mac의 핸드셰이크가 그 값에 묶여, 이후 어떤 프레임이 어느 세션의
        // 것인지 구분하지 못한다.
        if frame
            .connection_id()
            .as_bytes()
            .iter()
            .all(|byte| *byte == 0)
        {
            return self.reject(connection, RejectionCode::MalformedFrame);
        }
        let mut route = frame.route_id();
        let ticket_ttl = self.limits.ticket_ttl_secs;
        // v1 페어링 링크에는 route가 없다. 상한 있는 ticket 표에서 유일한 후보만 찾는다.
        // 명시적 route나 재접속에는 이 해석을 적용하지 않아 다른 route를 우회하지 못한다.
        if frame.frame_type() == FrameType::DeviceAdmission && route.as_bytes() == &[0; 16] {
            let mut candidates = self.routes.iter().filter(|(_, entry)| {
                entry.tickets.iter().any(|ticket| {
                    ticket.handle.matches(&handle)
                        && now >= ticket.published_at
                        && now - ticket.published_at < ticket_ttl
                })
            });
            let found = candidates.next().map(|(route, _)| *route);
            if found.is_none() || candidates.next().is_some() {
                self.rejected_admissions += 1;
                return self.reject(connection, RejectionCode::TicketUnknown);
            }
            route = found.expect("유일한 후보 확인");
        }
        let Some(entry) = self.routes.get_mut(&route) else {
            self.rejected_admissions += 1;
            // 존재하지 않는 라우트와 모르는 티켓을 같은 코드로 답한다.
            return self.reject(connection, RejectionCode::TicketUnknown);
        };
        let reconnect = frame.frame_type() == FrameType::ReconnectAdmission;
        // DB 복원은 여러 프레임에 걸친다. 완료 전의 빈 registry는 회수가 아니다.
        if reconnect && !entry.reconnect_ready {
            return self.reject(connection, RejectionCode::RouteBusy);
        }
        let verifier = AdmissionCredential::from_bytes(Sha256::digest(handle.as_bytes()).into());
        if reconnect {
            if !entry.grants.iter().any(|grant| {
                grant.verifier.matches(&verifier)
                    && now >= grant.published_at
                    && now < grant.expires_at
            }) {
                self.rejected_admissions += 1;
                return self.reject(connection, RejectionCode::CredentialRejected);
            }
        } else {
            // 기존 페어링은 5분·1회용이다. 바쁜 라우트에서도 먼저 소비한다.
            let Some(index) = entry.tickets.iter().position(|ticket| {
                ticket.handle.matches(&handle)
                    && now.saturating_sub(ticket.published_at) < ticket_ttl
            }) else {
                self.rejected_admissions += 1;
                return self.reject(connection, RejectionCode::TicketUnknown);
            };
            entry.tickets.remove(index);
        }
        if entry.device.is_some() {
            self.rejected_admissions += 1;
            return self.reject(connection, RejectionCode::RouteBusy);
        }

        entry.device_grant = reconnect.then_some(verifier);
        entry.device = Some(connection);
        entry.session = Some(frame.connection_id());
        let desktop = entry.desktop;

        if let Some(state) = self.connections.get_mut(&connection) {
            state.state = ConnectionState::Device { route };
        }
        let mut actions = Vec::new();
        if let Some(action) = self.notify(
            connection,
            FrameType::Admitted,
            route,
            frame.connection_id(),
        ) {
            actions.push(action);
        }
        if let Some(action) =
            self.notify(desktop, FrameType::PeerJoined, route, frame.connection_id())
        {
            actions.push(action);
        }
        actions
    }

    /// 재접속 검증자는 권한이 아니라 자원 입장 필터다. 실제 신원은 종단 간 검증한다.
    fn reconnect_control(
        &mut self,
        connection: ConnectionKey,
        route: RouteId,
        frame: &RelayFrame<'_>,
        now: u64,
    ) -> Vec<RelayAction> {
        if frame.route_id() != route {
            return self.reject(connection, RejectionCode::RouteUnknown);
        }
        if frame.frame_type() == FrameType::ReconnectSync {
            let Some(entry) = self.routes.get_mut(&route) else {
                return self.reject(connection, RejectionCode::RouteUnknown);
            };
            entry.reconnect_ready = true;
            return Vec::new();
        }
        let verifier = AdmissionCredential::from_bytes(
            frame.payload()[..32].try_into().expect("wire 길이 검증"),
        );
        let Some(entry) = self.routes.get_mut(&route) else {
            return self.reject(connection, RejectionCode::RouteUnknown);
        };
        entry
            .grants
            .retain(|grant| now >= grant.published_at && now < grant.expires_at);
        if frame.frame_type() == FrameType::ReconnectRevoke {
            entry
                .grants
                .retain(|grant| !grant.verifier.matches(&verifier));
            let active = entry
                .device_grant
                .is_some_and(|active| active.matches(&verifier));
            let device = active.then_some(entry.device).flatten();
            if let Some(device) = device {
                return self.reject(device, RejectionCode::CredentialRejected);
            }
            return Vec::new();
        }
        let expires_at =
            u64::from_be_bytes(frame.payload()[32..40].try_into().expect("wire 길이 검증"));
        if expires_at <= now || expires_at - now > relay_protocol::MAX_RECONNECT_LIFETIME_SECS {
            return self.reject(connection, RejectionCode::MalformedFrame);
        }
        if let Some(existing) = entry
            .grants
            .iter_mut()
            .find(|grant| grant.verifier.matches(&verifier))
        {
            // 재게시는 수명을 늘리지 않는다.
            existing.expires_at = existing.expires_at.min(expires_at);
        } else {
            if entry.grants.len() >= relay_protocol::MAX_RECONNECT_GRANTS {
                return self.control(
                    connection,
                    FrameType::Rejected,
                    route,
                    RejectionCode::CapacityReached,
                );
            }
            entry.grants.push(ReconnectGrant {
                verifier,
                published_at: now,
                expires_at,
            });
        }
        let ack = RelayFrame::new(
            FrameType::ReconnectPublished,
            route,
            frame.connection_id(),
            0,
            verifier.as_bytes(),
        )
        .expect("고정 ACK 길이")
        .to_vec();
        self.enqueue(connection, ack).into_iter().collect()
    }

    fn publish_ticket(
        &mut self,
        connection: ConnectionKey,
        route: RouteId,
        frame: &RelayFrame<'_>,
        now: u64,
    ) -> Vec<RelayAction> {
        if frame.route_id() != route {
            return self.reject(connection, RejectionCode::RouteUnknown);
        }
        let Some(handle) = frame.admission_credential() else {
            return self.reject(connection, RejectionCode::MalformedFrame);
        };
        let maximum = self.limits.max_tickets_per_route;
        let Some(entry) = self.routes.get_mut(&route) else {
            return self.reject(connection, RejectionCode::RouteUnknown);
        };
        if entry
            .tickets
            .iter()
            .any(|ticket| ticket.handle.matches(&handle))
        {
            // 같은 핸들의 재등록은 Mac의 멱등 재시도다. 끊으면 라우트와 붙어 있던 기기까지
            // 함께 사라진다 — 인접한 용량 경로처럼 치명적이지 않은 제어 프레임으로 답한다.
            return self.control(
                connection,
                FrameType::Rejected,
                route,
                RejectionCode::TicketConsumed,
            );
        }
        if entry.tickets.len() >= maximum {
            // 상한에 닿으면 조용히 밀어내지 않고 거절한다.
            return self.control(
                connection,
                FrameType::Rejected,
                route,
                RejectionCode::CapacityReached,
            );
        }
        entry.tickets.push(Ticket {
            handle,
            published_at: now,
        });
        Vec::new()
    }

    fn revoke_ticket(
        &mut self,
        connection: ConnectionKey,
        route: RouteId,
        frame: &RelayFrame<'_>,
    ) -> Vec<RelayAction> {
        if frame.route_id() != route {
            return self.reject(connection, RejectionCode::RouteUnknown);
        }
        let Some(handle) = frame.admission_credential() else {
            return self.reject(connection, RejectionCode::MalformedFrame);
        };
        if let Some(entry) = self.routes.get_mut(&route) {
            entry
                .tickets
                .retain(|ticket| !ticket.handle.matches(&handle));
        }
        Vec::new()
    }

    /// 상대에게 바이트를 **그대로** 넘긴다. 파싱도 로깅도 변형도 하지 않는다.
    fn forward(
        &mut self,
        sender: ConnectionKey,
        route: RouteId,
        frame: &RelayFrame<'_>,
        bytes: &[u8],
    ) -> Vec<RelayAction> {
        if frame.route_id() != route {
            return self.reject(sender, RejectionCode::RouteUnknown);
        }
        let Some(entry) = self.routes.get(&route) else {
            return self.reject(sender, RejectionCode::RouteUnknown);
        };
        // 세션 연결 id는 기기가 합류할 때 정해진다. 그 뒤로 양쪽 모두 그 값만 쓸 수 있다.
        match entry.session {
            Some(session) if session != frame.connection_id() => {
                return self.reject(sender, RejectionCode::MalformedFrame);
            }
            None => return Vec::new(),
            Some(_) => {}
        }
        let peer = if entry.desktop == sender {
            entry.device
        } else {
            Some(entry.desktop)
        };
        let Some(peer) = peer else {
            return Vec::new();
        };

        if !self.connections.contains_key(&peer) {
            return Vec::new();
        }
        // 예약을 먼저 한다. 상한을 넘기면 보낸 쪽이 아니라 **느린 쪽**을 끊는다 —
        // 그러지 않으면 느린 소비자 하나가 정상적인 상대를 밀어낸다.
        match self.enqueue(peer, bytes.to_vec()) {
            Some(action) => {
                self.forwarded_frames += 1;
                vec![action]
            }
            None => {
                let mut actions = vec![RelayAction::Disconnect {
                    connection: peer,
                    code: RejectionCode::QueueOverflow,
                }];
                actions.extend(self.drop_connection(peer));
                actions
            }
        }
    }

    /// 이 IP의 입장 시도를 창 단위로 센다. 결과와 무관하게 시도 자체를 센다 —
    /// 성공만 세면 추측 공격이 무료가 된다.
    fn rate_limit(&mut self, connection: ConnectionKey, now: u64) -> Option<Vec<RelayAction>> {
        let ip = self.connections.get(&connection)?.ip;
        let window = self.limits.admission_window_secs;
        let allowance = self.limits.admission_attempts_per_window;

        // 새 IP를 넣기 전에 만료된 창을 먼저 걷어낸다. 그러지 않으면 출발지 IP를 바꿔
        // 가며 실패시키는 것만으로 이 표가 끝없이 자란다.
        if !self.admission_windows.contains_key(&ip)
            && self.admission_windows.len() >= self.limits.max_admission_windows
        {
            self.prune_admission_windows(now);
            if self.admission_windows.len() >= self.limits.max_admission_windows {
                // 창 표가 가득 찼다. 새 출발지를 위해 기존 상태를 버리는 대신 거절한다 —
                // 버리면 그것이 곧 속도 제한 우회 수단이 된다.
                self.rate_limited_admissions += 1;
                return Some(self.reject(connection, RejectionCode::RateLimited));
            }
        }

        let entry = self.admission_windows.entry(ip).or_insert(RateWindow {
            started_at: now,
            attempts: 0,
        });
        if now.saturating_sub(entry.started_at) >= window {
            entry.started_at = now;
            entry.attempts = 0;
        }
        if entry.attempts >= allowance {
            self.rate_limited_admissions += 1;
            return Some(self.reject(connection, RejectionCode::RateLimited));
        }
        entry.attempts += 1;
        None
    }

    /// 창이 지난 항목을 표에서 걷어낸다. 창이 지나면 그 IP의 상태는 어차피 초기화된다.
    fn prune_admission_windows(&mut self, now: u64) {
        let window = self.limits.admission_window_secs;
        self.admission_windows
            .retain(|_, entry| now.saturating_sub(entry.started_at) < window);
    }

    fn expire_tickets(&mut self, now: u64) {
        let ttl = self.limits.ticket_ttl_secs;
        for route in self.routes.values_mut() {
            route
                .grants
                .retain(|grant| now >= grant.published_at && now < grant.expires_at);
            route
                .tickets
                .retain(|ticket| now.saturating_sub(ticket.published_at) < ttl);
        }
    }

    /// 발신 프레임 하나를 이 연결의 큐에 **예약하고** 전송 지시를 만든다. 제어 프레임도
    /// 예외가 아니다 — 예약 없이 내보낸 프레임을 전송 계층이 `queue_flushed`로 세면, 아직
    /// 안 나간 데이터 프레임의 예약이 풀려 상한이 한 프레임씩 새어 나간다.
    /// 예산을 넘기면 지시를 만들지 않는다(제어 프레임은 잃어도 되고, 데이터는 `forward`가
    /// 따로 느린 소비자로 처리한다).
    fn enqueue(&mut self, connection: ConnectionKey, frame: Vec<u8>) -> Option<RelayAction> {
        let target = self.connections.get_mut(&connection)?;
        if target.queued.len() + 1 > self.limits.max_queue_frames
            || target.queued_bytes + frame.len() > self.limits.max_queue_bytes
            || self.total_queued_bytes + self.draining_bytes + frame.len()
                > self.limits.max_total_queue_bytes
        {
            return None;
        }
        target.queued.push_back(frame.len());
        target.queued_bytes += frame.len();
        self.total_queued_bytes += frame.len();
        Some(RelayAction::Send { connection, frame })
    }

    /// 거절은 언제나 코드 하나를 보내고 즉시 끊는다. 사유 문자열은 와이어에 없다.
    fn reject(&mut self, connection: ConnectionKey, code: RejectionCode) -> Vec<RelayAction> {
        let mut actions = Vec::new();
        let route = match self.connections.get(&connection).map(|entry| entry.state) {
            Some(ConnectionState::Desktop { route } | ConnectionState::Device { route }) => route,
            _ => RouteId::from_bytes([0; 16]),
        };
        actions.extend(self.control(connection, FrameType::Rejected, route, code));
        actions.push(RelayAction::Disconnect { connection, code });
        actions.extend(self.drop_connection(connection));
        actions
    }

    fn control(
        &mut self,
        connection: ConnectionKey,
        frame_type: FrameType,
        route: RouteId,
        code: RejectionCode,
    ) -> Vec<RelayAction> {
        let payload = code.to_bytes();
        let Ok(frame) = RelayFrame::new(
            frame_type,
            route,
            ConnectionId::from_bytes([0; 16]),
            0,
            &payload,
        ) else {
            return Vec::new();
        };
        self.enqueue(connection, frame.to_vec())
            .map_or_else(Vec::new, |action| vec![action])
    }

    fn notify(
        &mut self,
        connection: ConnectionKey,
        frame_type: FrameType,
        route: RouteId,
        session: ConnectionId,
    ) -> Option<RelayAction> {
        let frame = RelayFrame::new(frame_type, route, session, 0, &[]).ok()?;
        self.enqueue(connection, frame.to_vec())
    }

    /// 연결 하나를 상태에서 완전히 걷어낸다. 라우트·티켓·큐·IP 카운트를 모두 되돌린다.
    fn drop_connection(&mut self, connection: ConnectionKey) -> Vec<RelayAction> {
        let Some(entry) = self.connections.remove(&connection) else {
            return Vec::new();
        };
        self.total_queued_bytes -= entry.queued_bytes;
        if entry.queued_bytes > 0 {
            self.draining.insert(connection, entry.queued_bytes);
            self.draining_bytes += entry.queued_bytes;
        }
        self.release_ip(entry.ip);
        match entry.state {
            ConnectionState::AwaitingAdmission => Vec::new(),
            ConnectionState::Desktop { route } => {
                let Some(removed) = self.routes.remove(&route) else {
                    return Vec::new();
                };
                match removed.device {
                    Some(device) => {
                        let mut actions = vec![RelayAction::Disconnect {
                            connection: device,
                            code: RejectionCode::PeerDisconnected,
                        }];
                        actions.extend(self.drop_connection(device));
                        actions
                    }
                    None => Vec::new(),
                }
            }
            ConnectionState::Device { route } => {
                let Some(entry) = self.routes.get_mut(&route) else {
                    return Vec::new();
                };
                entry.device = None;
                entry.device_grant = None;
                entry.session = None;
                let desktop = entry.desktop;
                self.notify(
                    desktop,
                    FrameType::PeerLeft,
                    route,
                    ConnectionId::from_bytes([0; 16]),
                )
                .map_or_else(Vec::new, |action| vec![action])
            }
        }
    }

    fn release_ip(&mut self, ip: [u8; 16]) {
        if let Some(count) = self.per_ip.get_mut(&ip) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.per_ip.remove(&ip);
            }
        }
    }
}

/// 라우트 id 비교도 조기 종료하지 않는다 — 검증자 루프 전체가 분기 없이 돌아야 한다.
fn constant_time_eq_16(left: &[u8; 16], right: &[u8; 16]) -> bool {
    left.iter()
        .zip(right.iter())
        .fold(0u8, |accumulator, (a, b)| accumulator | (a ^ b))
        == 0
}

const fn decode_rejection(error: DecodeError) -> RejectionCode {
    error.rejection_code()
}

#[cfg(test)]
mod tests {
    use super::*;
    use relay_protocol::{
        ADMISSION_CREDENTIAL_BYTES, AdmissionCredential, ConnectionId, FrameType, MAX_HELLO_BYTES,
        RejectionCode, RelayFrame, RouteId,
    };

    const START: u64 = 1_800_000_000;
    const DESKTOP_IP: [u8; 16] = [10; 16];
    const DEVICE_IP: [u8; 16] = [20; 16];

    fn route() -> RouteId {
        RouteId::from_bytes([0x41; 16])
    }

    fn credential(byte: u8) -> AdmissionCredential {
        AdmissionCredential::from_bytes([byte; ADMISSION_CREDENTIAL_BYTES])
    }

    fn connection_id(byte: u8) -> ConnectionId {
        ConnectionId::from_bytes([byte; 16])
    }

    fn new_core() -> RelayCore {
        RelayCore::new(
            RelayLimits::default(),
            vec![RouteVerifier::new(route(), credential(0xd1))],
        )
        .unwrap()
    }

    fn frame(frame_type: FrameType, connection: ConnectionId, payload: &[u8]) -> Vec<u8> {
        RelayFrame::new(frame_type, route(), connection, 0, payload)
            .unwrap()
            .to_vec()
    }

    /// 건강한 전송 계층을 흉내 낸다 — 코어가 낸 전송 지시를 전부 즉시 써 낸 것으로 친다.
    /// 제어 프레임도 큐 예약을 받으므로, 이걸 빼먹으면 예약이 남아 상한 테스트가 어긋난다.
    fn flush_all(core: &mut RelayCore, actions: &[RelayAction]) {
        for action in actions {
            if let RelayAction::Send { connection, .. } = action {
                core.queue_flushed(*connection, 1);
            }
        }
    }

    /// 데스크톱 하나를 입장시키고 그 키를 돌려준다.
    fn admit_desktop(core: &mut RelayCore, now: u64) -> ConnectionKey {
        let key = admit_desktop_unrestored(core, now);
        let actions = core
            .frame_received(
                key,
                &frame(FrameType::ReconnectSync, connection_id(1), &[]),
                now,
            )
            .unwrap();
        assert!(actions.is_empty());
        key
    }

    fn admit_desktop_unrestored(core: &mut RelayCore, now: u64) -> ConnectionKey {
        let key = core.connection_opened(DESKTOP_IP, now).unwrap();
        let actions = core
            .frame_received(
                key,
                &frame(
                    FrameType::DesktopAdmission,
                    connection_id(1),
                    credential(0xd1).as_bytes(),
                ),
                now,
            )
            .unwrap();
        assert!(
            sent_types(&actions).contains(&FrameType::Admitted),
            "{actions:?}"
        );
        flush_all(core, &actions);
        key
    }

    fn publish_ticket(core: &mut RelayCore, desktop: ConnectionKey, handle: u8, now: u64) {
        let actions = core
            .frame_received(
                desktop,
                &frame(
                    FrameType::TicketPublish,
                    connection_id(1),
                    credential(handle).as_bytes(),
                ),
                now,
            )
            .unwrap();
        flush_all(core, &actions);
    }

    fn admit_device(core: &mut RelayCore, handle: u8, now: u64) -> ConnectionKey {
        let key = core.connection_opened(DEVICE_IP, now).unwrap();
        let actions = core
            .frame_received(
                key,
                &frame(
                    FrameType::DeviceAdmission,
                    connection_id(2),
                    credential(handle).as_bytes(),
                ),
                now,
            )
            .unwrap();
        flush_all(core, &actions);
        key
    }

    fn sent_types(actions: &[RelayAction]) -> Vec<FrameType> {
        actions
            .iter()
            .filter_map(|action| match action {
                RelayAction::Send { frame, .. } => {
                    Some(RelayFrame::decode(frame).unwrap().0.frame_type())
                }
                RelayAction::Disconnect { .. } => None,
            })
            .collect()
    }

    fn disconnects(actions: &[RelayAction]) -> Vec<(ConnectionKey, RejectionCode)> {
        actions
            .iter()
            .filter_map(|action| match action {
                RelayAction::Disconnect { connection, code } => Some((*connection, *code)),
                RelayAction::Send { .. } => None,
            })
            .collect()
    }

    fn forwarded_to(actions: &[RelayAction], target: ConnectionKey) -> Vec<Vec<u8>> {
        actions
            .iter()
            .filter_map(|action| match action {
                RelayAction::Send { connection, frame } if *connection == target => {
                    Some(frame.clone())
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn reconnect_pairing_link_without_route_resolves_only_its_one_shot_ticket() {
        let mut core = new_core();
        let desktop = admit_desktop(&mut core, START);
        publish_ticket(&mut core, desktop, 7, START);
        let device = core.connection_opened(DEVICE_IP, START + 1).unwrap();
        let admission = RelayFrame::new(
            FrameType::DeviceAdmission,
            RouteId::from_bytes([0; 16]),
            connection_id(2),
            0,
            &[7; 32],
        )
        .unwrap()
        .to_vec();
        let actions = core.frame_received(device, &admission, START + 1).unwrap();
        assert!(sent_types(&actions).contains(&FrameType::Admitted));
        for action in &actions {
            if let RelayAction::Send { frame, .. } = action {
                assert_eq!(RelayFrame::decode(frame).unwrap().0.route_id(), route());
            }
        }
    }

    #[test]
    fn reconnect_route_free_pairing_rejects_ambiguous_tickets() {
        let second_route = RouteId::from_bytes([0x42; 16]);
        let mut core = RelayCore::new(
            RelayLimits::default(),
            vec![
                RouteVerifier::new(route(), credential(0xd1)),
                RouteVerifier::new(second_route, credential(0xd2)),
            ],
        )
        .unwrap();
        let first = admit_desktop(&mut core, START);
        publish_ticket(&mut core, first, 7, START);
        let second = core.connection_opened([30; 16], START).unwrap();
        for (kind, payload) in [
            (FrameType::DesktopAdmission, credential(0xd2)),
            (FrameType::TicketPublish, credential(7)),
        ] {
            let wire = RelayFrame::new(kind, second_route, connection_id(1), 0, payload.as_bytes())
                .unwrap()
                .to_vec();
            let actions = core.frame_received(second, &wire, START).unwrap();
            flush_all(&mut core, &actions);
        }
        let device = core.connection_opened(DEVICE_IP, START + 1).unwrap();
        let admission = RelayFrame::new(
            FrameType::DeviceAdmission,
            RouteId::from_bytes([0; 16]),
            connection_id(2),
            0,
            &[7; 32],
        )
        .unwrap()
        .to_vec();
        let actions = core.frame_received(device, &admission, START + 1).unwrap();
        assert_eq!(
            disconnects(&actions),
            vec![(device, RejectionCode::TicketUnknown)]
        );
        assert_eq!(
            core.ticket_count(),
            2,
            "중복 route의 티켓을 임의 소비하지 않는다"
        );
    }

    #[test]
    fn reconnect_during_desktop_restoration_is_transient_not_revoked() {
        let mut core = new_core();
        let desktop = admit_desktop_unrestored(&mut core, START);
        let device = core.connection_opened(DEVICE_IP, START + 1).unwrap();
        let actions = core
            .frame_received(
                device,
                &frame(FrameType::ReconnectAdmission, connection_id(2), &[7; 32]),
                START + 1,
            )
            .unwrap();
        assert_eq!(
            disconnects(&actions),
            vec![(device, RejectionCode::RouteBusy)],
            "DB verifier 복원 전 입장은 기기 회수가 아니라 일시적 대기다"
        );
        let verifier: [u8; 32] = Sha256::digest([7; 32]).into();
        let mut published = verifier.to_vec();
        published.extend_from_slice(&(START + 600).to_be_bytes());
        let actions = core
            .frame_received(
                desktop,
                &frame(FrameType::ReconnectPublish, connection_id(1), &published),
                START + 2,
            )
            .unwrap();
        flush_all(&mut core, &actions);
        core.frame_received(
            desktop,
            &frame(FrameType::ReconnectSync, connection_id(1), &[]),
            START + 2,
        )
        .unwrap();
        let device = core.connection_opened(DEVICE_IP, START + 3).unwrap();
        let actions = core
            .frame_received(
                device,
                &frame(FrameType::ReconnectAdmission, connection_id(2), &[7; 32]),
                START + 3,
            )
            .unwrap();
        assert!(sent_types(&actions).contains(&FrameType::Admitted));
        flush_all(&mut core, &actions);
        let actions = core.connection_closed(device, START + 3);
        flush_all(&mut core, &actions);
        let wrong = core.connection_opened(DEVICE_IP, START + 4).unwrap();
        let actions = core
            .frame_received(
                wrong,
                &frame(FrameType::ReconnectAdmission, connection_id(2), &[8; 32]),
                START + 4,
            )
            .unwrap();
        assert_eq!(
            disconnects(&actions),
            vec![(wrong, RejectionCode::CredentialRejected)]
        );
    }

    #[test]
    fn reconnect_grant_is_revoked_and_never_accepted_with_wrong_bytes() {
        let mut core = new_core();
        let desktop = admit_desktop(&mut core, START);
        let verifier: [u8; 32] = Sha256::digest([7; 32]).into();
        let mut payload = verifier.to_vec();
        payload.extend_from_slice(&(START + 600).to_be_bytes());
        let actions = core
            .frame_received(
                desktop,
                &frame(FrameType::ReconnectPublish, connection_id(1), &payload),
                START,
            )
            .unwrap();
        flush_all(&mut core, &actions);
        let wrong = core.connection_opened(DEVICE_IP, START + 1).unwrap();
        let actions = core
            .frame_received(
                wrong,
                &frame(FrameType::ReconnectAdmission, connection_id(2), &[8; 32]),
                START + 1,
            )
            .unwrap();
        assert_eq!(
            disconnects(&actions),
            vec![(wrong, RejectionCode::CredentialRejected)]
        );
        let actions = core
            .frame_received(
                desktop,
                &frame(FrameType::ReconnectRevoke, connection_id(1), &verifier),
                START + 2,
            )
            .unwrap();
        flush_all(&mut core, &actions);
        let revoked = core.connection_opened(DEVICE_IP, START + 3).unwrap();
        let actions = core
            .frame_received(
                revoked,
                &frame(FrameType::ReconnectAdmission, connection_id(2), &[7; 32]),
                START + 3,
            )
            .unwrap();
        assert_eq!(
            disconnects(&actions),
            vec![(revoked, RejectionCode::CredentialRejected)]
        );
    }

    #[test]
    fn reconnect_registry_has_an_independent_fixed_bound() {
        let mut core = new_core();
        let desktop = admit_desktop(&mut core, START);
        for index in 0..=relay_protocol::MAX_RECONNECT_GRANTS {
            let mut payload = vec![index as u8; 32];
            payload.extend_from_slice(&(START + 600).to_be_bytes());
            let actions = core
                .frame_received(
                    desktop,
                    &frame(FrameType::ReconnectPublish, connection_id(1), &payload),
                    START,
                )
                .unwrap();
            if index < relay_protocol::MAX_RECONNECT_GRANTS {
                assert!(sent_types(&actions).contains(&FrameType::ReconnectPublished));
            } else {
                assert!(sent_types(&actions).contains(&FrameType::Rejected));
            }
            flush_all(&mut core, &actions);
        }
        assert_eq!(
            core.routes[&route()].grants.len(),
            relay_protocol::MAX_RECONNECT_GRANTS
        );
        assert_eq!(core.ticket_count(), 0);
        assert_eq!(core.route_count(), 1);
    }

    #[test]
    fn reconnect_grant_survives_disconnect_but_not_expiry() {
        // SHA-256([7;32]), 고정 벡터를 써서 시험 자체에는 새 해시 의존성이 없다.
        let verifier = [
            0x4b, 0xb0, 0x6f, 0x8e, 0x4e, 0x3a, 0x77, 0x15, 0xd2, 0x01, 0xd5, 0x73, 0xd0, 0xaa,
            0x42, 0x37, 0x62, 0xe5, 0x5d, 0xab, 0xd6, 0x1a, 0x2c, 0x02, 0x27, 0x8f, 0xa5, 0x6c,
            0xc6, 0xd2, 0x94, 0xe0,
        ];
        let mut core = new_core();
        let desktop = admit_desktop(&mut core, START);
        let mut published = verifier.to_vec();
        published.extend_from_slice(&(START + 600).to_be_bytes());
        let actions = core
            .frame_received(
                desktop,
                &frame(FrameType::ReconnectPublish, connection_id(1), &published),
                START,
            )
            .unwrap();
        assert!(
            sent_types(&actions).contains(&FrameType::ReconnectPublished),
            "{actions:?}"
        );
        flush_all(&mut core, &actions);
        for at in [START + 1, START + 301] {
            let device = core.connection_opened(DEVICE_IP, at).unwrap();
            let actions = core
                .frame_received(
                    device,
                    &frame(FrameType::ReconnectAdmission, connection_id(2), &[7; 32]),
                    at,
                )
                .unwrap();
            assert!(
                sent_types(&actions).contains(&FrameType::Admitted),
                "{actions:?}"
            );
            flush_all(&mut core, &actions);
            let actions = core.connection_closed(device, at);
            flush_all(&mut core, &actions);
        }
        let device = core.connection_opened(DEVICE_IP, START + 600).unwrap();
        let actions = core
            .frame_received(
                device,
                &frame(FrameType::ReconnectAdmission, connection_id(2), &[7; 32]),
                START + 600,
            )
            .unwrap();
        assert!(!sent_types(&actions).contains(&FrameType::Admitted));
    }

    #[test]
    fn reconnect_publication_cannot_extend_expiry_or_cross_authority() {
        for expires_at in [
            START,
            START + relay_protocol::MAX_RECONNECT_LIFETIME_SECS + 1,
        ] {
            let mut core = new_core();
            let desktop = admit_desktop(&mut core, START);
            let mut payload = vec![7; 32];
            payload.extend_from_slice(&expires_at.to_be_bytes());
            let actions = core
                .frame_received(
                    desktop,
                    &frame(FrameType::ReconnectPublish, connection_id(1), &payload),
                    START,
                )
                .unwrap();
            assert_eq!(
                disconnects(&actions),
                vec![(desktop, RejectionCode::MalformedFrame)]
            );
        }
        let mut core = new_core();
        let desktop = admit_desktop(&mut core, START);
        let verifier: [u8; 32] = Sha256::digest([7; 32]).into();
        for expiry in [START + 100, START + 200] {
            let mut payload = verifier.to_vec();
            payload.extend_from_slice(&expiry.to_be_bytes());
            let actions = core
                .frame_received(
                    desktop,
                    &frame(FrameType::ReconnectPublish, connection_id(1), &payload),
                    START,
                )
                .unwrap();
            flush_all(&mut core, &actions);
        }
        assert_eq!(core.routes[&route()].grants[0].expires_at, START + 100);
        let device = core.connection_opened(DEVICE_IP, START + 1).unwrap();
        let actions = core
            .frame_received(
                device,
                &frame(FrameType::ReconnectAdmission, connection_id(2), &[7; 32]),
                START + 1,
            )
            .unwrap();
        flush_all(&mut core, &actions);
        // 입장한 기기도 grant 회수 권한은 없다.
        let actions = core
            .frame_received(
                device,
                &frame(FrameType::ReconnectRevoke, connection_id(2), &verifier),
                START + 2,
            )
            .unwrap();
        assert_eq!(
            disconnects(&actions),
            vec![(device, RejectionCode::MalformedFrame)]
        );
        let wrong_route = RelayFrame::new(
            FrameType::ReconnectRevoke,
            RouteId::from_bytes([9; 16]),
            connection_id(1),
            0,
            &verifier,
        )
        .unwrap()
        .to_vec();
        let actions = core
            .frame_received(desktop, &wrong_route, START + 3)
            .unwrap();
        assert_eq!(
            disconnects(&actions),
            vec![(desktop, RejectionCode::RouteUnknown)]
        );
    }

    #[test]
    fn an_unadmitted_connection_may_only_present_admission() {
        for forbidden in [
            FrameType::Ciphertext,
            FrameType::Hello,
            FrameType::Heartbeat,
        ] {
            let mut core = new_core();
            let key = core.connection_opened(DESKTOP_IP, START).unwrap();
            let payload = if forbidden == FrameType::Heartbeat {
                Vec::new()
            } else {
                b"x".to_vec()
            };
            let actions = core
                .frame_received(key, &frame(forbidden, connection_id(1), &payload), START)
                .unwrap();
            assert_eq!(
                disconnects(&actions),
                vec![(key, RejectionCode::MalformedFrame)],
                "{forbidden:?} must not be accepted before admission"
            );
            assert_eq!(core.route_count(), 0);
        }
    }

    #[test]
    fn desktop_admission_requires_the_exact_credential_for_that_route() {
        let mut core = new_core();
        let key = core.connection_opened(DESKTOP_IP, START).unwrap();
        let actions = core
            .frame_received(
                key,
                &frame(
                    FrameType::DesktopAdmission,
                    connection_id(1),
                    credential(0xff).as_bytes(),
                ),
                START,
            )
            .unwrap();
        assert_eq!(
            disconnects(&actions),
            vec![(key, RejectionCode::CredentialRejected)]
        );
        assert_eq!(core.route_count(), 0, "거절은 라우트를 할당하지 않는다");
    }

    #[test]
    fn a_route_admits_one_desktop_at_a_time() {
        let mut core = new_core();
        let first = admit_desktop(&mut core, START);
        let second = core.connection_opened(DESKTOP_IP, START).unwrap();
        let actions = core
            .frame_received(
                second,
                &frame(
                    FrameType::DesktopAdmission,
                    connection_id(3),
                    credential(0xd1).as_bytes(),
                ),
                START,
            )
            .unwrap();
        assert_eq!(
            disconnects(&actions),
            vec![(second, RejectionCode::RouteBusy)]
        );
        assert_eq!(core.route_count(), 1);

        // 원래 소유자가 떠나면 라우트가 해제되고 다음 데스크톱이 들어올 수 있다.
        core.connection_closed(first, START + 1);
        assert_eq!(core.route_count(), 0);
        let third = core.connection_opened(DESKTOP_IP, START + 2).unwrap();
        let actions = core
            .frame_received(
                third,
                &frame(
                    FrameType::DesktopAdmission,
                    connection_id(4),
                    credential(0xd1).as_bytes(),
                ),
                START + 2,
            )
            .unwrap();
        assert!(sent_types(&actions).contains(&FrameType::Admitted));
    }

    /// 연결 id는 기기가 정하고 서버는 **양쪽에 같은 값**을 돌려준다. 기기에게 가는
    /// `Admitted`에 그 값이 없으면 기기는 자기 세션 id를 확인할 길이 없다 —
    /// 서버는 `PeerJoined`를 Mac에게만 보내기 때문이다(2026-09-03 실측한 페어링 실패).
    #[test]
    fn the_device_chooses_the_connection_id_and_both_sides_are_told_the_same_one() {
        let mut core = new_core();
        let desktop = admit_desktop(&mut core, START);
        publish_ticket(&mut core, desktop, 0xa1, START);
        let device = core.connection_opened(DEVICE_IP, START).unwrap();
        let chosen = connection_id(0x5c);
        let actions = core
            .frame_received(
                device,
                &frame(FrameType::DeviceAdmission, chosen, &[0xa1; 32]),
                START,
            )
            .unwrap();
        let sent: Vec<(ConnectionKey, FrameType, ConnectionId)> = actions
            .iter()
            .filter_map(|action| match action {
                RelayAction::Send { connection, frame } => {
                    let (decoded, _) = RelayFrame::decode(frame).unwrap();
                    Some((*connection, decoded.frame_type(), decoded.connection_id()))
                }
                RelayAction::Disconnect { .. } => None,
            })
            .collect();
        assert_eq!(
            sent,
            vec![
                (device, FrameType::Admitted, chosen),
                (desktop, FrameType::PeerJoined, chosen),
            ],
            "기기에게 Admitted로, Mac에게 PeerJoined로 **같은** 연결 id가 가야 한다"
        );
    }

    /// 전0은 "연결 없음"이라 세션 id가 될 수 없다.
    #[test]
    fn an_all_zero_connection_id_is_refused_at_device_admission() {
        let mut core = new_core();
        let desktop = admit_desktop(&mut core, START);
        publish_ticket(&mut core, desktop, 0xa1, START);
        let device = core.connection_opened(DEVICE_IP, START).unwrap();
        let actions = core
            .frame_received(
                device,
                &frame(FrameType::DeviceAdmission, connection_id(0), &[0xa1; 32]),
                START,
            )
            .unwrap();
        assert_eq!(
            disconnects(&actions),
            vec![(device, RejectionCode::MalformedFrame)]
        );
    }

    #[test]
    fn a_device_ticket_is_single_use_and_expires() {
        let mut core = new_core();
        let desktop = admit_desktop(&mut core, START);
        publish_ticket(&mut core, desktop, 0xa1, START);

        let device = admit_device(&mut core, 0xa1, START + 1);
        assert_eq!(core.route_device_count(), 1);

        // 같은 핸들 재사용은 소비됨으로 거절된다.
        let replay = core.connection_opened(DEVICE_IP, START + 2).unwrap();
        let actions = core
            .frame_received(
                replay,
                &frame(
                    FrameType::DeviceAdmission,
                    connection_id(9),
                    credential(0xa1).as_bytes(),
                ),
                START + 2,
            )
            .unwrap();
        assert_eq!(
            disconnects(&actions),
            vec![(replay, RejectionCode::TicketUnknown)]
        );

        // 만료된 핸들도 마찬가지다.
        core.connection_closed(device, START + 3);
        publish_ticket(&mut core, desktop, 0xa2, START + 3);
        let expired_at = START + 3 + RelayLimits::default().ticket_ttl_secs;
        core.tick(expired_at);
        let late = core.connection_opened(DEVICE_IP, expired_at).unwrap();
        let actions = core
            .frame_received(
                late,
                &frame(
                    FrameType::DeviceAdmission,
                    connection_id(10),
                    credential(0xa2).as_bytes(),
                ),
                expired_at,
            )
            .unwrap();
        assert_eq!(
            disconnects(&actions),
            vec![(late, RejectionCode::TicketUnknown)]
        );
    }

    #[test]
    fn a_revoked_ticket_stops_admitting_immediately() {
        let mut core = new_core();
        let desktop = admit_desktop(&mut core, START);
        publish_ticket(&mut core, desktop, 0xb1, START);
        core.frame_received(
            desktop,
            &frame(
                FrameType::TicketRevoke,
                connection_id(1),
                credential(0xb1).as_bytes(),
            ),
            START,
        )
        .unwrap();

        let device = core.connection_opened(DEVICE_IP, START).unwrap();
        let actions = core
            .frame_received(
                device,
                &frame(
                    FrameType::DeviceAdmission,
                    connection_id(2),
                    credential(0xb1).as_bytes(),
                ),
                START,
            )
            .unwrap();
        assert_eq!(
            disconnects(&actions),
            vec![(device, RejectionCode::TicketUnknown)]
        );
    }

    #[test]
    fn only_the_desktop_may_publish_or_revoke_tickets() {
        let mut core = new_core();
        let desktop = admit_desktop(&mut core, START);
        publish_ticket(&mut core, desktop, 0xc1, START);
        let device = admit_device(&mut core, 0xc1, START);

        let actions = core
            .frame_received(
                device,
                &frame(
                    FrameType::TicketPublish,
                    connection_id(2),
                    credential(0xc2).as_bytes(),
                ),
                START,
            )
            .unwrap();
        assert_eq!(
            disconnects(&actions),
            vec![(device, RejectionCode::MalformedFrame)]
        );
    }

    #[test]
    fn data_frames_are_forwarded_byte_for_byte_to_the_peer_only() {
        let mut core = new_core();
        let desktop = admit_desktop(&mut core, START);
        publish_ticket(&mut core, desktop, 0xe1, START);
        let device = admit_device(&mut core, 0xe1, START);

        let hello = frame(FrameType::Hello, connection_id(2), &[0x9a; MAX_HELLO_BYTES]);
        let actions = core.frame_received(desktop, &hello, START).unwrap();
        assert_eq!(forwarded_to(&actions, device), vec![hello.clone()]);
        assert!(forwarded_to(&actions, desktop).is_empty(), "에코는 없다");

        let ciphertext = frame(FrameType::Ciphertext, connection_id(2), b"opaque-record");
        let actions = core.frame_received(device, &ciphertext, START).unwrap();
        assert_eq!(forwarded_to(&actions, desktop), vec![ciphertext]);
    }

    #[test]
    fn a_frame_for_another_route_or_connection_disconnects_the_sender() {
        let mut core = new_core();
        let desktop = admit_desktop(&mut core, START);
        publish_ticket(&mut core, desktop, 0xe2, START);
        let device = admit_device(&mut core, 0xe2, START);

        let other_route = RelayFrame::new(
            FrameType::Ciphertext,
            RouteId::from_bytes([0x99; 16]),
            connection_id(2),
            0,
            b"x",
        )
        .unwrap()
        .to_vec();
        let actions = core.frame_received(device, &other_route, START).unwrap();
        assert_eq!(
            disconnects(&actions),
            vec![(device, RejectionCode::RouteUnknown)]
        );

        let mut core = new_core();
        let desktop = admit_desktop(&mut core, START);
        publish_ticket(&mut core, desktop, 0xe3, START);
        let device = admit_device(&mut core, 0xe3, START);
        let wrong_connection = frame(FrameType::Ciphertext, connection_id(0x77), b"x");
        let actions = core
            .frame_received(device, &wrong_connection, START)
            .unwrap();
        assert_eq!(
            disconnects(&actions),
            vec![(device, RejectionCode::MalformedFrame)],
            "연결 id를 바꿔 다른 세션인 척할 수 없다"
        );
    }

    #[test]
    fn a_second_device_cannot_join_an_occupied_route() {
        let mut core = new_core();
        let desktop = admit_desktop(&mut core, START);
        publish_ticket(&mut core, desktop, 0xf1, START);
        publish_ticket(&mut core, desktop, 0xf2, START);
        let _first = admit_device(&mut core, 0xf1, START);

        let second = core.connection_opened(DEVICE_IP, START).unwrap();
        let actions = core
            .frame_received(
                second,
                &frame(
                    FrameType::DeviceAdmission,
                    connection_id(5),
                    credential(0xf2).as_bytes(),
                ),
                START,
            )
            .unwrap();
        assert_eq!(
            disconnects(&actions),
            vec![(second, RejectionCode::RouteBusy)]
        );
    }

    #[test]
    fn ticket_guessing_is_rate_limited_per_ip_without_touching_the_route() {
        let limits = RelayLimits::default();
        let mut core = new_core();
        let desktop = admit_desktop(&mut core, START);
        publish_ticket(&mut core, desktop, 0x01, START);

        let mut rejected = 0;
        let mut rate_limited = 0;
        for attempt in 0..limits.admission_attempts_per_window + 4 {
            let guess = core.connection_opened(DEVICE_IP, START).unwrap();
            let actions = core
                .frame_received(
                    guess,
                    &frame(
                        FrameType::DeviceAdmission,
                        connection_id(6),
                        credential(0x80 | (attempt as u8 & 0x0f)).as_bytes(),
                    ),
                    START,
                )
                .unwrap();
            match disconnects(&actions).first().map(|(_, code)| *code) {
                Some(RejectionCode::TicketUnknown) => rejected += 1,
                Some(RejectionCode::RateLimited) => rate_limited += 1,
                other => panic!("unexpected {other:?}"),
            }
            core.connection_closed(guess, START);
        }
        assert_eq!(rejected, limits.admission_attempts_per_window as usize);
        assert_eq!(rate_limited, 4);

        // 창이 지나면 다시 시도할 수 있다. 그동안 데스크톱은 생존 신호로 살아 있어야
        // 한다 — 유휴 시한이 속도 제한 창과 같은 60초라 신호가 없으면 라우트가 먼저 닫힌다.
        let later = START + limits.admission_window_secs;
        core.frame_received(
            desktop,
            &frame(FrameType::Heartbeat, connection_id(1), b""),
            later - 1,
        )
        .unwrap();
        assert!(disconnects(&core.tick(later)).is_empty());
        let retry = core.connection_opened(DEVICE_IP, later).unwrap();
        let actions = core
            .frame_received(
                retry,
                &frame(
                    FrameType::DeviceAdmission,
                    connection_id(7),
                    credential(0x01).as_bytes(),
                ),
                later,
            )
            .unwrap();
        assert!(sent_types(&actions).contains(&FrameType::Admitted));
    }

    #[test]
    fn a_slow_consumer_is_disconnected_instead_of_buffering_without_limit() {
        let limits = RelayLimits::default();
        let mut core = new_core();
        let desktop = admit_desktop(&mut core, START);
        publish_ticket(&mut core, desktop, 0x21, START);
        let device = admit_device(&mut core, 0x21, START);

        // 기기가 아무것도 소비하지 않는 동안 데스크톱이 계속 보낸다.
        let mut disconnected = None;
        for index in 0..limits.max_queue_frames + 4 {
            let payload = vec![0x33; 1024];
            let actions = core
                .frame_received(
                    desktop,
                    &frame(FrameType::Ciphertext, connection_id(2), &payload),
                    START,
                )
                .unwrap();
            if let Some((connection, code)) = disconnects(&actions).first() {
                disconnected = Some((*connection, *code, index));
                break;
            }
        }
        let (connection, code, index) = disconnected.expect("느린 소비자는 결국 끊긴다");
        assert_eq!(
            connection, device,
            "끊기는 쪽은 보낸 쪽이 아니라 느린 쪽이다"
        );
        assert_eq!(code, RejectionCode::QueueOverflow);
        assert!(index <= limits.max_queue_frames + 1);
    }

    #[test]
    fn a_single_huge_backlog_is_bounded_by_bytes_not_only_by_frames() {
        let limits = RelayLimits::default();
        let mut core = new_core();
        let desktop = admit_desktop(&mut core, START);
        publish_ticket(&mut core, desktop, 0x22, START);
        let device = admit_device(&mut core, 0x22, START);

        let big = vec![0x44; 512 * 1024];
        let mut sent_bytes = 0usize;
        loop {
            let actions = core
                .frame_received(
                    desktop,
                    &frame(FrameType::Ciphertext, connection_id(2), &big),
                    START,
                )
                .unwrap();
            if let Some((connection, code)) = disconnects(&actions).first() {
                assert_eq!(*connection, device);
                assert_eq!(*code, RejectionCode::QueueOverflow);
                break;
            }
            sent_bytes += big.len();
            assert!(
                sent_bytes <= limits.max_queue_bytes + big.len(),
                "바이트 상한 없이 프레임 수만 세면 메모리가 터진다"
            );
        }
    }

    #[test]
    fn an_idle_connection_times_out_and_heartbeats_keep_it_alive() {
        let limits = RelayLimits::default();
        let mut core = new_core();
        let desktop = admit_desktop(&mut core, START);
        publish_ticket(&mut core, desktop, 0x31, START);
        let device = admit_device(&mut core, 0x31, START);

        let almost = START + limits.idle_timeout_secs - 1;
        assert!(disconnects(&core.tick(almost)).is_empty());

        // 데스크톱만 생존 신호를 보낸다.
        core.frame_received(
            desktop,
            &frame(FrameType::Heartbeat, connection_id(2), b""),
            almost,
        )
        .unwrap();

        let expired = START + limits.idle_timeout_secs;
        let actions = core.tick(expired);
        assert_eq!(
            disconnects(&actions),
            vec![(device, RejectionCode::IdleTimeout)],
            "조용했던 기기만 끊긴다"
        );
    }

    #[test]
    fn an_unadmitted_connection_times_out_on_the_shorter_handshake_deadline() {
        let limits = RelayLimits::default();
        let mut core = new_core();
        let key = core.connection_opened(DESKTOP_IP, START).unwrap();
        assert!(disconnects(&core.tick(START + limits.handshake_timeout_secs - 1)).is_empty());
        assert_eq!(
            disconnects(&core.tick(START + limits.handshake_timeout_secs)),
            vec![(key, RejectionCode::IdleTimeout)]
        );
        assert!(
            limits.handshake_timeout_secs < limits.idle_timeout_secs,
            "입장 전 연결은 더 짧은 시한을 받는다"
        );
    }

    /// 만료는 정리 주기가 아니라 판정 시점의 시계로 본다. `tick`을 한 번도 돌리지
    /// 않아도 만료된 티켓은 입장시키지 못한다.
    #[test]
    fn an_expired_ticket_is_refused_even_when_no_cleanup_tick_has_run() {
        let limits = RelayLimits::default();
        let mut core = new_core();
        let desktop = admit_desktop(&mut core, START);
        publish_ticket(&mut core, desktop, 0xa9, START);

        let expired_at = START + limits.ticket_ttl_secs;
        // 데스크톱은 살아 있어야 하므로 생존 신호만 보낸다 — 정리는 돌리지 않는다.
        core.frame_received(
            desktop,
            &frame(FrameType::Heartbeat, connection_id(1), b""),
            expired_at,
        )
        .unwrap();

        let late = core.connection_opened(DEVICE_IP, expired_at).unwrap();
        let actions = core
            .frame_received(
                late,
                &frame(
                    FrameType::DeviceAdmission,
                    connection_id(2),
                    credential(0xa9).as_bytes(),
                ),
                expired_at,
            )
            .unwrap();
        assert_eq!(
            disconnects(&actions),
            vec![(late, RejectionCode::TicketUnknown)]
        );
        assert_eq!(core.route_device_count(), 0);
    }

    /// 라우트가 차 있어 거절당한 티켓도 소비된다. 소비하지 않으면 같은 핸들을 상대가
    /// 떠난 뒤 다시 쓸 수 있어 재생 공격이 열린다.
    #[test]
    fn a_ticket_refused_because_the_route_is_busy_is_still_consumed() {
        let mut core = new_core();
        let desktop = admit_desktop(&mut core, START);
        publish_ticket(&mut core, desktop, 0xf1, START);
        publish_ticket(&mut core, desktop, 0xf2, START);
        let first = admit_device(&mut core, 0xf1, START);
        assert_eq!(core.ticket_count(), 1);

        let blocked = core.connection_opened(DEVICE_IP, START).unwrap();
        let actions = core
            .frame_received(
                blocked,
                &frame(
                    FrameType::DeviceAdmission,
                    connection_id(5),
                    credential(0xf2).as_bytes(),
                ),
                START,
            )
            .unwrap();
        assert_eq!(
            disconnects(&actions),
            vec![(blocked, RejectionCode::RouteBusy)]
        );
        assert_eq!(core.ticket_count(), 0, "거절당한 티켓도 소비된다");

        // 첫 기기가 떠나 라우트가 비어도 그 핸들은 되살아나지 않는다.
        core.connection_closed(first, START + 1);
        let retry = core.connection_opened(DEVICE_IP, START + 2).unwrap();
        let actions = core
            .frame_received(
                retry,
                &frame(
                    FrameType::DeviceAdmission,
                    connection_id(6),
                    credential(0xf2).as_bytes(),
                ),
                START + 2,
            )
            .unwrap();
        assert_eq!(
            disconnects(&actions),
            vec![(retry, RejectionCode::TicketUnknown)]
        );
    }

    /// 출발지 IP를 바꿔 가며 실패시키는 것만으로 속도 제한 표가 자라면 안 된다.
    #[test]
    fn the_rate_limit_table_is_bounded_against_rotating_source_addresses() {
        let limits = RelayLimits::default();
        let mut core = new_core();

        // 창 표 상한을 넘길 만큼 서로 다른 IP에서 실패시킨다.
        for index in 0..limits.max_admission_windows + 64 {
            let mut ip = [0u8; 16];
            ip[..8].copy_from_slice(&(index as u64).to_be_bytes());
            let Ok(key) = core.connection_opened(ip, START) else {
                break;
            };
            let _ = core.frame_received(
                key,
                &frame(
                    FrameType::DeviceAdmission,
                    connection_id(3),
                    credential(0x5c).as_bytes(),
                ),
                START,
            );
            core.connection_closed(key, START);
        }
        assert!(
            core.admission_window_count() <= limits.max_admission_windows,
            "창 표가 상한을 넘었다: {}",
            core.admission_window_count()
        );

        // 창이 지나면 표는 다시 비워진다.
        core.tick(START + limits.admission_window_secs);
        assert_eq!(core.admission_window_count(), 0);
    }

    /// 연결당 상한만으로는 부족하다 — 서버 전체 예약에도 상한이 있어야 한다.
    #[test]
    fn the_total_queue_reservation_is_bounded_across_all_connections() {
        let limits = RelayLimits::default();
        assert!(
            limits.max_total_queue_bytes
                < limits
                    .max_connections
                    .saturating_mul(limits.max_queue_bytes),
            "전체 상한이 연결당 상한의 단순 합보다 작아야 의미가 있다"
        );

        let mut core = new_core();
        let desktop = admit_desktop(&mut core, START);
        publish_ticket(&mut core, desktop, 0x23, START);
        let _device = admit_device(&mut core, 0x23, START);

        let big = vec![0x55; 256 * 1024];
        loop {
            let actions = core
                .frame_received(
                    desktop,
                    &frame(FrameType::Ciphertext, connection_id(2), &big),
                    START,
                )
                .unwrap();
            if !disconnects(&actions).is_empty() {
                // 끊긴 기기 대신 데스크톱에 간 PeerLeft는 건강한 전송이 바로 써 낸다.
                flush_all(&mut core, &actions);
                break;
            }
            assert!(core.queued_bytes() <= limits.max_total_queue_bytes);
        }

        // 판정만으로는 예산이 풀리지 않는다 — 그 바이트는 아직 전송 채널에 살아 있다.
        assert!(
            core.draining_bytes() > 0,
            "끊긴 연결의 바이트가 즉시 사라진 것처럼 계산됐다"
        );
        assert_eq!(core.queued_bytes(), core.draining_bytes());

        // 전송 계층이 실제로 버렸다고 확인해 줄 때에야 풀린다.
        core.connection_closed(_device, START + 1);
        assert_eq!(core.draining_bytes(), 0);
        assert_eq!(core.queued_bytes(), 0, "정리는 예약을 남기지 않는다");
    }

    /// 끊긴 연결의 바이트를 즉시 풀어 버리면, 그 바이트가 아직 전송 채널에 있는 동안
    /// 새 연결이 같은 예산을 다시 채워 전체 상한을 넘길 수 있다.
    #[test]
    fn a_dropped_peers_bytes_keep_counting_until_the_transport_confirms() {
        let limits = RelayLimits::default();
        let mut core = new_core();
        let desktop = admit_desktop(&mut core, START);
        publish_ticket(&mut core, desktop, 0x24, START);
        let device = admit_device(&mut core, 0x24, START);

        let big = vec![0x66; 512 * 1024];
        let mut reserved = 0usize;
        loop {
            let actions = core
                .frame_received(
                    desktop,
                    &frame(FrameType::Ciphertext, connection_id(2), &big),
                    START,
                )
                .unwrap();
            if !disconnects(&actions).is_empty() {
                flush_all(&mut core, &actions);
                break;
            }
            reserved = core.queued_bytes();
        }
        assert!(reserved > 0);
        assert_eq!(
            core.draining_bytes(),
            reserved,
            "예약분이 그대로 정리 대기로 옮겨져야 한다"
        );
        assert!(core.queued_bytes() <= limits.max_total_queue_bytes);

        // 확인이 오기 전에는 그 예산을 다시 쓸 수 없다.
        core.connection_closed(device, START + 1);
        assert_eq!(core.queued_bytes(), 0);
    }

    /// 제어 프레임도 큐 예약을 받는다. 예약 없이 나간 프레임을 전송 계층이 세면 데이터
    /// 프레임의 예약이 풀려 상한이 한 프레임씩 샌다.
    #[test]
    fn control_frames_reserve_queue_bytes_like_data_frames() {
        let mut core = new_core();
        let key = core.connection_opened(DESKTOP_IP, START).unwrap();
        assert_eq!(core.queued_bytes(), 0);
        let actions = core
            .frame_received(
                key,
                &frame(
                    FrameType::DesktopAdmission,
                    connection_id(1),
                    credential(0xd1).as_bytes(),
                ),
                START,
            )
            .unwrap();
        assert_eq!(sent_types(&actions), vec![FrameType::Admitted]);
        assert!(
            core.queued_bytes() > 0,
            "Admitted 제어 프레임이 데스크톱 큐에 예약돼야 한다"
        );
        // 전송 계층이 실제로 써 낸 만큼만 풀린다.
        core.queue_flushed(key, 1);
        assert_eq!(core.queued_bytes(), 0);
        // 더 풀 것이 없으면 아무 일도 일어나지 않는다.
        core.queue_flushed(key, 1);
        assert_eq!(core.queued_bytes(), 0);
    }

    /// 같은 핸들의 재등록은 멱등 재시도다. 라우트와 기기를 끊으면 안 된다.
    #[test]
    fn republishing_a_pending_ticket_is_not_fatal() {
        let mut core = new_core();
        let desktop = admit_desktop(&mut core, START);
        publish_ticket(&mut core, desktop, 0x82, START);
        let actions = core
            .frame_received(
                desktop,
                &frame(
                    FrameType::TicketPublish,
                    connection_id(1),
                    credential(0x82).as_bytes(),
                ),
                START,
            )
            .unwrap();
        assert!(disconnects(&actions).is_empty(), "{actions:?}");
        assert_eq!(sent_types(&actions), vec![FrameType::Rejected]);
        assert_eq!(core.route_count(), 1);
        assert_eq!(core.ticket_count(), 1);
    }

    /// 데스크톱과 기기가 같은 바퀴에 함께 만료되면, 데스크톱 정리가 기기를 이미
    /// 연쇄로 걷어낸다. 그 기기에 절단 지시를 두 번 내면 안 된다.
    #[test]
    fn a_cascading_timeout_disconnects_each_connection_exactly_once() {
        let limits = RelayLimits::default();
        let mut core = new_core();
        let desktop = admit_desktop(&mut core, START);
        publish_ticket(&mut core, desktop, 0x91, START);
        let device = admit_device(&mut core, 0x91, START);

        let expired = START + limits.idle_timeout_secs;
        let actions = core.tick(expired);
        let closed = disconnects(&actions);
        let mut keys: Vec<ConnectionKey> = closed.iter().map(|(key, _)| *key).collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(
            keys.len(),
            closed.len(),
            "같은 연결에 두 번 절단 지시가 나갔다: {closed:?}"
        );
        assert_eq!(keys.len(), 2);
        assert!(keys.contains(&desktop) && keys.contains(&device));
        assert_eq!(core.connection_count(), 0);
        assert_eq!(core.route_count(), 0);
        assert_eq!(core.queued_bytes(), 0);
    }

    #[test]
    fn capacity_is_bounded_per_ip_and_in_total() {
        let limits = RelayLimits::default();
        let mut core = new_core();
        let mut keys = Vec::new();
        for _ in 0..limits.max_connections_per_ip {
            keys.push(core.connection_opened(DEVICE_IP, START).unwrap());
        }
        assert!(matches!(
            core.connection_opened(DEVICE_IP, START),
            Err(AdmissionRefusal::PerIpCapacity)
        ));
        // 다른 IP는 영향받지 않는다.
        assert!(core.connection_opened(DESKTOP_IP, START).is_ok());

        core.connection_closed(keys.pop().unwrap(), START);
        assert!(core.connection_opened(DEVICE_IP, START).is_ok());
    }

    #[test]
    fn closing_a_peer_notifies_the_other_side_and_releases_every_resource() {
        let mut core = new_core();
        let desktop = admit_desktop(&mut core, START);
        publish_ticket(&mut core, desktop, 0x51, START);
        let device = admit_device(&mut core, 0x51, START);

        let actions = core.connection_closed(device, START + 1);
        assert_eq!(sent_types(&actions), vec![FrameType::PeerLeft]);
        assert_eq!(core.route_device_count(), 0);
        assert_eq!(core.connection_count(), 1);

        let actions = core.connection_closed(desktop, START + 2);
        assert!(sent_types(&actions).is_empty());
        assert_eq!(core.connection_count(), 0);
        assert_eq!(core.route_count(), 0);
        assert_eq!(core.ticket_count(), 0, "라우트가 사라지면 티켓도 사라진다");
        assert_eq!(core.queued_bytes(), 0);
    }

    #[test]
    fn shutdown_closes_every_connection_once_and_drains_state() {
        let mut core = new_core();
        let desktop = admit_desktop(&mut core, START);
        publish_ticket(&mut core, desktop, 0x61, START);
        let device = admit_device(&mut core, 0x61, START);

        let actions = core.shutdown(START + 1);
        let closed = disconnects(&actions);
        assert_eq!(closed.len(), 2);
        assert!(
            closed
                .iter()
                .all(|(_, code)| *code == RejectionCode::ShuttingDown)
        );
        assert!(closed.iter().any(|(key, _)| *key == desktop));
        assert!(closed.iter().any(|(key, _)| *key == device));
        assert_eq!(core.connection_count(), 0);
        assert!(
            core.shutdown(START + 2).is_empty(),
            "종료는 두 번 통지하지 않는다"
        );
        assert!(matches!(
            core.connection_opened(DEVICE_IP, START + 3),
            Err(AdmissionRefusal::ShuttingDown)
        ));
    }

    /// 전체 시나리오를 돌린 뒤 Relay 자신의 상태·통계·발신 제어 프레임 어디에도
    /// 애플리케이션 바이트가 남지 않는지 훑는다.
    #[test]
    fn a_full_scenario_leaves_no_application_bytes_in_relay_state_or_output() {
        const MARKER: &[u8] = b"TERMINAL_PLAINTEXT_MARKER";
        let mut core = new_core();
        let desktop = admit_desktop(&mut core, START);
        publish_ticket(&mut core, desktop, 0x71, START);
        let device = admit_device(&mut core, 0x71, START);

        let mut control_output = Vec::new();
        for (sender, payload) in [
            (desktop, MARKER.to_vec()),
            (device, MARKER.to_vec()),
            (desktop, [MARKER, b"-2"].concat()),
        ] {
            let actions = core
                .frame_received(
                    sender,
                    &frame(FrameType::Ciphertext, connection_id(2), &payload),
                    START,
                )
                .unwrap();
            let peer = if sender == desktop { device } else { desktop };
            // 전달된 것은 상대에게 가는 그 프레임 하나뿐이다.
            assert_eq!(forwarded_to(&actions, peer).len(), 1);
            core.queue_flushed(peer, 1);
            for action in &actions {
                if let RelayAction::Send { connection, frame } = action
                    && *connection != peer
                {
                    control_output.extend_from_slice(frame);
                }
            }
        }
        control_output.extend_from_slice(format!("{core:?}").as_bytes());
        control_output.extend_from_slice(format!("{:?}", core.stats()).as_bytes());
        for action in core.shutdown(START + 1) {
            if let RelayAction::Send { frame, .. } = action {
                control_output.extend_from_slice(&frame);
            }
        }

        assert!(
            !control_output
                .windows(MARKER.len())
                .any(|window| window == MARKER),
            "Relay 상태·통계·제어 출력에 애플리케이션 바이트가 새면 안 된다"
        );

        let production = production_source();
        for forbidden in ["payload()", "{payload", "payload =", "String::from_utf8"] {
            assert!(
                !production.contains(&format!("tracing::info!({forbidden}")),
                "{forbidden}"
            );
        }
    }

    /// 서버가 복호화할 수 있는 것은 아무것도 없어야 한다 — 그러려면 복호화 코드가
    /// 링크되지 않아야 한다.
    #[test]
    fn the_server_links_no_secret_crypto_storage_or_ui_dependency() {
        let manifest = include_str!("../Cargo.toml");
        // SHA-256은 입장 grant의 단방향 검증에만 쓴다. ECDH/HKDF/AEAD는 계속 금지한다.
        assert!(manifest.contains("sha2 = { workspace = true }"));
        for forbidden in [
            "secret",
            "web-remote",
            "storage",
            "runtime",
            "terminal",
            "egui",
            "eframe",
            "p256",
            "aes-gcm",
            "hkdf",
            "rusqlite",
            "keyring",
        ] {
            assert!(
                !manifest.contains(&format!("\n{forbidden} =")),
                "relay-server must not depend on {forbidden}"
            );
        }

        let production = production_source();
        for forbidden in ["decrypt", "SecretString", "PairingSecret", "private"] {
            assert!(!production.contains(forbidden), "{forbidden}");
        }
    }

    fn production_source() -> &'static str {
        include_str!("core.rs")
            .split("\n#[cfg(test)]\nmod tests {")
            .next()
            .unwrap()
    }
}
