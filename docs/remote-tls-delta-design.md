# Remote TLS + Delta Viewport 스트리밍 설계

> 상태: **설계 전용(DESIGN-ONLY)**. 구현/의존성 추가 없음. 이 문서는 향후 구현(PR-19+)의 지침이다.
> 근거 코드: `crates/runtime/src/remote.rs`(v1 remote), `client.rs`(slot 이벤트 모델),
> `command.rs`/`event.rs`(와이어 프로토콜), `crates/terminal/src/{viewport_snapshot,alacritty_backend,change_set}.rs`,
> `crates/session/src/session.rs`(dirty/pump), `crates/runtime/src/in_process.rs`(worker/emit).
> 근거 설계문서: `ai_agent_workspace_final_architecture_v2_5_FINAL.md` §1.4(keyring), §1.5(transport/authorization),
> §8.1~8.2(snapshot/delta), §14.3·§14.6(resource policy).

---

## 0. 현재 상태 요약 (설계의 출발점)

`remote.rs` v1이 이미 제공하는 것:

- **프레이밍**: `[u32 LE len][postcard payload]`, 양측 16 MiB 상한(`MAX_FRAME_BYTES`). 길이 0 프레임 = heartbeat.
- **인증**: 실행마다 생성하는 토큰(uuid v4 ×2 ≈ 244bit)을 **첫 프레임**으로 제시 → 상수시간 비교(`token_matches`) → 성공 ACK(`b"ok"`), 5s(`AUTH_TIMEOUT`) 침묵 시 종료.
- **바인드**: `serve()`가 `127.0.0.1`에만 bind(주소를 인자로 받지 않아 loopback을 **형태로** 강제). attach도 `addr.ip().is_loopback()` 아니면 거부.
- **동시성**: 접속당 스레드. `serve_connection`이 **reader 스레드**(명령 프레임 수신 → `validate_command` → worker)와 **pump 스레드**(`receiver.drain()` → 이벤트 프레임 송신 + heartbeat)를 붙인다. 세 갈래(reader/pump/shutdown)가 모두 `TcpStream::try_clone()`으로 같은 fd를 공유한다.
- **이벤트 모델**(client.rs / in_process.rs): 상태 이벤트는 mpsc 채널(unbounded, 수명당 상수 개수), **Viewport는 세션별 최신본 slot**(`HashMap<SessionId, RuntimeEvent>`, latest-wins 코얼레싱). `RuntimeEventReceiver::drain()`이 slot을 먼저 take한 뒤 채널을 비워 happens-before(“Viewport가 있으면 그 세션 Spawned가 같은 drain에”)를 지킨다.
- **Viewport 페이로드**: `RuntimeEvent::Viewport { session, snapshot: Arc<TerminalViewportSnapshot>, bracketed_paste }`. 스냅샷은 **매 push마다 전체 grid**(`visible_cells: Arc<[TerminalCell]>`, row-major `cols*rows`개)를 싣는다.
- **손상(damage) 추적은 이미 존재**: `AlacrittyBackend::feed()`가 `TermDamage::{Full,Partial}` → `TerminalChangeSet.dirty_rows: Vec<u16>`를 계산한다. 하지만 이 per-row 정보는 **소비되지 않고 버려진다** — 세션은 `dirty: bool`(코얼레싱된 불리언)만 유지하고, `TerminalViewportSnapshot.dirty_ranges`는 **항상 빈 Vec**(`viewport_snapshot()`이 명시적으로 `Vec::new()`)이다.

이 문서는 위 골격 위에 (1) 비-loopback 보안 attach(TLS), (2) delta viewport 스트리밍을 **협상 가능·하위호환**으로 얹는다.

---

## 1. 목표 / 비목표

### 목표
- **TLS**: loopback 밖에서도 **암호화 + 서버 인증 + 클라이언트 인가**된 attach. tokio 없는 sync 스택 유지(프로젝트는 의도적으로 tokio 회피 — `oauth2 + ureq` 참조).
- **Delta**: 원격 링크에서 Viewport 대역폭 대폭 절감. 전체 grid 대신 변경분만. 손실/따라잡기 상황에서 keyframe로 안전 복구.
- 두 기능 모두 **핸드셰이크에서 협상**, 구버전/미협상 클라이언트는 기존 전체 스냅샷 경로 그대로.
- **UI 무변경**: `RuntimeEventReceiver`/`RuntimeEvent::Viewport` 계약을 바꾸지 않는다.

### 비목표
- 원격 명령 권한/capability 제한(RCE 경계). remote.rs의 기존 경고대로 이는 **선행/병행 별건**이다 — 아래 §4.6에서 경계만 명시.
- 멀티유저 신뢰/ACL, 세션 공유 정책.
- 압축(zstd 등) — delta 이후의 추가 최적화로 미룬다(§5.6 open question).

---

## 2. Feature 1 — Remote TLS

### 2.1 라이브러리 선택: rustls (권장) vs native-tls

**결론: `rustls`.**

| 기준 | rustls | native-tls |
|---|---|---|
| 순수 Rust / OpenSSL 의존 | 순수 Rust(crypto는 ring/aws-lc-rs). **OpenSSL 불필요** | Linux=OpenSSL(빌드/배포 부담), Windows=SChannel, macOS=SecureTransport(**deprecated**) |
| sync 스택 | `rustls::StreamOwned<Conn, TcpStream>`가 `Read+Write` — tokio 불필요 | sync `TlsStream` 존재하나 플랫폼 백엔드 편차 |
| TOFU 핀닝(커스텀 검증기) | `client::danger::ServerCertVerifier` **1급 지원** — 이 설계의 핵심 | 커스텀 검증 사실상 불가/빈약 |
| cross-platform 일관성 | 세 OS 동일 코드 | 백엔드별 동작 차이 |

부수 결정 — **crypto provider는 `ring`** (aws-lc-rs 아님): rustls 0.23 기본은 aws-lc-rs지만 이는 Windows에서 NASM+CMake C 툴체인을 요구한다. `ring` provider(`rustls::crypto::ring`)를 명시 선택해 Windows 빌드 마찰(§1.5 cross-platform)을 없앤다. 자기서명 인증서 생성은 `rcgen`(같은 crypto 계열)으로.

추가 crate(구현 시): `rustls`(ring provider), `rcgen`(cert 생성). `sha2`는 이미 workspace 의존(지문 계산에 재사용). **tokio/reqwest/openssl 없음.**

### 2.2 인증서 모델: 자기서명 + TOFU 지문 핀닝 (권장)

개인 도구의 원격 attach에 CA 인프라·사용자 제공 인증서는 과하다. **SSH `known_hosts` 방식**을 채택한다.

- **서버**: remote(비-loopback) 최초 활성화 시 `rcgen`으로 자기서명 인증서 + 키페어(ed25519 또는 ECDSA P-256) 생성.
  - **개인키**: **OS keyring에 저장**(§1.4 — “평문 secret 디스크 금지”). `keyring-core` 서비스/username 아래 PEM. 디스크에 개인키를 두지 않는다.
  - **공개 인증서(DER/PEM)**: 비밀이 아니므로 데이터 디렉터리(app dirs)에 저장 가능.
  - 시작 시: keyring에서 키 로드 + 디스크에서 cert 로드. 없으면 재생성.
- **클라이언트(TOFU)**: 특정 호스트에 **최초** attach 시, 서버가 (설정 UI/stdout으로) 출력한 **SHA-256(cert DER) 지문**을 사용자가 대역외(out-of-band)로 대조. 확인되면 클라이언트가 지문을 `known_hosts`(데이터 디렉터리, 평문 — 공개 지문이라 secret 아님)에 **핀닝**. 이후 접속은 커스텀 `ServerCertVerifier`가 **PKI/CA·hostname 검증 대신 지문 일치만** 확인.
  - 지문 불일치 = SSH식 강한 경고(“REMOTE HOST IDENTIFICATION HAS CHANGED”) 후 거부. 사용자가 명시적으로 갱신해야 재핀닝.

**기각한 대안**:
- 사용자 제공 인증서: 개인 도구에 과한 마찰.
- 공개 CA(Let’s Encrypt 등): 공개 DNS 필요 — 개인 remote엔 부적합.
- 자기서명 + 핀닝 없음: MITM 가능 — 핀닝이 서버 인증의 전부이므로 필수.

### 2.3 TLS × 기존 프레이밍 × 토큰 인증

**핵심: TLS는 채널 보안(기밀성 + 서버 인증), 토큰은 앱계층 인가. 둘 다 유지(belt-and-suspenders).**

- `write_frame`/`read_frame`은 `impl Read/Write`를 받으므로 `StreamOwned`(TLS) 위에서 **그대로** 동작한다. `[u32 len][postcard]` 프레이밍·16 MiB 상한·heartbeat 불변.
- 토큰은 이제 TLS 채널 **안**에서 `ClientHello`에 실려 전달 — v1이 loopback 평문으로 보내던 토큰이 네트워크에서 도청 불가해진다.
- **mTLS(클라이언트 인증서) vs 토큰**: 토큰 유지 권장. mTLS는 클라이언트 인증서 프로비저닝(추가 이동부품)이 필요. 개인 도구엔 `TLS(핀닝=서버인증) + token(클라이언트 인가)`가 최적점. mTLS는 다중 신뢰 클라이언트 도입 시 future(§5.6).

### 2.4 ⚠️ TLS가 깨는 것: `try_clone` 기반 3-스레드 분할 → **접속당 단일 I/O 스레드** (중요 발견)

현재 `serve_connection`은 reader/pump/shutdown이 `TcpStream::try_clone()`으로 **같은 fd**를 공유한다. **rustls `StreamOwned`는 clone 불가**하고, TLS record 계층(버퍼·시퀀스 상태)을 두 스레드가 동시에 read/write하면 안 된다. rustls `Connection`은 독립 read/write 半으로 안전 분할되지 않는다.

**해결(권장): TLS 접속은 접속당 스레드 하나가 rustls 스트림을 단독 소유**하고 poll/timeout 루프로 양방향을 처리한다.

```text
loop {
  if stop || done: break
  // 1) 나갈 이벤트 송신 (코얼레싱된 최신본)
  for wire in encode(receiver.drain()) { write_frame(tls, wire)? }   // 실패 → 접속 정리
  // 2) heartbeat (유휴 HEARTBEAT_INTERVAL 경과 시 길이 0 프레임)
  // 3) 짧은 read timeout으로 명령 프레임 흡수
  set_read_timeout(SHORT);
  match read_frame(tls) {
    Frame(bytes) => handle_command(decode(bytes)),
    Timeout      => continue,   // WouldBlock — 다음 tick
    Eof/Err      => break,
  }
}
```

- `read_frame`을 `Frame|Timeout|Eof` 3-값으로 정제해야 한다(현재는 성공/None 2-값 — TLS의 WouldBlock 중간-record와 EOF를 구분).
- **shutdown**: 다른 스레드는 TLS 상태를 만지지 않고, 접속 시작 시 떠둔 **raw `TcpStream` clone**에 `shutdown(Both)`만 호출(fd 레벨 — TLS 무관)해 poll 루프를 깨운다. `stop`/`conn_done` `AtomicBool`도 그대로.
- **평문 loopback 경로**도 이 단일 I/O 스레드 모델로 **통일** 권장(코드 경로 이원화 제거). 현재의 2-스레드(reader+pump) 모델은 제거 가능하며, 단일 스레드가 오히려 단순하다. `connections` 추적/Drop 계약은 그대로 재사용.
- **클라이언트도 동일하다(codex 지적).** `RemoteRuntimeClient`도 writer `TcpStream`과 reader clone으로 분할돼 있어(`remote.rs`의 writer/reader/heartbeat 경로), `StreamOwned` 단일 접속을 쓰면 **클라이언트도 “IO 스레드가 TLS 스트림 단독 소유, 명령·`RequestKeyframe`는 채널로 보냄”** 구조가 필요하다. 서버만 바꾸면 안 된다.
- **단일 IO의 새 기아(starvation) 리스크(codex 지적).** 현재 reader 스레드는 pump write가 막혀도 명령을 계속 읽는다. 단일 IO가 `receiver.drain()`을 **전량 write한 뒤** read하면, 느린 클라이언트/큰 viewport에서 write 구간이 길어져 **수신 명령 처리가 굶는다**. 완화: 한 tick의 write를 **N프레임/바이트로 상한**하고 그 사이 짧은 read를 끼우거나, write 전 non-blocking read로 대기 명령을 먼저 흡수한다. `read_frame`의 3-값 정제(§위)와 함께 “timeout/EOF/protocol error를 모두 `None`으로 접지 않기”가 전제.

이는 이 설계에서 가장 위험한 리팩터이므로 §5 구현 단계에서 별도 취급한다. 대안(제어/이벤트 2개 TLS 접속, `Connection` mutex+nonblocking, native-tls split)은 인증·상관·정리 복잡도나 사실상 이벤트 루프 재구현 때문에 **단일 IO owner가 최선**이다.

### 2.5 바인드 정책 · DNS rebinding / origin (§1.5)

- 기본은 **127.0.0.1 유지**(`serve()` 무변경).
- 비-loopback bind는 **TLS 구성 + 명시 opt-in**일 때만 허용하는 별도 진입점:

```rust
pub struct RemoteBindConfig {
    pub bind: SocketAddr,          // 예: Tailscale 인터페이스 IP
    pub allow_non_loopback: bool,  // 명시 true 필수
    pub tls: TlsServerConfig,      // 비-loopback이면 필수 (없으면 거부)
}
pub fn serve_tls(backend: InProcessRuntimeClient, cfg: RemoteBindConfig) -> anyhow::Result<RemoteRuntimeServer>;
```

- `bind`가 비-loopback인데 `allow_non_loopback=false`거나 `tls`가 없으면 **거부**. 비-loopback bind 시 로그 + 설정 UI에 **눈에 띄는 경고**.
- **DNS rebinding / Origin**: 우리는 브라우저가 아니라 TCP+TLS+토큰이므로 브라우저發 DNS-rebinding은 직접 적용되지 않는다. §1.5 지침(Origin 검증·localhost 기본·auth 필수)은 여기서 **cert 핀닝 + 토큰**으로 매핑된다: 핀닝이 “다른 서버로의 rebind/MITM”을 막고, 토큰이 auth를 강제한다. HTTP가 아니라 Origin 헤더는 없다.
- **운영 권고**: `0.0.0.0`가 아니라 **사용자 자신의 네트워크 신뢰 경계(Tailscale/WireGuard 등) 인터페이스 IP**에 bind하도록 문서화. 공개 인터넷 노출은 권장하지 않는다.

### 2.6 하위호환 / 핸드셰이크 버전닝

- **전송 보안(TLS 유무) ⟂ 프로토콜 버전(v1/v2 코덱)** — 직교. loopback은 평문 유지(cert 마찰 없음), 비-loopback은 TLS. v2 핸드셰이크는 평문·TLS 양쪽에서 동일하게 동작.
- v1(현재): 첫 프레임 = 원시 토큰, 응답 `b"ok"`. v2: 첫 프레임 = `ClientHello`(§3.1), 응답 `ServerHello`.
- v1은 **미출시 스켈레톤**이므로 v2로 하드 컷 가능. 다만 마이그레이션 창을 위해: 서버가 첫 프레임을 `ClientHello`로 postcard 디코드 시도 → 실패하면 v1 원시 토큰으로 폴백(magic 바이트로 구분, §3.1). 권장은 **v2 단일화**.

---

## 3. 핸드셰이크 & 와이어 프로토콜 (공통 기반)

TLS와 delta 둘 다 이 협상 위에 선다. 먼저 핸드셰이크·코덱을 만들고 그 위에 각 기능을 얹는다.

### 3.1 핸드셰이크 (첫 프레임 교환)

```rust
/// 클라이언트가 (TLS 수립 후) 보내는 첫 프레임. postcard.
struct ClientHello {
    magic: [u8; 4],      // b"DPRT" — v1 원시 토큰과 구분 + 오접속 조기 거부
    proto_version: u16,  // 2
    features: u32,       // 클라이언트가 지원하는 기능 비트마스크
    token: Vec<u8>,      // per-run 토큰 (TLS 안에서 안전)
}

/// 서버 응답. postcard.
struct ServerHello {
    proto_version: u16,  // 서버가 말하는 버전
    features: u32,       // features_ack = server_supported & client.features (교집합)
    // 인증 실패 시 서버는 ServerHello를 보내지 않고 접속을 끊는다 (기존 계약과 동일)
}

/// 기능 비트
const FEAT_DELTA_VIEWPORT: u32 = 1 << 0;
// 향후: FEAT_FRAME_COMPRESSION = 1 << 1, ...
```

- 서버: `magic`/`proto_version` 검증 → 토큰 **상수시간 비교**(기존 `token_matches` 재사용) → `features_ack` 계산 → `ServerHello` 회신. 협상된 `features_ack`가 **접속 전체의 코덱**을 결정한다.
- `AUTH_TIMEOUT`(5s) 침묵 방어, ACK 동기 확인(attach가 거부를 동기적으로 앎)은 v1 계약 그대로.

### 3.2 프레임 코덱: `WireMsg` (delta 협상 시에만)

- **미협상**(구버전/opt-out): 프레임 = `postcard(RuntimeEvent)` — **오늘 그대로**. Viewport는 전체 스냅샷.
- **협상(`FEAT_DELTA_VIEWPORT`)**: 프레임 = `postcard(WireMsg)`.

```rust
/// delta 협상 시 접속의 이벤트 프레임 인코딩.
enum WireMsg {
    /// 상태 이벤트 전부 + (viewport 아님) — 기존 RuntimeEvent 그대로 감싼다.
    Event(RuntimeEvent),
    /// 세션 viewport 전체 기준선. (재)구독·리사이즈·alt-screen·heavy repaint 시.
    ViewportKeyframe {
        session: SessionId,
        seq: u64,
        snapshot: Arc<TerminalViewportSnapshot>,
        bracketed_paste: bool,
    },
    /// 직전 전송본 대비 변경분.
    ViewportDelta {
        session: SessionId,
        seq: u64,        // 이 프레임 번호
        base_seq: u64,   // 이 delta가 딛는 직전 프레임(재구성 검증용)
        delta: ViewportDelta,
        bracketed_paste: bool,
    },
}
```

- 두 코덱은 와이어에서 호환되지 않으므로(variant prefix 상이) **핸드셰이크가 접속 단위로 코덱을 확정**한다 — 이것이 버전닝의 존재 이유.
- **heartbeat(길이 0 프레임)은 코덱과 무관하게 유지**: 수신측이 postcard 디코드 **전에** `frame.is_empty()`로 소비(현재 클라이언트가 이미 그러함). postcard `RuntimeEvent`/`WireMsg`는 항상 길이>0이라 충돌 없음.
- `RuntimeEvent` enum 자체는 **건드리지 않는다**(in-process 경로 무영향). delta는 transport 계층의 `WireMsg`에만 존재.

---

## 4. Feature 2 — Delta Viewport 스트리밍

### 4.1 diff 단위: **row 단위 content-diff** (권장)

- **단위 = 변경된 row**. alacritty 손상 모델(per-line damage)과 정렬되고, 재구성이 단순(`row*cols` 슬라이스 교체).
- **어떻게 변경 row를 찾나 — content-diff(서버에서 직전 전송본과 memcmp)**:
  - 서버 pump가 접속별 `last_sent: HashMap<SessionId, Arc<TerminalViewportSnapshot>>` 유지. 새 전체 스냅샷이 오면 row별로 `prev.visible_cells[r*cols..] != cur[...]` 비교.
  - 비용: `cols*rows` 셀 memcmp(예: 200×50 = 1만 셀) — 이미 매 프레임 수행하는 postcard 직렬화보다 훨씬 싸다.
- **기각: 손상(dirty_rows) 배관**. `feed()`가 `dirty_rows`를 계산하지만 (a) worker→snapshot까지 배관이 없고(현재 `dirty_ranges`는 항상 빈 Vec), (b) `TermDamage::Full`이 스크롤 등에서 **과보고**한다. content-diff가 더 견고하고 배관 변경이 없다. `dirty_ranges`를 채워 memcmp를 건너뛰는 것은 향후 최적화로만(§5.6).
- **cell-run(row 내 부분 구간)**: v1은 row 전체를 보낸다(row=`cols`셀, 80~200개로 작음). 타이핑도 어차피 커서 row를 더럽힌다. row 내 run 압축은 v2 옵션(§5.6).

### 4.2 왜 서버 pump에서 diff하나 (worker 아님)

- worker의 slot 모델(latest-wins)이 **서버가 diff를 보기 전에 이미 중간 Viewport를 코얼레싱**한다. 즉 서버 pump의 `receiver.drain()`은 세션별 **최신 전체 스냅샷**만 받는다. 그래서 delta = `diff(이 접속에 마지막으로 보낸 것, 지금 최신)` — 정확히 이 접속의 last_sent 대비 diff라 **중간 프레임이 필요 없다**.
- worker/in-process를 delta로 바꾸지 않으므로 in-process UI는 전체 스냅샷을 계속 받고 happens-before 계약이 불변.
- 트레이드오프: 클라이언트가 여러이면 접속마다 diff 중복. 개인 도구(1~2 접속)엔 무해. 다중 클라이언트 최적화는 future(§5.6).

### 4.3 slot 코얼레싱과의 충돌 해소 (핵심)

**긴장**: delta는 **순서 적용**이 필수인데 slot은 latest-wins로 중간을 **버린다**.

**해소: 재구성을 클라이언트 transport(reader/IO) 스레드에서 수행한다. slot에는 항상 “재구성된 전체 스냅샷”만 담긴다.**

- 클라이언트 IO 스레드는 **신뢰·순서 보장 TCP**를 **UI 소비와 무관하게 완전히 드레인**한다 → transport 계층에서 delta는 **절대 유실되지 않는다**.
- 접속당(구독자당 아님) 재구성 상태 유지:

```rust
// RemoteRuntimeClient reader 스레드 소유
struct Reconstructor {
    // 세션별 현재 재구성 스냅샷 + 마지막 적용 seq
    running: HashMap<SessionId, (Arc<TerminalViewportSnapshot>, u64)>,
}
```

- keyframe 수신 → `running[session] = (snapshot, seq)`. delta 수신 → `base_seq`가 `running` seq와 일치하는지 확인 후 적용:

```rust
fn apply(prev: &TerminalViewportSnapshot, d: &ViewportDelta) -> TerminalViewportSnapshot {
    debug_assert!(prev.cols == d.cols && prev.rows == d.rows);
    let cols = d.cols as usize;
    let mut cells = prev.visible_cells.to_vec();     // COW clone (오늘도 Arc 클론 취급 중)
    for patch in &d.changed_rows {
        let base = patch.row as usize * cols;
        cells[base..base + cols].copy_from_slice(&patch.cells);
    }
    TerminalViewportSnapshot {
        cols: d.cols, rows: d.rows,
        cursor: d.cursor,
        visible_cells: cells.into(),
        dirty_ranges: Vec::new(),
        title: d.title.clone(),
        scroll_offset: d.scroll_offset,
        is_alt_screen: d.is_alt_screen,
    }
}
```

- 적용 후 **재구성된 전체 스냅샷을 기존 latest-wins slot에 넣고**, 기존 `dispatch()`로 모든 구독자 slot에 배포한다. **UI는 여전히 `RuntimeEvent::Viewport`(전체)만 본다 — UI 무변경.** UI가 느려 slot이 덮어써도 안전(전체 스냅샷의 latest-wins는 올바름).
- 즉 **delta/순서는 TCP+IO 스레드에 갇히고, slot은 재구성 완료본만 본다.** 긴장 해소.

### 4.4 keyframe 조건 · seq · 재동기화

서버 pump가 keyframe을 보내는 경우:
1. 그 세션의 `last_sent`가 **없음**(신규 세션 / 아카이브 detach 후 재등장 / **재구독**).
2. **리사이즈**: `cols`/`rows` 변경(baseline 무효).
3. **alt-screen 토글**: `is_alt_screen` 변경(전면 재도색).
4. **heavy repaint 폴백**: 변경 row 수가 임계(예: 전체 row의 60%) 초과, 또는 추정 delta 바이트 ≥ keyframe 바이트 → keyframe이 더 싸다.
5. 클라이언트의 **명시 keyframe 요청** 수신 시(아래).

- **seq**: (접속, 세션)마다 viewport 프레임 송신 시 단조 증가. keyframe은 새 baseline, delta는 `base_seq = 직전 seq`.
- **재동기화(신뢰 TCP라 이론상 불필요하지만 방어)**: 클라이언트가 delta의 `base_seq`와 자신의 `running` seq 불일치를 보면 이후 delta를 버리고 keyframe 요청:

```rust
// WireMsg에 추가되는 클라이언트→서버 제어 (delta 협상 접속)
enum WireCmd {
    Command(RuntimeCommand),           // 기존 명령
    RequestKeyframe { session: SessionId },
}
```

- 서버가 `RequestKeyframe`를 받으면 `last_sent.remove(session)` → 다음 tick에 keyframe. **“방금 구독해 현재 화면이 필요”**한 경우도 이걸로 처리. (미협상 접속은 `WireCmd` 없이 기존처럼 `RuntimeCommand` 프레임만.)

> 참고: `RuntimeCommand`(command.rs)는 **append-only** 계약(postcard variant index). `WireCmd`는 transport 래퍼라 `RuntimeCommand` 자체는 불변.

### 4.5 리사이즈 · scrollback cap · Warm→Active 상호작용

- **리사이즈**(`RuntimeCommand::Resize`): grid 차원 변경 → §4.4-(2) keyframe.
- **scrollback cap 변경**(§14.3 hidden↔visible 시 `set_visible`이 ring buffer를 shrink/grow): cols/rows는 그대로지만 화면 내용이 크게 바뀔 수 있다. hidden→visible 전이 시 worker가 `push_watched_viewports()`로 전체 스냅샷을 emit → 서버 diff가 크면 heavy-repaint 폴백(§4.4-4)이 자동 keyframe. 또한 아카이브 detach 후 재등장이면 `last_sent` 없어 keyframe(§4.4-1).
- **Warm→Active 복귀**(`SetWorkspaceState(Active)`): worker가 MuxUpdated + 전체 viewport 재-push. 서버는 `last_sent`가 살아있으면 diff, 아니면 keyframe. 안전측: **재구독/재활성 직후 첫 viewport는 keyframe 강제**(구현에서 `last_sent.clear()` 트리거).

### 4.6 대역폭/CPU 트레이드오프

- `TerminalCell` ≈ postcard로 char(1~4B utf8) + fg(3) + bg(3) + wide(1) + spacer(1) ≈ **6~10B**. 80×24 전체 ≈ **12~20 KB**/프레임, 200×50 ≈ 60~100 KB.
- 타이핑: 1 row 변경 → header(~20B) + 1 row(80셀×~7B ≈ 560B) ≈ **~0.6 KB** (전체 대비 **~24×** 절감).
- 커서 깜빡임/이동만: 변경 row 0 → 헤더만 ~20B (커서·scroll_offset·title은 항상 헤더에 포함, 작음).
- 전면 재도색(vim redraw): 대부분 row 변경 → keyframe 폴백 → **오늘과 동일**(더 나쁘지 않음).
- CPU: 서버 memcmp `O(cols*rows)`(직렬화보다 저렴), 클라이언트 재구성은 접속당 grid COW clone 1회(오늘의 Arc 클론 취급과 동급).

### 4.7 ViewportDelta 타입 스케치

```rust
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct ViewportDelta {
    // 재구성 검증용 헤더 (전량 — 작다)
    cols: u16,
    rows: u16,
    cursor: CursorSnapshot,
    scroll_offset: i32,
    is_alt_screen: bool,
    title: Option<String>,
    // 바뀐 row들 (row-major 인덱스, 각 row는 정확히 cols개 셀)
    changed_rows: Vec<RowPatch>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct RowPatch {
    row: u16,
    cells: Vec<TerminalCell>,   // len == cols
}
```

서버 diff:

```rust
fn diff(prev: &TerminalViewportSnapshot, cur: &TerminalViewportSnapshot) -> Option<ViewportDelta> {
    if prev.cols != cur.cols || prev.rows != cur.rows || prev.is_alt_screen != cur.is_alt_screen {
        return None; // → keyframe
    }
    let cols = cur.cols as usize;
    let mut changed = Vec::new();
    for r in 0..cur.rows as usize {
        let rng = r * cols..(r + 1) * cols;
        if prev.visible_cells[rng.clone()] != cur.visible_cells[rng.clone()] {
            changed.push(RowPatch { row: r as u16, cells: cur.visible_cells[rng].to_vec() });
        }
    }
    // heavy-repaint 폴백
    if changed.len() * 100 >= cur.rows as usize * 60 {
        return None; // 60%+ 변경 → keyframe이 낫다
    }
    Some(ViewportDelta {
        cols: cur.cols, rows: cur.rows, cursor: cur.cursor,
        scroll_offset: cur.scroll_offset, is_alt_screen: cur.is_alt_screen,
        title: cur.title.clone(), changed_rows: changed,
    })
}
```

### 4.8 검증 (완료 기준)

- **왕복 등가성**: 임의 escape 시퀀스 스트림에 대해, keyframe 후 delta들을 적용한 재구성 스냅샷이 매 tick **source 전체 스냅샷과 셀 단위로 동일**(`==`)해야 한다. 이것이 delta 정확성의 goal-driven 기준.
- 리사이즈/alt-screen/heavy-repaint에서 keyframe 폴백이 발동하고 재구성이 깨지지 않음.
- 미협상(구버전) 접속이 전체 스냅샷을 그대로 받음(회귀).

---

## 5. 구현 단계 (의존성 · 리스크)

각 단계는 “verify” 기준으로 독립 검증 가능하게 쪼갠다.

### 단계 A — 핸드셰이크 v2 + 코덱 버전닝 (TLS·delta 없음)
- `ClientHello`/`ServerHello`, feature 협상, `WireMsg`/`WireCmd` 봉투 **정의**. 봉투는 협상 성공 시에만 프레이밍에 쓰인다.
- **중요(codex 지적): 기능 off 경로는 봉투로 감싸지 않는다.** `FEAT_DELTA_VIEWPORT` 미협상이면 프레임은 **기존 `postcard(RuntimeCommand)`/`postcard(RuntimeEvent)` 그대로**(§3.2) — 오늘과 동일 바이트. 즉 단계 A가 “봉투 도입”이라 해서 off-path 프레임을 바꾸면 안 된다. 그래야 A를 delta/TLS 없이도 **독립 배포**할 수 있다.
- **verify**: loopback attach/명령/이벤트 왕복(기존 테스트) 그대로 통과(off-path 바이트 불변). 잘못된 magic/version/토큰 거부.
- **리스크: 낮음.** 두 기능의 공통 토대.

### 단계 B — Delta viewport (평문 loopback + 단계 A 위)
- 서버 pump `last_sent` + `diff` + keyframe 폴백; 클라이언트 `Reconstructor` → slot; `RequestKeyframe`.
- **verify**: §4.8 왕복 등가성 테스트, 대역폭 측정(타이핑/커서/redraw 시나리오).
- **리스크: 중.** 재구성 정확성 — 등가성 테스트가 가드.

### 단계 C — TLS 전송
의존: 단계 A(핸드셰이크). 단계 B와 **직교**(병행 가능)하나 C-2가 `serve_connection`을 건드리므로 순서상 A 이후.
- **C-1 인증서/키 수명주기**: `rcgen` 자기서명 생성, 개인키 keyring 저장, cert 디스크 저장/로드/재생성. 독립 테스트 가능. 리스크: 중.
- **C-2 rustls 통합 + 접속당 단일 I/O 스레드**(§2.4): `StreamOwned`(ring provider), reader/pump 통합, `read_frame` 3-값 정제, raw-fd clone 전용 shutdown. **리스크: 높음**(sync rustls 스레딩) — 가장 큰 리팩터.
- **C-3 TOFU 핀닝**: 커스텀 `ServerCertVerifier`(지문 대조), 클라이언트 `known_hosts` 저장, 지문 변경 경고 UX. 리스크: 중.
- **C-4 비-loopback bind opt-in**: `RemoteBindConfig`, 경고 로그/설정 UI, TLS 없는 비-loopback 거부. 리스크: 낮음.
- **verify**: 비-loopback TLS attach 성공, 핀닝 불일치 거부, 잘못된 토큰 거부, shutdown이 유휴 TLS 접속에 블록 안 됨(기존 회귀 테스트의 TLS판).

### 순서 권장
권장: **`A → 평문 단일 I/O 통일 → (B ∥ C)`** (codex 지적 반영).

- 원안 `A → (B ∥ C)`는 **B와 C-2가 같은 `serve_connection`/클라이언트 IO 구조를 동시에 바꿔 충돌** 위험이 크다(B는 pump `drain` 송신 + 클라이언트 dispatch를, C-2는 같은 IO를 단일 스레드로).
- 그래서 A 직후 **평문 loopback을 §2.4의 단일 I/O 스레드 모델로 먼저 통일**(서버+클라이언트)해 IO 경계를 한 번만 재편한다. 이 위에서 B(코덱/재구성)와 C(TLS 스트림 교체)는 서로 겹치지 않게 얹힌다.
- C 내부: **C-1(인증서, 독립) 먼저**, 그다음 C-2(고위험). **C-2만으로 비-loopback을 열지 말 것** — 서버 인증(TOFU) 없는 노출은 위험하므로 **C-3 이후에만** C-4(비-loopback opt-in)를 활성화한다.
- B는 사용자 체감(대역폭) 이득이 크고 평문 위에서 검증 가능하니 C와 병행/선행 무방.

---

## 6. 보안 분석 (위협 모델)

| 위협 | 완화 |
|---|---|
| 도청(비-loopback) | TLS 기밀성. 토큰이 TLS 안에서 전달(v1 평문 노출 해소). |
| MITM / 서버 사칭 | TOFU cert 핀닝(지문 대조). 최초 접속 대역외 검증이 신뢰 앵커. |
| 무단 클라이언트 | per-run 토큰(상수시간 비교) + `AUTH_TIMEOUT`. |
| 리플레이 | TLS 세션 내 방어; 토큰은 실행 단위라 교차실행 리플레이 무의미. |
| DoS(자원 고갈) | 16 MiB 프레임 상한, auth 침묵 timeout, `validate_command`(기존). |
| 개인키 유출 | keyring 저장(§1.4), 디스크 평문 금지. cert(공개)만 디스크. |
| 최초 접속 지문 위조 | 대역외(설정 UI/stdout) 지문 대조를 사용자에게 **명시 요구**. SSH TOFU와 동일 한계. |
| **원격 명령 = 셸 권한** | remote.rs 기존 경고: `SpawnAgent`가 command/args/env를 실어 원격 attach = 런타임 완전 제어 = 셸 접근 등가. **토큰 보유자는 완전 신뢰 주체**로 취급. 비-loopback 노출은 “셸 접근 부여”로 문서화. capability 제한/제한 명령 스키마는 **별건 선행 작업**(§1 비목표) — 이 설계는 그 경계를 넓히지 않는다. |

---

## 7. Open Questions

1. **loopback도 TLS?** 권장: 평문 유지(마찰 최소). 설정으로 “TLS everywhere” 강제 옵션은 둘 수 있음.
2. **cert 만료/키 로테이션**: 자기서명 유효기간(예: 10년) 길게. 핀닝 검증기가 만료를 **무시**할지 존중할지 결정 필요(SSH는 만료 개념 없음 → 무시 쪽이 TOFU와 일관).
3. **지문 변경 UX**: `known_hosts` 포맷, 변경 시 재핀닝 흐름(설정 UI vs CLI).
4. **row 단위로 충분한가?** v1은 충분(타이핑 24× 절감). cell-run(row 내 부분)·프레임 압축은 필요 시 v2.
5. **worker diff vs 서버-접속 diff**: 서버-접속 채택(개인 도구). 다중 클라이언트 확산 시 worker 단일 diff 재고.
6. **압축 스택**: `FEAT_FRAME_COMPRESSION`(deflate/zstd)을 delta 위에 얹을지 — delta 먼저, 측정 후 결정.
7. **dirty_ranges 활용**: `feed()`의 per-row damage를 worker→snapshot으로 배관해 서버 memcmp를 생략하는 최적화(과보고 주의).
8. **crypto provider**: `ring` 확정 권장(Windows 빌드). aws-lc-rs 성능이 필요해질 근거가 생기면 재검토.
9. **mTLS 승격 경로**: 다중 신뢰 클라이언트 도입 시 토큰→클라이언트 인증서 전환의 마이그레이션.

---

## 8. codex xhigh 검토 반영 (2026-07-04)

이 문서는 codex-exec(gpt-5.5, xhigh)로 실제 코드(`remote.rs`/`in_process.rs`/`terminal`)에 비춰 검토했다. 결론: **방향은 타당, 구현 전 5개 보완 반영 완료**.

1. **단계 A 봉투 범위 명확화** — delta 미협상 시 `WireMsg` 봉투를 쓰지 않고 기존 `RuntimeCommand`/`RuntimeEvent` 프레임을 그대로 유지(§3.2, §5 단계 A). A의 독립 배포 조건.
2. **구현 순서 조정** — `A → 평문 단일 I/O 통일 → (B ∥ C)`. B와 C-2가 같은 `serve_connection`/클라이언트 IO를 건드려 충돌하므로 IO 경계를 먼저 한 번 재편(§5 순서 권장). C-2만으로 비-loopback 개방 금지(C-3 이후).
3. **클라이언트도 단일 I/O 스레드 필요** — 서버뿐 아니라 `RemoteRuntimeClient`도 writer/reader 분할이라 TLS `StreamOwned`엔 동일 리팩터 필요(§2.4).
4. **단일 I/O owner가 최선** — 2접속/mutex/native-tls 대안은 복잡도·이벤트루프 재구현 문제. shutdown용 raw `TcpStream` clone은 TLS에서도 유지 가능(§2.4).
5. **단일 I/O 기아 리스크 문서화** — drain 전량 write 후 read 시 느린 클라이언트가 명령 처리를 굶길 수 있음 → write 상한/중간 read로 완화. `read_frame`은 timeout/EOF/protocol error를 구분(모두 `None`으로 접지 않기)(§2.4).
