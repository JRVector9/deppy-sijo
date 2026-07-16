# v3.9 — 전역 대기 인박스 (알림 팝오버) PR 계획

작성: 2026-07-17. 기준 커밋: main 983d213.

## 목표

**어느 워크스페이스에 있든, 그 세션·워크스페이스로 이동하지 않고** 승인/입력 대기를 처리한다.
벨(🔔) 하나에서 전역 대기를 보고 인라인으로 승인/거부/y·n/자유입력을 끝낸다.

부수 목표: 알림 확인이 통합 설정 창 전체를 여는 현재 구조(무거움)를 경량 팝오버로 분리한다.

## 배선 조사 결과 (코드 확인 — 2026-07-17)

계획의 전제를 실제 코드로 검증했다.

| 전제 | 확인 결과 | 근거 |
|---|---|---|
| MCP 승인이 전역으로 보인다 | **성립** — `Db::list_pending_approvals()`는 워크스페이스 필터 없는 전역 조회. I2에서 pane_id 컬럼 + sessions/mux_panes LEFT JOIN으로 세션 UUID·제목까지 이미 반환 | `storage/src/db.rs:1412`, `app.rs:4262 poll_pending_approvals` |
| PTY 입력 대기가 전역으로 보인다 | **성립(단, 현재 필터링 중)** — hook 신호는 DB 테이블 `agent_needs_input`에 세션 키 `{workspace_id}:{session_id}`로 저장되고 `list_waiting_sessions()`는 전역 반환. `App::refresh_needs_input`이 **활성 워크스페이스만 남기고 버리는** 중 — 필터를 풀면 전역 확보 | `db.rs:164/967`, `app.rs:1960-1973` |
| warm 워크스페이스에 응답 주입이 된다 | **성립** — warm의 `WorkspaceRuntime.runtime`은 `InProcessRuntimeClient`. App(UI 스레드)에서 `self.warm.get(&id).runtime.send_command(WriteInput)` 직접 호출 가능. web-remote용 `command_sink`(활성 전용) 경로가 아니어도 된다 | `app.rs:855 WorkspaceRuntime`, `app.rs:2431/2467` |

### ⚠ 발견한 제약: warm 워크스페이스의 화면 미리보기

와이어프레임의 PTY 카드는 판단 재료로 **화면 마지막 줄**("Proceed? (y/n)")을 보여준다. 그런데:

- 활성 워크스페이스: `WorkspaceUi`의 세션 `summary`(마지막 비어있지 않은 행)가 Viewport 이벤트로 채워짐 — **있음**
- **warm 워크스페이스: §14.1이 Warm에서 render/snapshot을 금지** → Viewport 없음 → summary **없음**
- suspended: PTY 자체가 없어 대기 상태도 존재하지 않음 (인박스 대상 아님 — 문제 없음)

즉 **"다른 워크스페이스 것을 가지 않고 판단"이라는 핵심 요구가 미리보기 없이는 반쪽**이 된다.

**해결안(PR-N3에서 채택)**: 세션 로그 tail을 읽는다.
- `logs_root/<세션 UUID>/plain.txt`의 마지막 N줄 — 디스크 기반이라 워크스페이스 활성 여부와 무관
- 이미 **redaction이 적용된** 파일이라 secret 유출 위험 없음(§7 계약)
- 세션 키(`{ws}:{u64}`) → UUID 매핑은 승인 조회가 이미 쓰는 `sessions`/`mux_panes` 조인 패턴 재사용
- 대안으로 검토했으나 배제: warm 워커에 스냅샷 요청(§14.1 위반), P5a lease 승격(과함 — 인박스는 1회 조회지 스트림이 아님)

---

## PR 단위

원칙: 각 PR은 **단독으로 머지 가능하고 앱이 동작**해야 한다. 순서 의존만 있고 기능 의존은 최소화.

### PR-N1 — 벨 팝오버 컨테이너 (알림을 설정 창에서 분리)

**범위**
- 상단바 벨 버튼에 앵커된 경량 팝오버(egui Window, `settings_open`과 독립). 밖 클릭 시 닫힘
- 팝오버 내용: 「최근 알림 N개」 + 「전체 보기 →」(기존 설정→알림 카테고리로 연결)
- 뱃지: 현재 미확인 알림 수 유지 (대기 수 우선순위는 N2에서)
- `⌘⇧U`(ShortcutAction::OpenNotifications) 재배선: 설정 창 열기 → 팝오버 토글
- 기존 설정→알림 카테고리는 **그대로 유지**(전체 기록 열람용)

**파일**: `app.rs`(팝오버 상태·렌더·단축키 디스패치), `ui/notifications.rs`(팝오버용 compact 렌더 추가), i18n×5

**리스크**: 낮음. 기존 알림 데이터 구조 재사용, 새 상태는 `notifications_popover_open: bool` 하나
**검증**: 벨/⌘⇧U로 팝오버 토글, 설정 창은 안 뜸, 「전체 보기」가 기존 화면으로 연결
**크기**: 중 (~150줄)

### PR-N2 — MCP 승인 카드 (인라인 승인/거부)

**범위**
- 팝오버 상단에 「대기 중」 섹션 + 승인 카드: 워크스페이스/세션 제목, 도구명, redacted 인자 미리보기, [승인] [거부] [이동→]
- 결정은 기존 경로 그대로: `Db::resolve_approval`(first-writer-wins) — 폰 대시보드·모달과 충돌 없음
- 「이동→」: 해당 워크스페이스로 전환 + pane 포커스 (기존 switch_workspace + FocusPane 재사용)
- 뱃지 우선순위: 대기 수 > 미확인 알림 수
- 카드 데이터는 이미 전역인 `poll_pending_approvals` 결과 재사용 — **새 폴링 없음**

**의존**: PR-N1 (팝오버 컨테이너)
**파일**: `app.rs`, `ui/notifications.rs`, i18n×5
**리스크**: 낮음 — 데이터·결정 경로가 전부 기존 것. UI 조립만
**검증**: MCP proxy 경유 에이전트로 실제 승인 발생 → 다른 워크스페이스에서 인박스로 승인 → 그 워크스페이스 전환 없이 진행되는지 실측
**크기**: 중 (~200줄)

### PR-N3 — PTY 입력 대기 카드 (핵심 PR)

**범위**
- `refresh_needs_input`의 활성 워크스페이스 필터 해제 → 전역 대기 맵(`{ws_id, session_id}` → 대기)
- 세션 키 → (워크스페이스명, 세션 제목, 세션 UUID) 해석 (승인 조회의 조인 패턴 재사용)
- **미리보기**: 활성 워크스페이스는 기존 summary, 그 외는 `plain.txt` tail 3줄 (redacted, 4KB 상한 읽기)
  - 위 「발견한 제약」 참조. tail 읽기는 UI 스레드 금지 — **백그라운드 1회 조회 + 캐시**(hover cwd 해석과 같은 stale-while-revalidate 관례)
- 카드: [y] [n] [자유 입력 ↵] [이동→]
- 응답 주입: 활성이면 `self.active.runtime`, warm이면 `self.warm.get(&ws).runtime`에 `WriteInput` 직접 전송
- 안전장치: 주입 전 대기 상태 재확인(stale 카드가 엉뚱한 입력을 넣지 않게 — I1 "모르는 세션이면 명령 미생성" 원칙 준수)

**의존**: PR-N1, PR-N2 (섹션 UI)
**파일**: `app.rs`, `ui/notifications.rs`, `storage/src/db.rs`(세션 키 조인 쿼리 1개 추가 가능성), i18n×5
**리스크**: **중** — 유일하게 새 데이터 경로(로그 tail)를 만든다. 미리보기 없이도 카드는 동작하므로, tail이 실패하면 미리보기만 비고 응답 버튼은 살아 있게 폴백
**검증**: 다른 워크스페이스의 claude에 y/n 프롬프트 띄우고 → 인박스에서 y → 전환 없이 진행 실측. warm 상태(창 최소화)에서도 동일 확인
**크기**: 대 (~350줄)

### PR-N4 — 승인 모달 정리 + 마감

**범위**
- 기존 중앙 승인 모달 **표시만 끔**(코드·`ApprovalsUi` 보존 — 되돌리기 쉽게). 설정 토글 없이 인박스 일원화
  - 근거: 모달은 MCP proxy 경유 에이전트에서만 뜨는 저빈도 경로고, 강제 개입 역할은 뱃지+OS 알림이 대체
- 알림 클릭(OS 알림 → 앱)이 인박스를 열도록 배선(현재는 해당 pane 포커스)
- 문서 갱신: 이 계획서 + 설계문서 §알림/승인 섹션

**의존**: PR-N2, PR-N3
**리스크**: 낮음 (표시 토글)
**크기**: 소 (~80줄)

---

## 순서와 병렬화

```
PR-N1 (팝오버 컨테이너)          ← 단독 선행 필수 (다른 PR의 컨테이너)
   ├─ PR-N2 (MCP 승인 카드)      ← N1 후 병렬 가능
   └─ PR-N3 (PTY 대기 카드)      ← N1 후 병렬 가능 (N2와 파일 충돌 주의: 둘 다 notifications.rs)
        └─ PR-N4 (모달 정리)     ← N2·N3 후
```

**서브에이전트 병렬 시 충돌 방지**: N2·N3가 `ui/notifications.rs`를 함께 건드린다.
→ N1에서 **섹션 렌더 함수를 미리 분리**(`render_waiting_section` / `render_recent_section` 스텁)하고,
   N2는 승인 카드 함수만, N3는 PTY 카드 함수만 채우는 방식으로 파일 내 영역을 나눈다.
   i18n은 키 prefix로 분리(`inbox.approval.*` / `inbox.waiting.*`).

## 진행 결과 (2026-07-17 — 4개 PR 전부 완료·푸시)

| PR | 커밋 | 결과 |
|---|---|---|
| N1 | `06a18c2` | 벨 버튼 신설(뱃지 이관) + 팝오버(설정 창 분리) + ⌘⇧U 재배선. **착수 시 발견**: 벨이 아예 없었고 unread가 「설정」 라벨에 붙어 있었다 → 범위가 "벨 신설"로 넓어짐 |
| N2 | `2870f96` | MCP 승인 카드(워크스페이스명·도구·인자 미리보기·승인/거부/이동). 새 폴링 없이 기존 approval-watcher 데이터 재사용 |
| N3 | `29149b4` | PTY 대기 카드(미리보기 3줄·y/n·자유 입력·이동). 전역 대기 수집 + warm 직접 주입 + stale 재확인 |
| N4 | `79c7503` | 중앙 승인 모달 호출 중단(코드는 allow(dead_code)로 보존 — 되살리기 1줄) |

### 계획 대비 달라진 것 (중요)

**① 승인 조인이 프로덕션에서 항상 실패한다 (기존 버그 발견)**
계획서는 "PR-N3의 세션 키 해석에 `list_pending_approvals`의 조인 패턴을 재사용"을 전제했으나, 실제 DB로 검증한 결과 **그 조인은 절대 매칭되지 않는다**:
- `pending_approvals.pane_id` ← `DEPPY_SESSION_ID` ← `in_process.rs::session_key()` = `{workspace_id}:{u64}`
  (실증: `agent_needs_input.session_key` = `315f68b6-...-...:2`)
- `mux_panes.id` = `MuxPaneId::new()` = 순수 UUID (실증: `130d9017-be25-...`)
- → `LEFT JOIN mux_panes p ON p.id = a.pane_id`는 서로 다른 식별자 공간을 비교 → `session_uuid`/`session_title`이 **프로덕션에서 항상 NULL**

즉 I2가 의도한 "승인↔세션 연결"(폰 딥링크·승인 팝업 세션명)이 실제로는 동작하지 않는다. 유닛 테스트는 양쪽에 같은 가짜 문자열을 넣어 통과할 뿐이다.
**대응**: N2·N3 모두 조인 대신 `pane_id`/세션 키 문자열을 파싱해 workspace_id를 얻고, 세션 제목은 메모리(`WorkspaceRuntime.session_titles`)에서 해석. DB는 건드리지 않았다.
**잔여**: 근본 수정(조인 제거 또는 pane_id 의미 정정)은 별도 과제 — 아래 후속 참조.

**② 미리보기 UUID는 이미 있었다**
계획서는 "u64→UUID 매핑을 App에서 구할 수 있는지 불명"으로 뒀으나, `PaneSnapshot.persistent_session_id`(v3.7 I1이 "경계를 넘는 식별자·알림 딥링크용"으로 이미 넣어둔 필드)가 mux 스냅샷에 실려 온다. warm 세션의 로그 경로를 이걸로 찾는다. mux가 stale이면 미리보기만 생략(폴백).

**③ 통합 시 수정한 것 (오케스트레이터 검수)**
- N3의 `build_active_waiting_card`가 카드마다 `session_entries()`(mux 전체 순회)를 호출 → 팝오버 열린 동안 매 프레임 × 카드 수 재구성. **1회만 만들어 공유하도록 수정**.
- N2·N3의 벨 뱃지 충돌 → 「대기 중」 = 승인 + PTY 합산으로 병합(둘 다 이미 폴링된 값이라 새 조회 없음).

### 남은 검증 (실사용 필요 — 자동화 불가)
- MCP proxy 경유 에이전트로 실제 승인 발생 → 다른 워크스페이스에서 인박스로 승인 → 전환 없이 진행되는지
- claude/codex y/n·번호 프롬프트 → 인박스 자유 입력으로 응답 → 전환 없이 진행되는지 (warm 상태 포함)
- idle CPU 0.0%p 유지 / 팝오버 오픈 시 메모리 델타 (`DEPPY_RESOURCE_STATS=1`)

## 비범위 (이번 계획 밖)

- 에이전트 메뉴 시나리오 ①~⑧ (선택→보내기·프리셋·브로드캐스트 등) — 별도 웨이브
- regex 기반 대기 감지의 인박스 노출 — 1차는 **hook 신호만**(오탐 0). 커버리지 확장은 실사용 후 판단
- 폰(PWA) 인박스 — 폰은 이미 승인 대시보드가 있음. PTY 대기 카드의 폰 이식은 후속

## 확정 사항 (2026-07-17 사용자)

1. **자유 입력칸 1차 포함** — PTY 카드에 y/n 버튼 + 한 줄 입력(`2↵` 같은 메뉴 번호·짧은 답)을 함께 넣는다. claude/codex의 번호 선택 프롬프트가 y/n만으로는 처리 불가하므로 인박스의 실효를 위해 필수. 입력은 개행 포함 1줄로 주입하고, 전송 전 대기 상태를 재확인한다(stale 방어).
2. **미리보기 줄 수** — tail 3줄로 시작. 실사용 후 조정(설정화는 하지 않는다 — 상수).

## 상단바 현황 (N1 착수 시 확인, 2026-07-17)

계획 수립 시 가정과 달랐던 점: **상단바에 벨 버튼이 없다.** 미확인 알림 수는 「설정」 버튼 라벨에
`설정 (3)` 형태로 얹혀 있고(app.rs:4696-4706), 알림 화면은 통합 설정 창의 좌측 네비 카테고리다.
→ N1은 "기존 벨에 팝오버 붙이기"가 아니라 **벨 버튼 신설 + 뱃지 이관 + 팝오버**가 범위다.

사용 API(egui 0.35 확인): `Popup::from_response(&resp).id(..).open_memory(SetOpenCommand::Toggle)`
+ `close_behavior(CloseOnClickOutside)`, 단축키 토글은 `Popup::toggle_id(ctx, id)`.
