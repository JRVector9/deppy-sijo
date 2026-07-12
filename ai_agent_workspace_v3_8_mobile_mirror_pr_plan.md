# v3.8 — 모바일 미러 · 대기/절전 상태표기 · 이어서 작업 PR 계획

폰에서 비활성 워크스페이스에 **들어가 작업을 이어서** 하기 위한 작업(I1b). I1(세션
identity 영속화)이 토대를 놨고, 이번엔 그 위에서 **워크스페이스 전환(미러)** 과 **상태
구분(대기/절전)** 을 완성한다.

**확정된 결정** (2026-07-12 사용자):
- 미러 방식 = **하드 미러** (폰이 X로 진입하면 데스크탑 active도 X로 전환)
- 초기 스코프 = **I1b-1(상태표기) + I1b-2(대기 미러 진입)**. 절전 깨우기 다듬기(I1b-3)는 후속.

---

## 0. 조사로 확인한 현재 구조 (설계의 근거 — 추측 아님)

```text
- switch_workspace(id) (app.rs:2292)가 이미 미러 진입에 필요한 걸 거의 다 한다:
  · target == active면 no-op
  · projected_live_warm_count > MAX_LIVE_WARM(4)이면 warm_limit_warning 세팅 후 전환 거부
    (= "워크스페이스 많을 때" 안전장치가 이미 있음)
  · warm 풀에 있으면 warm.remove로 재사용(대기 진입 = burst 없음),
    없으면 make_runtime + (하류) RestoreWorkspace + auto_resume_agents로 --resume(절전 깨우기)
- has_live_sessions() (app.rs:904, 순수함수 workspace_is_live): tracker_live || pending_spawns>0
  || (첫 MuxUpdated 전 10초 유예). live면 evict_warm/evict_idle_warm 모두 건너뜀
  → 에이전트/셸이 살아있는 warm은 MAX_WARM(2)·30분 무시하고 무기한 유지(= "대기").
- 웹 프레임 상태는 3개뿐: WorkspaceState::{Active,Warm,Idle} (dashboard.rs:193-195).
  web_workspace_seed(app.rs:2150): active.id→Active, self.warm.get→Warm, else→Idle.
  → 진짜 절전(워커 종료·에이전트 kill)과 미개봉이 똑같이 "유휴(idle)"로 뭉뚱그려짐.
    이게 사용자가 지적한 문제: 살아있는 대기 워크스페이스가 "유휴"로 보임.
- 웹→앱 채널이 없다: bridge는 CommandSink=Arc<dyn Fn(RuntimeCommand)>(워커레벨) +
  wake()=Arc<dyn Fn()>(이벤트루프 킥)만 보유(dashboard.rs:48,467). 워크스페이스 전환은
  app 레벨(switch_workspace)이라 새 채널이 필요하다.
- 워크스페이스 id는 이미 폰에 전달됨: WorkspaceSeed.id = ws.id.clone()(app.rs:2143,2172).
  세션 u64와 달리 workspace id는 안정 문자열이라 별도 IdMap 불필요 — 그대로 되돌려보내면 됨.
- 에이전트 resume 배관 존재: agent_transcript.rs(claude --resume <sid> / codex resume <sid>),
  config.auto_resume_agents(기본 true), app.rs:1832-1862 auto-resume + resumed_panes 추적.
- bridge는 single-source: subscribe_with_wake_background로 단 하나의 active 워커만 구독.
  → active가 바뀌면 폰·데스크탑이 자동으로 같은 화면(미러). 새 워커 안 만듦 = 자원 불변.
```

### 0.1 이 작업이 푸는 증상

| 증상 | 근본 원인 | 해결 |
|---|---|---|
| 살아있는 대기 워크스페이스가 폰에서 "유휴"로 보임 | 프레임 상태가 대기/절전을 구분 못 함 | I1b-1 |
| 폰에서 비활성 워크스페이스 세션에 **진입/이어서 작업 불가** | 웹→앱 전환 채널 없음 | I1b-2 |
| 상한 초과 경고가 데스크탑만 뜨고 폰은 모름 | warm_limit_warning이 폰에 안 실림 | I1b-2 |

### 0.2 자원 불변 논증 (사용자 확인 완료)

steady-state = active 1 + warm ≤ MAX_LIVE_WARM(4), 불변. 미러는 **새 워커를 안 만들고
active 대상을 바꿀 뿐**이다. 대기 진입은 warm 재사용(burst 0), 절전 깨우기만 워커 생성
1회(데스크탑에서 직접 여는 것과 동일). 폰이 5번째 live 워커를 깨우려 하면 switch_workspace가
이미 거부한다 → 무한 누적 없음.

---

## PR-I1b-1 — 상태 재분류 & 표기 (중형, DB 무관)

### 목표

"유휴" 한 덩어리를 **활성 / 대기 / 절전**으로 쪼개 폰·데스크탑에 의미 있게 표시한다.

### 설계 결정

- **A안(채택): 기존 데이터로 파생**. 새 상태를 저장하지 않고 이미 있는 신호로 분류한다:
  · active.id == id → **Active(활성)**
  · self.warm.get(id) 존재 → **Standby(대기)** (warm은 사실상 live이거나 곧 정리되는 과도)
  · else + persisted_activity_panes에 이력 有 → **Suspended(절전)**
  · else + 이력 無 → **Fresh(새 워크스페이스)** — 폰에선 숨기거나 별도 표기
  - 장점: 저장/마이그레이션 없음, 판정 로직만 추가.
- **B안(기각): 상태를 DB/런타임에 영속**. suspend 시점을 기록. 파생으로 충분한데 상태를
  이중 관리하게 됨 — 정합성 부채.

### 범위

```text
crates/web-remote: dashboard.rs(WorkspaceState에 Standby/Suspended/Fresh 추가,
  as_str "standby"/"suspended"/"fresh"), protocol.rs(WorkspaceView 문자열 노출)
crates/app: app.rs web_workspace_seed의 Idle 분기를 persisted_activity_panes 유무로
  Suspended/Fresh 분기, Warm→Standby 매핑. 활동 패널(ActivityWorkspaceState)도 동일 규칙 적용.
crates/i18n: 활성/대기/절전/새 워크스페이스 라벨(ko-KR; 활동 패널·설정에서 쓰는 것)
crates/web-remote/assets: app.js renderWorkspaces 그룹 헤더 색/라벨, app.css 배지 스타일
```

### 검증

- 유닛: warm 워크스페이스 → Standby, 워커 없음+이력 有 → Suspended, 이력 無 → Fresh
- 수동: 데스크탑에서 워크스페이스 전환 후 폰 새로고침 → 직전 것이 "대기"로,
  오래전 닫은 것이 "절전"으로 구분돼 보임

---

## PR-I1b-2 — 대기 미러 진입 (근본, 배관)

### 목표

폰에서 대기(및 절전) 워크스페이스를 탭 → **데스크탑 active를 그 워크스페이스로 전환** →
single-source bridge가 자동으로 미러. 입력은 기존 경로(active 워커) 그대로.

### 설계 결정

- **A안(채택): 웹→앱 switch_sink 신설, switch_workspace 재사용**. bridge에
  `switch_sink: Arc<dyn Fn(String) + Send + Sync>` 추가. 폰이 `{type:"switch",
  workspace:"<id>"}` 보내면 sink가 앱 큐에 넣고 wake(). 앱이 ui()에서 드레인 →
  switch_workspace(id). 대기=재사용/절전=재생성/상한초과=거부를 switch_workspace가 이미 처리.
  - 장점: 미러/깨우기/상한이 전부 검증된 한 함수로 수렴. 새 렌더 소스 없음(자원 불변).
    workspace id는 이미 폰이 가짐 → 세션 IdMap 같은 매핑 불필요.
  - 단점: 하드 미러라 데스크탑 사용자가 보던 화면이 폰 전환에 따라감(확정된 트레이드오프).
- **B안(기각): 멀티소스 bridge(폰만 별도 뷰)**. 데스크탑 화면 안 건드림. 그러나 워커 2개
  동시 스냅샷 = 자원 증가 + u64 IdMap 워커 간 충돌 처리. 사용자가 자원 증가를 명시적으로 거부.
- **C안(기각): P5a SetRemoteViewing lease로 warm peek**. lease는 "훔쳐보기"용이지
  입력·이어서작업 경로가 아님. 입력을 그 워커로 보내려면 결국 active 전환이 필요.

### 범위

```text
crates/web-remote: dashboard.rs(Bridge에 switch_sink 필드 + set_switch_sink, WS 메시지
  파서에 {type:"switch", workspace} 분기 → switch_sink 호출), lib.rs/서버 배선,
  프레임에 warm_limit_warning 전달용 필드(예: WorkspaceFrame.notice: Option<String>)
crates/app: app.rs start_web에서 switch_sink 주입(전환요청 큐 push + egui_ctx.request_repaint),
  ui()에서 큐 드레인 → self.switch_workspace(&id). warm_limit_warning을 웹 프레임에도 실어보냄.
crates/web-remote/assets: app.js 워크스페이스/세션 카드에 "이어서 작업" 버튼 → switch 전송,
  전환 후 미러 화면 자동 표시(기존 active watch 흐름 재사용), notice 토스트(대기 꽉 참) 표시.
```

### 동시성·엣지 주의

- **큐 경유 필수**: switch_sink는 웹 스레드에서 불리므로 switch_workspace를 직접 호출하면
  안 됨(App은 egui 스레드 소유). Mutex<Vec<String>> 큐에 push + request_repaint, ui()에서 처리.
- **중복/경합**: 같은 대상 연타 → switch_workspace가 target==active no-op으로 흡수. 서로 다른
  대상 빠른 전환 → join_pending_shutdown이 이미 window 행 경합을 막음.
- **상한 거부 피드백**: switch_workspace가 warm_limit_warning만 세팅하고 조용히 return하므로,
  폰이 "왜 안 바뀌지"가 되지 않게 그 경고를 프레임 notice로 반드시 폰에 전달.

### 검증

- 수동(실기기): 데스크탑 A 활성 상태에서 폰이 대기 B 탭 → 데스크탑·폰 모두 B로 전환,
  폰에서 입력 → B 워커에 전달돼 이어서 작업됨. A는 대기로 내려감.
- 상한: live 워커 4개인 상태에서 폰이 5번째 진입 → 전환 안 되고 폰에 "대기 꽉 참" notice.
- 회귀: 활성 워크스페이스 세션 watch/입력(기존 경로) 무변경 확인.

---

## PR-I1b-3 — 절전 깨우기 다듬기 (후속, 선택)

switch_workspace의 절전 경로 + auto_resume_agents가 이미 --resume까지 하므로 대부분
동작한다. 남는 마감만:

```text
- 순수 셸(에이전트 없던) 세션 복원 경로 확인(RestoreWorkspace가 셸만 되살리는지)
- resume 실패 시 폰 피드백(전사 파일 없음/깨짐 → "새 세션으로 시작?")
- 깨우는 동안 폰에 "복원 중…" 스피너(첫 MuxUpdated 전 유예창 동안)
- 절전 진입이 잦을 때 warm 상한과의 상호작용 재확인
```

---

## 순서

1. **I1b-1**(상태표기) — 독립, 먼저. 폰에서 대기/절전이 구분돼 보이는 것부터.
2. **I1b-2**(대기 미러 진입) — I1b-1의 상태 위에서 "이어서 작업" 배선.
3. **I1b-3**(절전 깨우기 마감) — 후속.
