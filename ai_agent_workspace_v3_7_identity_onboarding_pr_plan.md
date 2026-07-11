# v3.7 — 세션 identity 영속화 · 승인↔세션 연결 · serve 온보딩 PR 계획

세 작업을 한 문서로 묶는다. **순서가 중요하다**: identity(I1)가 나머지 둘의 토대이자
현재 여러 제약의 공통 근본 원인이고, 승인 연결(I2)은 그 위에서 완성된다. serve
온보딩(O1)은 독립이라 언제든 병렬 가능하다.

---

## 0. 조사로 확인한 현재 구조 (설계의 근거 — 추측 아님)

```text
- sessions 테이블(§11.1)에 이미 **영속 UUID**가 있다: sessions.id TEXT PRIMARY KEY,
  mux_panes.session_id → sessions.id FK (crates/persist/src/lib.rs:85-99).
  즉 "영속 세션 id"를 새로 만들 필요가 없다 — **런타임이 그걸 안 들고 있을 뿐**이다.
- runtime SessionId(u64)는 worker-로컬 카운터(next_id, in_process.rs:177 — worker마다
  1부터). PersistPipe가 spawn 시 UUID를 발급해 DB에 넣지만(persistence.rs:115
  session_spawned), 그 UUID는 **worker 내부에만** 있고 이벤트/스냅샷에 실리지 않는다.
- 복원 경로는 persistent_id(UUID)로 행을 되찾아 재결속한다(persistence.rs:152).
  → 워커는 이미 SessionId ↔ persistent UUID 매핑을 갖고 있다.
- mcp-proxy는 **이미 DEPPY_SESSION_ID(=pane_id)를 env로 받는다**(main.rs:141) —
  hook/statusLine 경로가 그것으로 needsInput/turn_done을 DB에 쓴다. 승인 등록
  경로(hook.rs:148 insert_pending_approval)만 그 값을 안 싣고 있다.
- pending_approvals 스키마: id, server_id, tool_name, arguments_preview, schema_hash,
  status, remember, created_at, resolved_at (세션 컬럼 없음).
- tailscale 감지는 status --json의 Self.DNSName만 읽는다(crates/app/src/tailscale.rs).
  serve 설정 여부·tailnet HTTPS 활성 여부는 보지 않는다.
```

### 0.1 이 세 작업이 푸는 실제 증상

| 증상 | 근본 원인 | 해결 |
|---|---|---|
| 폰에서 warm/유휴 워크스페이스 세션을 못 봄(표시 전용) | 세션 id가 worker-로컬 | I1 |
| 폰 재접속 auto re-watch가 엉뚱한 세션에 붙을 수 있음 | 〃 | I1 |
| 알림 딥링크가 재시작 후 다른 세션을 염 | 〃 | I1 |
| 승인 카드에서 "이게 어느 세션이지?"를 못 봄 | 승인 행에 세션 없음 | I2 |
| 새 Mac에서 QR이 안 열림(오늘 겪음) | serve 미설정/미승인을 앱이 모름 | O1 |

---

## PR-I1 — 세션 identity 영속화 (근본)

### 목표

런타임 이벤트/명령/웹 프로토콜이 **영속 세션 UUID**를 함께 나른다. worker-로컬 u64는
내부 최적화로 남기고, **경계를 넘는 식별자는 UUID**로 통일한다.

### 설계 결정 (대안 검토 포함)

- **A안(채택): u64 유지 + UUID 동반**. RuntimeEvent/Command의 SessionId(u64)는 그대로
  두고, 이벤트에 `persistent_id: Option<String>`을 **추가**한다. 웹 프로토콜과 딥링크는
  UUID만 쓴다.
  - 장점: postcard wire 호환(append-only), runtime 내부 HashMap<u64> 경로 무변경,
    변경 표면이 작다.
  - 단점: 두 id가 공존 — "경계에선 UUID, 내부에선 u64" 규칙을 문서/리뷰로 강제해야 한다.
- **B안(기각): SessionId를 UUID로 교체**. 모든 맵/이벤트/명령/remote wire가 바뀐다
  (~40개 파일). postcard wire 호환이 깨지고(원격 데스크톱 클라 재배포), 핫 경로가
  String 키가 된다. 이득(식별자 하나)에 비해 위험·비용이 과하다.
- **C안(기각): 웹 계층만 매핑 테이블 유지**(브리지가 u64↔UUID 맵을 보관). 워크스페이스
  전환/재시작마다 맵이 깨지고, 결국 워커가 알려줘야 하므로 A안의 열화판이다.

### 범위

```text
crates/runtime: event.rs(SessionSpawned류 이벤트에 persistent_id 동반 — enum 끝 append),
  in_process.rs(worker가 SessionId→UUID 맵 보유: spawn 시 PersistPipe가 발급한 UUID를
  기억, 복원 시 persistent_id 그대로, kill/close 시 정리), MuxSnapshot의 PaneSnapshot에
  persistent_session_id 추가(웹/UI가 pane→UUID를 알 수 있게)
crates/persist: PersistPipe가 발급 UUID를 호출측에 돌려준다(현재는 내부에만)
crates/web-remote: protocol.rs(SessionView.id를 UUID 문자열로, Watch{session:String}),
  dashboard.rs(watchers 키를 UUID로, lease 명령 시 UUID→u64 역매핑은 **워커가** 한다),
  ws_api.rs, assets/app.js
crates/runtime/command.rs: SetRemoteViewing/WriteInput/Scroll이 UUID를 받는 변형 추가?
  → **아니다**: 워커가 UUID→u64를 해석하므로 명령은 u64 그대로. 웹 계층이 브리지에서
  UUID를 넘기면 **브리지가 워커에게 UUID로 묻는** 대신, 워커가 이벤트로 알려준 매핑을
  브리지가 캐시해 u64로 변환한다(단방향 — 워커가 진실).
```

### 구현 요점

- 워커가 `SessionId → persistent UUID` 맵을 소유하고, **MuxUpdated 스냅샷의 각 pane에
  UUID를 실어 보낸다**. 브리지/UI는 그 스냅샷만으로 UUID를 안다 — 별도 이벤트 불필요.
- 웹 프로토콜은 UUID만 노출한다(u64는 폰에 안 보낸다). 브리지가 최신 MuxUpdated로
  UUID→u64 맵을 유지하고, 시청/입력 명령을 만들 때 변환한다. **UUID를 못 찾으면
  명령을 만들지 않는다**(현재 세션 목록에 없는 = 다른 워크스페이스/죽은 세션 → no-op).
  이것이 앨리어싱의 구조적 차단이다(u64를 폰이 아예 모르므로 위조도 불가).
- **비활성 워크스페이스 시청은 여전히 불가**(워커가 없으니 스냅샷 소스가 없다) —
  I1은 "엉뚱한 세션이 잡히는 것"을 없애지, "warm 세션을 보게" 만들지는 않는다.
  warm 시청은 워커가 살아 있으므로 **후속 PR(I1b)**에서 워크스페이스별 command sink +
  구독을 붙이면 가능해진다. 유휴(워커 없음)는 여전히 표시 전용이 옳다.
- 딥링크/재접속: 폰이 UUID로 watch → 재시작 후에도 같은 UUID면 같은 세션. 없으면
  조용히 무시(현재의 "실재 확인" 로직이 UUID 기준으로 정확해진다).

### 완료 기준

- 단위: 워커가 spawn/복원/kill에서 UUID 맵을 정확히 유지, MuxSnapshot에 UUID가 실림.
- e2e(web-remote): UUID로 watch → 시청, **다른 워크스페이스 UUID로 watch → no-op**,
  워커 재시작 후 같은 UUID로 재시청 성공(auto re-watch 신뢰성).
- 회귀: postcard wire 바이트 호환(remote.rs 원격 데스크톱 경로 무영향) — 기존 테스트.

### wire 호환 (검토에서 정정한 항목 — 초안의 오판)

초안은 "PaneSnapshot 끝에 필드 append면 wire 안전"이라고 썼으나 **틀렸다**. postcard
구조체는 태그 없는 순차 인코딩이라 필드를 추가하면 **모든 MuxUpdated 메시지의 바이트가
바뀐다** — enum variant append(기존 discriminant 보존)와 성질이 다르다.

다만 실제로는 문제가 되지 않는다: 원격 접속은 **PROTO_VERSION 정확 일치일 때만 수립**
된다(remote.rs:326/1270 — 불일치 hello는 즉시 거부). 즉 원격 양단은 언제나 같은 빌드다.
따라서 올바른 절차는:

- PaneSnapshot에 `persistent_id: Option<String>` 추가 (구조체 끝).
- **`PROTO_VERSION` 5 → 6으로 증가** (protocol.rs:29). 구버전 원격 클라는 조용히
  오작동하는 대신 hello 단계에서 명확히 거부된다 — fail-fast가 옳다.
- 기존 postcard 라운드트립 테스트에 persistent_id가 실리는지 추가.

### 그 외 리스크

- 복원 경로에서 UUID가 없는 세션(레거시 행/폴백 spawn)이 있을 수 있다 → Option으로 두고
  UUID 없는 세션은 폰에서 표시 전용(시청 불가)으로 강등. 조용한 오동작보다 명시적 제한.
- MuxUpdated는 고빈도 이벤트다. pane당 String이 늘어 클론 비용이 커진다 →
  `Option<Arc<str>>` 또는 pane_id처럼 이미 String인 필드와 같은 취급으로 두고,
  §14 프레임 예산(p95)에 회귀가 없는지 perf 하네스로 확인한다.

---

## PR-I2 — 승인 ↔ 세션 연결 (I1 뒤)

### 목표

승인 카드에서 "**어느 세션의 승인인지**" 보이고, 그 세션 화면을 바로 열 수 있다
(P6c에서 데이터가 없어 미배선한 "화면 보기").

### 범위

```text
crates/storage: 마이그레이션 +1 — pending_approvals.pane_id TEXT (NULL 허용)
  (+ 선택: session_uuid TEXT — 아래 결정 참조)
crates/mcp-store: PendingApprovalInsert/PendingApprovalRow에 필드 추가
crates/mcp-proxy: hook.rs가 env DEPPY_SESSION_ID(=pane_id)를 읽어 insert에 싣는다
crates/app: 승인 UI(approvals.rs)에 세션명 표시
crates/web-remote: ApprovalView에 session(UUID) + title, 폰 승인 카드에 "화면 보기"
```

### 설계 결정: pane_id인가 session UUID인가

- proxy가 **이미 갖고 있는 것은 pane_id**다(DEPPY_SESSION_ID). 세션 UUID는 모른다.
- pane_id → session UUID는 **mux_panes.session_id**로 조인하면 나온다(DB에 이미 있음).
- 따라서: **저장은 pane_id**(proxy가 아는 것을 그대로, 추가 조회 없이), **표시/딥링크는
  조인해서 UUID/제목**을 얻는다. 프록시에 DB 조회를 새로 넣지 않는다(fail-closed 원칙:
  승인 등록이 조회 실패로 막히면 안 된다).
- pane_id가 없는 승인(레거시 행, env 미주입 경로)은 NULL → UI에서 "세션 불명"으로 표시,
  "화면 보기" 버튼 없음.

### fail-closed 계약 (검토에서 확인 — 반드시 보존)

`hook.rs:148`의 승인 등록은 **실패 시 Deny**로 떨어진다("승인 요청 등록 실패 — 안전을
위해 거부됨", 감사 기록 포함). pane_id 컬럼 추가로 이 경로에 **새로운 실패 지점을
만들면 안 된다**:

- pane_id는 이미 프로세스 env에 있는 값이므로 **조회 없이** 그대로 싣는다(DB 조인/쿼리
  추가 금지 — 조회 실패가 곧 도구 실행 차단이 된다).
- env가 없으면 `None`을 싣고 **등록은 성공시킨다**(승인 자체를 막지 않는다).
- 즉 이 PR은 등록 경로의 실패 가능성을 **늘리지 않는다**. 회귀 테스트로 고정.

### 구현 요점

- 마이그레이션은 **ADD COLUMN NULL 허용**(파괴적 변경 없음, 롤백은 컬럼 무시).
- `list_pending_approvals`가 mux_panes/sessions와 LEFT JOIN해 (pane_id, session_uuid,
  title)을 함께 반환. 승인 폴링은 접속 시 1초 주기 — 조인 비용은 무시할 수준(행 수 ≤ 수십).
- 데스크톱 승인 다이얼로그에도 세션명을 표시한다(현재는 server/tool만 — H3 리뷰의
  "어느 세션이 요청했는지 모른다" 지적과 같은 뿌리).
- 폰: 승인 카드에 세션명 + "화면 보기"(그 UUID로 watch) — P6c에서 남긴 TODO 완결.

### 완료 기준

- 마이그레이션 스모크(기존 DB 전 버전 → 최신, fk_check), 승인 등록 시 pane_id 기록,
  조인 조회가 세션명/UUID를 정확히 반환, pane_id NULL 승인의 graceful 표시.
- e2e: proxy가 올린 승인이 폰 카드에 세션명과 함께 뜨고, "화면 보기"로 그 세션이 열린다.

---

## PR-O1 — serve 온보딩 (독립 — 병렬 가능)

### 목표

새 Mac에서 모바일 웹을 켜면 **앱이 스스로 진단하고 안내/설정**한다. 오늘 사용자가 겪은
과정(serve 미설정 → tailnet 미승인 → 콘솔 승인 → CLI 실행)을 UI가 대신한다.

### 범위

```text
crates/app: tailscale.rs 확장(serve 상태 진단 + 설정 실행), ui/settings.rs(모바일 웹
  페이지에 상태 카드 + 버튼), i18n 5로케일
```

### 구현 요점 (오늘 실측한 CLI 동작에 근거)

- 진단(1회성 스레드, 상주 폴링 없음 — 기존 감지 관례):
  1. `tailscale status --json` → BackendState/DNSName (이미 구현)
  2. `tailscale serve status` → 실측한 출력 형식(2026-07-12, v1.98):
     ```text
     미설정:  "No serve config"
     설정됨:  https://jr-macbookair.tail02799e.ts.net (tailnet only)
              |-- / proxy http://127.0.0.1:8737
     ```
     → 프록시 대상 포트를 파싱해 **현재 bind 포트와 일치하는지**까지 본다(포트가 바뀌면
     "재설정 필요" 상태). 파싱은 `proxy http://127.0.0.1:(\d+)` 한 줄 매칭이면 충분하다.
- 설정 실행: `tailscale serve --bg <port>` (사용자 클릭 시에만).
  - **실패 케이스**: "Serve is not enabled on your tailnet."이 오면 **승인 URL을 안내**
    (출력에 `https://login.tailscale.com/f/serve?node=...` 포함) — 버튼으로 브라우저 열기.
    이것이 오늘 막혔던 지점이고, CLI가 URL을 주므로 파싱해 그대로 쓴다.
- 상태 카드 4상태: `CLI 없음` / `미로그인·MagicDNS off` / `serve 미설정(설정 버튼)` /
  `정상(URL 표시)`. 각 상태에 **다음 한 걸음**만 보여준다.
- 권한: CLI 실행은 사용자 클릭에서만. 자동 실행 금지(계획 v3.3 P1의 "CLI 자동 실행 안 함"
  결정을 **버튼 명시 동의**로 완화 — 문서에 근거 기록).

### 완료 기준

- 단위: `serve status` 출력 파서(미설정/설정됨/포트 불일치), 승인 URL 추출, 상태 머신.
- 실기기: 새로(또는 serve off 상태로) 켠 Mac에서 버튼만으로 폰 접속까지 도달.

---

## 순서·리스크 요약

```text
O1 (독립, 소형)  ─┐
                  ├─ 병렬 가능
I1 (근본, 중형) ──┴─→ I2 (I1의 UUID를 씀)
```

- **I1이 가장 크고 위험하다**: postcard wire(원격 데스크톱)를 지나가므로 append-only
  규칙을 어기면 원격 클라가 깨진다. 기존 wire 호환 테스트를 먼저 확인하고 시작한다.
- I2는 마이그레이션이 있지만 ADD COLUMN NULL이라 안전하다. proxy 변경은 fail-closed
  경로라 신중히(승인 등록 실패 = 도구 실행 차단이 아니라 **승인 없이 진행되면 안 됨**).
- O1은 CLI 문자열 파싱 의존 — Tailscale 버전에 따라 출력이 바뀔 수 있다. 파싱 실패는
  "진단 불가"로 폴백하고 기존 문서 안내를 남긴다(하드 실패 금지).
```
