# Rust AI Agent Workspace — 모바일 PWA 원격 접속 PR 계획 v3.3 (방법 A)

작성일: 2026-07-11
대상: v2.5/v3.2 기반 구현 완료 코드베이스 (main)
문서 성격: **방법 A(내장 서버 + Tailscale HTTPS) 기준 P트랙 PR 계획서**
표기 주의: 본 문서의 PR-P 번호는 모바일 PWA 트랙 내부 번호다. v2.8 persistence 문서의 PR-P*(storage-core)와 무관하다.

---

## 0. 목적 / 확정 설계 방향 (방법 A)

폰 브라우저(PWA)에서 데스크톱 앱의 **에이전트 승인/거부 + 상태 대시보드**를 원격으로 쓴다. 풀 터미널은 보조 기능이다.

```text
1. Rust 앱에 소형 HTTP+WS 서버 내장 — tokio 금지, sync tungstenite + 스레드
   (remote.rs의 rustls + non-blocking 단일 IO 스레드가 선례, 웹 서버는 접속 수가
   1~2대라 접속당 블로킹 스레드로 단순화)
2. PWA 정적 자산은 바이너리에 임베드(include_bytes!). vanilla HTML/JS/CSS —
   bun/Next.js 등 별도 런타임/빌드 스텝 없음
3. HTTPS는 Tailscale: 기본 = `tailscale serve`(데몬이 TLS 종단, 앱은 127.0.0.1 평문),
   대안 = `tailscale cert`(ts.net Let's Encrypt PEM으로 rustls 자체 TLS).
   평문 HTTP로는 SW/설치/웹푸시가 성립하지 않는다(secure context 필수)
4. 서버 계층은 static_srv(정적 서빙)와 ws_api(WS API)로 분리 — 방법 B(클라우드
   앱 셸) 이전 시 static_srv만 교체되도록
5. 전제: 폰에 Tailscale 앱 설치 + 같은 tailnet 로그인 (tailnet 밖 노출 없음,
   Funnel 미사용)
```

## 0.1 검증된 사실 (2026-07 웹서치 + 로컬 확인)

```text
[Tailscale]
- tailscale serve: 리버스 프록시 백엔드는 http://127.0.0.1만 지원(포트/부분/전체
  URL 표기). HTTPS는 자동 발급 인증서로 데몬이 종단. --bg로 백그라운드 유지.
  macOS App Store 변형은 포트 프록시 지원(파일 서빙만 오픈소스 변형 제한).
  → https://tailscale.com/kb/1242/tailscale-serve
- tailscale cert: MagicDNS + admin console HTTPS Certificates 활성화 필요.
  <machine>.<tailnet>.ts.net 이름으로 Let's Encrypt(DNS-01) 발급. 90일 만료 —
  cert 파일을 직접 쓰면 갱신은 사용자 책임. 머신명이 공개 CT 로그에 남는다.
  → https://tailscale.com/docs/how-to/set-up-https-certificates
- serve의 WebSocket 프록시: 공식 문서에 명시 없음. 실사용 보고는 많으나 특정
  버전에서 10–40초마다 1001로 끊기는 이슈 리포트 존재 → P1이 cert 자체 TLS
  모드를 함께 제공하는 이유.
  → https://github.com/tailscale/tailscale/issues/18827
- 이 머신: /Applications/Tailscale.app/Contents/MacOS/Tailscale v1.98.8 존재,
  PATH 미등록(`which tailscale` 실패) — 문서/설정 UI에서 전체 경로 안내 필요.

[iOS PWA]
- Service Worker는 secure context(HTTPS 또는 localhost) 전용.
  → https://developer.mozilla.org/en-US/docs/Web/API/Service_Worker_API
- 웹푸시는 iOS/iPadOS 16.4+ 그리고 **홈화면에 추가된(설치형) 웹앱 전용** —
  Safari 탭에서는 PushManager 접근 불가. manifest `display: standalone` 필요.
  표준 VAPID 사용(별도 Apple 등록 불필요). Notification.requestPermission은
  사용자 제스처(클릭 핸들러) 안에서만 동작. iOS의 모든 브라우저는 WebKit이라
  제약 동일.
  → https://github.com/andreinwald/webpush-ios-example
  → https://www.magicbell.com/blog/pwa-ios-limitations-safari-support-complete-guide

[App Badging]
- iOS/iPadOS 16.4+: 홈화면 웹앱 전용 + 알림 권한 승인 후에만 뱃지 표시.
  → https://webkit.org/blog/14112/badging-for-home-screen-web-apps/
- Android Chrome: setAppBadge 미지원. 대신 알림 도착 시 OS가 자동 뱃지 —
  P4(푸시)만 있으면 Android 뱃지는 공짜.
  → https://developer.mozilla.org/en-US/docs/Web/Progressive_web_apps/How_to/Display_badge_on_app_icon
```

## 0.2 재사용 자산 맵 (코드 확인 결과)

```text
- crates/runtime/src/remote.rs: 토큰 인증·프레이밍·delta viewport·heartbeat(15s)·
  큐 상한 관례. 단, RemoteRuntimeServer는 전용 fresh worker를 소유한다
  (app.rs:1826 — 원격 클라가 스스로 세션 생성). GUI 세션을 미러링하지 않으므로
  PWA는 이 서버를 재사용하지 않고 "프로토콜·스레드 관례"만 가져온다.
- crates/runtime/src/in_process.rs:253,287: InProcessRuntimeClient는 다중 구독
  fan-out 지원. subscribe_with_wake로 상태 이벤트 도착 시 임의 스레드를 깨움
  (§14.1 Warm 알림용) — PWA WS 계층이 GUI worker를 탭하는 지점.
- crates/runtime/src/event.rs: SessionStatusChanged / SessionStatusViewChanged /
  SessionExited / ResourceUsage — 대시보드 데이터 소스로 충분.
- crates/terminal/src/viewport_snapshot.rs: TerminalViewportSnapshot(serde 지원
  이미 있음) + session.rs take_dirty_ranges — P5 프레임 직렬화 소스.
- 승인: pending_approvals는 proxy↔GUI가 **공유 DB로 IPC**하는 구조
  (storage/db.rs:1164 list_pending_approvals / 1169 resolve_approval,
  mcp-proxy는 자체 Db 연결 사용). 웹 계층도 자체 Db 연결로 동일 관례 —
  승인 경로에 runtime 개입 불필요.
- crates/runtime/src/tls_identity.rs: keyring 비밀 저장 + rcgen 관례 —
  웹 토큰/VAPID 키 영속화에 같은 패턴 적용(자기서명 cert 자체는 브라우저가
  거부하므로 재사용하지 않음).
- crates/app/src/ui/settings.rs:1800 remote_page + RemoteAction/RemoteView 패턴,
  crates/app/src/config.rs RemoteConfig — 설정 UI/config 확장 선례.
- docs/remote-tls-delta-design.md §2.5: 비-loopback bind는 TLS+명시 opt-in 필수.
  §1.5 Origin 지침이 브라우저 HTTP 세계에서는 이제 직접 적용된다(Host/Origin 검증).
```

## 0.3 공통 완료 조건 / 리소스 예산

모든 PR 공통:

```bash
cargo fmt --check && cargo clippy --workspace --all-targets
cargo check --workspace --all-targets && cargo test --workspace
cargo xtask check-deps    # 신규 의존성은 정책 등록 포함
cargo xtask i18n-check    # 신규 설정 문자열
```

리소스 예산(§14 관례, release 빌드 실측·측정 조건 PR에 명시):

```text
- 기능 OFF: 추가 스레드/소켓/타이머/폴링 0 — idle CPU 증가 0%p, RAM 증가 0
- ON + 접속 0: accept 블로킹 대기만. 이벤트 drain/승인 폴링 정지 — idle CPU 증가 0%p
  (예외: P4 푸시 구독 ≥1이면 5s 간격 SELECT 1회 허용, CPU 증가 0.5%p 이하 실측)
- 스트림 중(P5): CPU +5%p, RAM +10MB 이내
- idle repaint 0회 관례 훼손 금지 (웹 계층은 egui repaint를 유발하지 않는다)
```

---

## PR-P1 — web-remote 크레이트: 임베드 정적 서빙 + Tailscale 2모드 + 설정 UI

### 목표

폰 Safari에서 `https://<machine>.<tailnet>.ts.net`로 임베드된 앱 셸이 뜬다. WS API는 아직 없다.

### 범위

```text
신규 crates/web-remote/ (listener, http.rs=최소 HTTP/1.1, static_srv.rs, assets/)
crates/app: app.rs(WebRemoteState — RemoteTlsState 관례), config.rs(WebConfig),
            ui/settings.rs(Category::MobileWeb 페이지)
Cargo.toml: tungstenite(sync) 워크스페이스 등록(실사용은 P2), i18n 카탈로그
docs/mobile-pwa-design.md: 설계 + tailscale serve/cert 셋업 가이드
```

### 구현 요점

- 단일 포트, 단일 accept 스레드 + 접속당 블로킹 스레드. 요청 라인/헤더만 파싱하는 수제 HTTP/1.1(GET + Upgrade 판별, 경로 화이트리스트, 그 외 404) — 서버 프레임워크 의존성 없음, remote.rs 수제 프레이밍 관례와 일치.
- **2모드**: `serve 모드`(기본) = 127.0.0.1 평문 bind + `tailscale serve --bg <port>` 안내. `cert 모드` = 설정에 PEM 경로(cert/key) 지정 시 rustls로 자체 TLS, Tailscale 인터페이스 IP bind. **비-loopback + 평문 조합은 거부**(§2.5 관례). 두 모드 모두 브라우저 오리진은 동일한 ts.net 호스트명 → SW/푸시 연속성 유지.
- **Host/Origin 검증**: 설정된 ts.net 호스트명과 localhost만 허용, 불일치 403 — DNS rebinding 차단. 정적 응답에 `Content-Security-Policy: default-src 'self'` 부여.
- 정적 자산 `include_bytes!` + 컴파일 타임 매니페스트(경로→(MIME, bytes)). static_srv는 trait 뒤에 두지 않되 ws_api와 모듈 경계 분리(방법 B 대비).
- 페어링 토큰: 32바이트 랜덤을 keyring에 영속(`tls_identity` 관례, 재시작 후 재페어링 방지). 설정 UI에 표시/복사/재발급. 이 PR에서는 발급·표시까지만(검증은 P2).

### 완료 기준

- 폰 실기기: serve 모드로 ts.net URL 접속 → 앱 셸 렌더 확인. cert 모드도 동일(맥 1대 실측).
- 단위 테스트: 경로 화이트리스트, Host 불일치 403, 비-loopback 평문 bind 거부.
- 실측: OFF 시 스레드/소켓 증가 0·idle CPU 증가 0%p. ON+접속 0 시 idle CPU 증가 0%p.
- `cargo xtask check-deps` 통과(tungstenite 트리 등록), i18n-check 통과.

### 리스크·미결정

- serve 모드는 Tailscale 데몬 상태에 의존 — 설정 UI에 "serve 활성 여부"까지 감지할지(CLI 호출) vs 문서 안내만 할지 미결정(기본: 문서 안내만, CLI 자동 실행 안 함).
- cert 모드 90일 갱신은 사용자 책임 — 만료 임박 경고 UI는 후속.
- QR 페어링(qrcode 크레이트 1개 추가)은 미결정 — P2에서 토큰 입력 UX 확인 후 결정.

---

## PR-P2 — WS API v1: 승인/거부 + 상태 대시보드 (킬러 기능)

### 목표

폰에서 pending 승인을 보고 Allow/Deny(+remember)하고, 세션 상태·리소스를 실시간으로 본다.

### 범위

```text
crates/web-remote: ws_api.rs(tungstenite 업그레이드 + 접속 스레드),
                   protocol.rs(JSON 메시지, 버전 필드), dashboard_state.rs
crates/app: app.rs — GUI worker의 subscribe_with_wake 핸들을 web-remote에 전달,
            db_path 전달(웹 계층 자체 Db 연결)
assets/: 대시보드 + 승인 카드 UI(JS)
```

### 구현 요점

- 프로토콜은 **JSON 텍스트 프레임**(브라우저 친화 — postcard 바이너리 코덱은 데스크톱 원격 전용으로 유지). 첫 프레임 = `{v, token}` 인증, 5초 타임아웃·상수시간 비교(remote.rs AUTH_TIMEOUT 관례). 15초 ping(HEARTBEAT 관례 — serve 프록시 idle 절단 대응 겸용).
- **GUI worker 탭**: `subscribe_with_wake`로 구독하고 wake 클로저가 web-remote 이벤트 스레드를 unpark → **egui 프레임과 무관하게** SessionStatusChanged/SessionExited/ResourceUsage를 수신, 경량 상태맵(dashboard_state)을 갱신해 접속 중인 WS로 push. 접속 0이면 drain 정지(수신 큐는 fan-out 상한 관례 적용), UI 코드(ActivityUi)에 의존하지 않고 동일 이벤트 소스만 공유.
- **승인 경로는 DB 직행**: 웹 계층이 자체 `storage::Db` 연결(프록시 크로스 프로세스 관례)로 접속 중에만 1초 폴링 → pending 목록 push. Allow/Deny 수신 시 `resolve_approval(id, allowed, remember, now)`. GUI 팝업과 폰이 동시에 결정하는 경합은 기존 재-resolve 가드 의미를 테스트로 고정(최초 결정 우선).
- `arguments_preview`는 proxy가 이미 redact한 표시용 텍스트 — 신뢰하지 않고 JS에서 `textContent`로만 삽입(innerHTML 금지). raw 로그/secret은 어떤 경로로도 웹에 내보내지 않는다(불변 원칙 6/10).

### 완료 기준

- E2E(실기기): 도구 호출 → 폰 승인 카드 표시 → Allow → proxy 진행 / Deny → 거부. remember가 permission 규칙으로 영속되는 것 확인.
- 단위: 인증 실패/타임아웃 절단, 동시 resolve 경합, 접속 0 시 폴링·drain 정지.
- 실측: 접속 1 + 유휴 CPU 증가 1%p 이하, 세션 10개 상태 변화 시 대시보드 반영 ≤1s.
- GUI 승인 팝업 동작 회귀 없음(창 숨김 시 GUI는 안 뜨지만 폰으로는 승인 가능 — 이 조합이 본 기능의 가치).

### 리스크·미결정

- wake 클로저 수명: worker보다 web 스레드가 먼저 죽는 순서 관리(Drop 순서 명시 필요).
- 대시보드 범위 = **활성 workspace worker의 세션**(§14.1 — Suspended/Closed workspace는 라이브 상태가 없음). 비활성 workspace는 DB 메타데이터 목록만 표시할지 미결정.
- 승인 폴링을 SQLite `data_version` 체크로 더 줄일지는 실측 후 결정(1s SELECT면 예산 내 예상).

---

## PR-P3 — PWA 설치 셸: manifest + Service Worker + Badging

### 목표

홈화면 설치형 앱이 된다: 아이콘·standalone 표시·오프라인 셸·앱 아이콘 뱃지(열려 있는 동안).

### 범위

```text
crates/web-remote/assets/: manifest.webmanifest(display: standalone),
  sw.js(앱 셸 cache-first, API 네트워크 전용), 아이콘 세트(sijobird 기반
  192/512 + apple-touch-icon), 설치 안내 화면(iOS 공유시트 수동 설치 안내)
static_srv: sw.js 스코프/캐시버스트 헤더
```

### 구현 요점

- SW는 정적 셸만 캐시(버전 키 = 빌드 해시) — WS/승인 데이터는 캐시 금지. 오프라인이면 "연결 끊김 + 마지막 상태 시각" 화면.
- Badging: 대시보드가 pending 승인 수를 `navigator.setAppBadge(n)`으로 반영. iOS는 **알림 권한 승인 후에만 표시**되므로(0.1절) 권한 유도 버튼을 설치 후 첫 화면에 배치 — P4 푸시 권한과 동일 제스처로 통합.
- iOS는 beforeinstallprompt가 없다 — Safari 공유시트 "홈 화면에 추가" 수동 안내 UI.

### 완료 기준

- iOS 실기기: 홈화면 설치 → standalone 실행 → 서버 재시작 후에도(오프라인) 셸 로드 → 재연결.
- 뱃지: pending 2건 → 아이콘 뱃지 2, 모두 해소 → clearAppBadge (iOS 16.4+ 실기기, 알림 권한 승인 상태).
- SW 갱신: 자산 변경 배포 후 재방문 2회 내 새 셸 반영(버전 키 테스트).
- 리소스: 서버 측 변화 없음(정적 자산 추가뿐) — OFF=0 재확인.

### 리스크·미결정

- iOS 설치형 PWA는 사파리와 저장소가 분리 — localStorage 토큰을 설치 후 재입력해야 함(페어링 화면이 설치 후 1회 더 뜨는 UX 허용).
- 아이콘/이름 최종안(sijobird 자산 활용) 미결정.

---

## PR-P4 — Web Push (VAPID): 앱이 닫혀 있어도 승인 요청 알림

### 목표

폰이 주머니에 있어도 승인 대기/세션 완료·입력대기 푸시가 온다. Android 뱃지는 이걸로 자동 해결.

### 범위

```text
crates/web-remote: push.rs — VAPID JWT(ES256) + RFC 8291 aes128gcm 암호화 +
  ureq POST(sync, tokio 불필요). 트리거 스레드(구독 ≥1일 때만 5s 폴링)
Cargo.toml: p256, hkdf, aes-gcm, base64(RustCrypto sync 계열) 등록
crates/storage: web_push_subscriptions 테이블(additive 마이그레이션:
  endpoint PK, p256dh, auth, created_at, last_ok_at)
assets/: 구독 UI(사용자 제스처 내 requestPermission→subscribe), sw.js push 핸들러
VAPID 키쌍: 개인키 keyring(tls_identity 관례), 공개키는 JS 노출
```

### 구현 요점

- 발송 대상 이벤트: pending 승인 생성(핵심), 세션 완료/입력대기(SessionStatusChanged). 페이로드는 제목/종류/개수만 — **도구 인자·로그 내용은 푸시에 싣지 않는다**(push service 제3자 경유이므로, redaction 원칙의 확장).
- 트리거: 승인은 DB 폴링(구독 ≥1일 때만 5s — GUI 폴링은 프레임 의존이라 재사용 불가), 상태는 P2의 이벤트 구독 경로 재사용. 발송은 ureq 동기 호출을 전용 스레드에서(WS 스레드 블로킹 금지).
- 410/404 응답 구독은 즉시 삭제, 실패는 재시도 1회 후 포기(폭주 방지).
- iOS 제약 명시: 설치형 + 사용자 제스처 권한 요청 + iOS 16.4+(0.1절 출처). 데스크톱 알림(notify-rust)과 중복되지 않게 "폰 푸시는 폰 구독이 있을 때만" — 억제 규칙 없음(서로 다른 기기라 중복 아님).

### 완료 기준

- iOS 실기기: PWA 완전 종료 상태에서 승인 발생 → 푸시 수신 → 탭하면 승인 카드 딥링크.
- Android Chrome 1기기: 푸시 수신 + 아이콘 자동 뱃지 확인.
- 마이그레이션: `cargo xtask smoke-db-migrations` 통과, 구식 DB에서 무손실 업그레이드.
- 실측: 구독 1 + 유휴 CPU 증가 0.5%p 이하(5s SELECT), 구독 0이면 폴링 스레드 정지(0%p).
- RFC 8291 암호화 라운드트립 단위 테스트(고정 벡터).

### 리스크·미결정

- 웹푸시 스택(ECDH/HKDF/JWT) 수제 구현은 이 트랙 최대 구현 리스크 — 기존 web-push 크레이트는 async(hyper) 의존이라 배제. 고정 테스트 벡터 + 실기기 검증으로 상쇄.
- Apple push 엔드포인트의 전달 지연/스로틀은 통제 밖 — "긴급 승인은 폰을 여는 UX"를 문서화.
- 알림 클릭 딥링크 URL 스킴(경로 라우팅) 미결정 — P3 셸 라우터에 맞춰 확정.

---

## PR-P5 — 터미널 뷰어 (보조 기능, 후순위)

### 목표

폰에서 특정 세션 화면을 읽기 전용으로 실시간 열람(+ Ctrl-C 등 최소 제어).

### 범위

```text
crates/web-remote: protocol.rs 확장(ViewportKeyframe/DirtyRows JSON),
  viewer 스트림 경로(keyframe + dirty rows, RequestKeyframe 재동기화)
crates/runtime: "원격 시청" 승격 커맨드 신설(아래), session.take_dirty_ranges 재사용
assets/: 셀 그리드 렌더러(canvas), 세션 선택, Ctrl-C/Enter 버튼
```

### 구현 요점

- 직렬화 소스는 TerminalViewportSnapshot(serde 이미 지원) + dirty_ranges — remote.rs delta 설계(§4 keyframe/seq/재동기화)를 JSON으로 이식. Viewport는 최신본만 유지·coalesce(슬롯 관례).
- **§14 충돌 해소가 핵심 설계 결정**: hidden 세션은 스냅샷 생성 금지가 불변 원칙(5번)이다. 폰이 시청하는 동안만 해당 세션을 "visible 등가"로 승격하는 RuntimeCommand(시청 refcount)를 추가하고, 시청 종료·WS 절단·세션 종료 시 반드시 원복(tombstone 관례로 trailing viewport 차단). GUI 렌더 예산에는 영향 없음(스냅샷 생성만 허용, egui repaint 아님).
- 입력은 Ctrl-C/Enter 버튼만(WriteInput 재사용). 자유 타이핑·IME는 비범위 — 필요해지면 별도 PR.

### 완료 기준

- 실측(release, 10MB/min 출력 세션 1개 시청): 데스크톱 CPU 증가 +5%p 이내, RAM +10MB 이내, 폰 표시 지연 체감 ≤1s.
- 시청 종료/절단 시 세션이 hidden 예산으로 복귀(스냅샷 생성 중단)를 테스트로 고정.
- seq 불일치 → RequestKeyframe 재동기화 단위 테스트. 한글(wide cell) 렌더 확인.

### 리스크·미결정

- JSON 셀 페이로드가 크면(80×24 keyframe 수십 KB) 대역폭·인코딩 CPU가 예산을 칠 수 있다 — 실측 후 행 단위 RLE 또는 permessage-deflate 검토(§4.6 트레이드오프 관례, 선최적화 금지).
- 시청 승격 커맨드가 mux 상태기계에 넣는 복잡도 — 설계 리뷰(runtime 소유자) 선행 권장.
- 스크롤백 열람 범위(현재 화면만 vs Scroll 커맨드 연동) 미결정 — v1은 현재 화면만.

---

## 방법 B(클라우드 앱 셸)로의 이전 경로

방법 B는 앱 셸(HTML/JS/manifest/SW)만 클라우드 정적 호스팅(예: Cloudflare Pages)으로 옮기고, WS API는 지금처럼 각 데스크톱의 tailnet 주소로 직접 붙는 구조다. 본 계획은 그 경계를 미리 갈라 둔다: P1의 static_srv/ws_api 모듈 분리, P2의 "JSON 프로토콜 + 토큰 인증 + Origin 화이트리스트"가 이전 시 바뀌는 전부다. 구체적으로 (1) static_srv를 끄고 자산을 클라우드에 배포, (2) ws_api의 Origin 허용 목록에 클라우드 오리진 추가(CORS/CSWSH 재검토), (3) SW·푸시 구독·localStorage 토큰이 오리진에 묶이므로 클라우드 오리진 기준 재페어링 1회, (4) 접속 화면에 "어느 머신(ts.net 호스트)에 붙을지" 선택 UI 추가. 유의: 방법 B에서도 WS 종단은 tailnet 안이므로 폰의 Tailscale 가입은 여전히 필요하다 — 이 제약까지 없애려면 공개 릴레이(Funnel 또는 자체 중계 서버)가 필요하며 그건 별도 보안 검토 대상이다.

---

## 참고 출처

- https://tailscale.com/kb/1242/tailscale-serve
- https://tailscale.com/docs/how-to/set-up-https-certificates
- https://github.com/tailscale/tailscale/issues/18827
- https://developer.mozilla.org/en-US/docs/Web/API/Service_Worker_API
- https://github.com/andreinwald/webpush-ios-example
- https://www.magicbell.com/blog/pwa-ios-limitations-safari-support-complete-guide
- https://webkit.org/blog/14112/badging-for-home-screen-web-apps/
- https://developer.mozilla.org/en-US/docs/Web/Progressive_web_apps/How_to/Display_badge_on_app_icon
