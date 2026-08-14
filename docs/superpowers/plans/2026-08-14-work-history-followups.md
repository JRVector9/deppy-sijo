# 작업 이력 후속 — 결함 수정과 orca 격차 보완 PR 계획

## 문서 목적

2026-08-14 pane-tab 전환 구현 중·후에 **코드와 실제 DB로 확인한** 결함과, orca
AI Vault 대비 우리 쪽에 없는 기능을 PR 단위로 쪼갠 실행 계획이다. 추측은 배제하고,
각 항목에 확인 방법과 근거 위치를 남긴다.

기준 브랜치는 `main`, 원격 기준 커밋은 `8489527`. pane-tab 구현은 아직 **미커밋**
상태로 작업 트리에 있다.

## A. 확인된 결함

### A1. 세션 없는 워크스페이스에서 이력이 아예 뜨지 않는다 — High

핸드오프에서 잠근 기본값 3번은 「활성 세션이 없는 workspace에서도 이력 단독 탭을
허용」이다. 현재 구현은 이를 만족하지 않는다.

- 근거: `crates/app/src/ui/workspace.rs:3271`, `:3311`. `show_with_input`은 mux가
  없거나 active tab이 없으면 `show_new_session_prompt`를 그리고
  `WorkspaceSurfaceOutput::default()`로 **조기 반환**한다. 기본값은
  `aux_body_rect: None`, `aux_tab_intent: None`이다.
- 결과: 레일 `이력`을 눌러 `work_history_tab`이 `OpenActive`가 되어도 탭 chrome도
  본문도 렌더되지 않고 「새 셸」 프롬프트만 남는다. 레일은 켜져 있는데 화면은
  아무 반응이 없어 고장으로 읽힌다.
- 같은 원인의 두 번째 경로: `mux.focused_pane`이 `None`이면 어떤 pane도
  `focused == true`가 아니라 보조 탭이 붙을 곳이 없다(`render_pane_header`는
  `focused && aux_tab.is_some()`에서만 탭을 만든다).

수정 방향: 두 조기 반환 지점과 focused pane 부재 경로에서, 보조 탭이 활성이면
`show_new_session_prompt` 대신 **세션 없는 탭 스트립 + 본문 rect**를 만들어
돌려준다. 세션 탭 자리에는 기존 「새 셸」 진입점을 남긴다(세션이 없다는 사실과
새로 만드는 길이 동시에 보여야 한다). 이력 데이터는 workspace-scoped라 세션이
없어도 유효하다.

RED 테스트: mux 없는 `WorkspaceUi`에 활성 보조 탭을 넣고 `show_with_input`이
`aux_body_rect`를 돌려주는지, `focused_pane` 없는 mux에서도 같은지.

### A2. branch·변경 수가 세션당 카드 한 장에만 붙는다 — Medium

- 근거: `crates/app/src/app.rs:11963`. `first_observed_cwd`는 `index == 0`(최근
  창의 최신 턴)일 때만 세팅되고, 나머지 턴은 `None`으로 `detected_work_history_facts`에
  들어가 cwd/branch/변경 수가 영원히 비어 있다.
- 실제 DB 확인(2026-08-14 조회, 7행):

  | agent_session_id | kind | model | instruction | cwd 있음 |
  |---|---|---|---|---|
  | 78ae365e-7 | claude | Opus 5 (1M context) | ㅂㅈㄷ… | ✅ |
  | 78ae365e-7 | claude | Opus 5 (1M context) | ㅁㅂㅁㄴㅇㅁ | ❌ |
  | session_92 | kimi | kimi-code/k3 | 어떻게 해야해? | ✅ |
  | session_92 | kimi | kimi-code/k3 | 머했는지 기억해? | ❌ |
  | session_92 | kimi | kimi-code/k3 | 사용자들이 스킬을… | ❌ |
  | session_92 | kimi | kimi-code/k3 | 프로젝트 배포는… | ❌ |
  | session_92 | kimi | kimi-code/k3 | 웹 띄어봐 | ❌ |

  model/effort는 **모든 행에 있다**. 비어 있는 것은 cwd 파생 사실(branch, 변경 수)뿐이다.

- 데이터 규칙 자체는 옳다. 오래된 턴의 cwd는 알 수 없고(세션이 `cd`했을 수 있다),
  현재 cwd를 과거 턴에 소급 각인하는 것은 `0e934c0`에서 고친 High 결함이다.
  **바꿔야 하는 것은 표시 방식이지 데이터가 아니다.**

수정 방향: B1(세션 그룹핑)에서 세션 컨텍스트를 그룹 헤더로 올려 한 번만 보여준다.
그룹 헤더의 branch는 그 세션에서 관측된 최신 값이고, 카드마다 붙였다 안 붙었다
하는 비일관이 사라진다. 데이터 계약은 건드리지 않는다.

### A3. 탭을 키보드로 선택할 수 없다 — Low, 알려진 한계

세션 탭·세션 `×`·도구·새 이력 탭 모두 `ui::interact` 기반 포인터 전용이다.
이력 탭만 focusable로 만들면 같은 행 안에서 규칙이 어긋난다. pane 헤더 전체를
키보드 대응하는 별도 작업으로 남긴다. 이번 계획의 범위 밖.

### A4. (수정 완료, 미커밋) 열지 않은 이력 탭이 항상 보였다

`set_aux_tab`을 탭의 *활성* 여부로만 넘기고 *열림* 여부를 보지 않아, 한 번도 열지
않았는데 헤더에 `이력 ×`가 떠 있었다. 그 상태면 이력 `×`가 아무것도 하지 않아
완료 기준(「History X는 UI tab만 닫는다」)이 깨진다. `work_history_tab.is_open()`
게이트로 수정하고 상태 테스트·source-law 테스트에 고정했다.

## B. orca AI Vault 대비 격차

orca의 에이전트 이력은 `src/shared/ai-vault-types.ts` + `src/main/ai-vault/*`이며,
터미널 스크롤백 이력(`terminal-history-*`)·git 이력(`git-history-*`)과는 별개다.

우리와 orca는 **입도**(우리=지시 턴, orca=세션 파일), **출처**(우리=앱이 띄운 pane
관측 기록, orca=파일시스템 스캔), **지속성**(우리=SQLite durable, orca=60초 메모리
캐시)이 근본적으로 다르다. 따라서 격차를 전부 메우는 것은 목표가 아니다.

### 채택 후보

| # | 항목 | orca 근거 | 우리 비용 | 비고 |
|---|---|---|---|---|
| B1 | 세션 단위 그룹핑 | `groupAiVaultSessions(… 'folder'\|'project'\|'agent')` | 낮음 | A2를 함께 해소 |
| B2 | provider 다중 선택 필터 | `AI_VAULT_AGENTS` 17종 토글 | 낮음 | 우리는 3종 |
| B3 | 정렬 선택 | `AiVaultSort = 'updated' \| 'created'` | 낮음 | 지금은 상태순 고정 |
| B4 | 지시문/요약 복사 | `aiVault.getFirstUserPrompt` 온디맨드 재파싱 | 낮음 | 우리는 이미 전문 보유, 재파싱 불필요 |
| B5 | 세션당 턴 수 표시 | `messageCount` | 낮음 | 우리는 rows 카운트로 충분 |
| B6 | 스코프 3단(workspace/project/all) | `AiVaultScope` | **높음** | projection·저장 계약 변경, 별도 설계 |

### 비채택 (이유 명시)

- **앱 밖 세션 파일시스템 스캔** — orca AI Vault의 근간이지만 제품 방향 자체가
  다르다. 도입하면 우리 이력의 의미(「이 앱이 관측한 작업」)가 바뀌고, 라이브
  상태·「현재 세션으로 이동」이 성립하지 않는 행이 섞인다. 하려면 별개 기능이다.
- **세션 삭제** — 우리는 transcript 소유자가 아니다. orca도 저장 레이아웃이 안전한
  9종만 허용하고 codex·kimi·antigravity·opencode는 의도적으로 제외한다.
- **서브에이전트 transcript** — Claude Task 전용이고 우리 detector 범위 밖이다.
- **토큰 총량** — transcript 파서 확장이 필요한데, 우리는 이미 context %를 사이드바에
  표시한다. 가치 대비 비용이 높다.

## C. PR 단위

작업 흐름은 확립된 관례를 따른다 — `git worktree add <path> -b <branch> origin/main`
으로 격리, PR당 단일 커밋(리뷰 반영은 amend), `gh pr merge --rebase`, 머지 후
브랜치·worktree 즉시 정리.

### PR 1 — 이력을 세션 pane 보조 탭으로 (A1 + A4 포함)

현재 미커밋 작업 + A1 수정. 기능 하나가 완성 조건을 만족한 상태로 들어가야 하므로
A1을 여기 포함한다.

- 범위: `app.rs`, `ui/workspace.rs`, `ui/work_history.rs`, `ui/agent_terminal.rs`,
  `ui/file_tree.rs`, 5개 locale
- 추가 작업: A1의 RED 테스트 2건 + 세션 없는 워크스페이스 탭 스트립
- 이미 통과한 게이트: workspace 139/139, work_history 18/18, 전체 1,489 + 통합
  4/5/14/15, clippy·check·fmt·diff·i18n-check 0

### PR 2 — 세션 그룹 헤더와 카드 재구성 (A2 + B1 + B5)

- `agent_session_id`로 그룹, 그룹 헤더에 provider·model·effort·branch·변경 수·최신
  시각·턴 수
- 턴 카드는 지시문·요약·상태·시각만 남긴다
- 그룹 접기/펴기, 단일 vertical ScrollArea 유지
- 저장·projection 계약 변경 없음(순수 UI leaf 재구성)

### PR 3 — 필터와 정렬 (B2 + B3)

- provider 다중 선택 칩(claude/codex/kimi), 상태 필터와 AND 결합
- 정렬 토글: 상태 우선(기본) / 최신순
- 선택 상태는 세션 내 UI state로만 유지(설정 저장 안 함)

### PR 4 — 지시문·요약 복사 (B4)

- 카드 확장 영역에 복사 버튼, 클립보드 바이트 상한 적용
- 기존 clipboard 경로 재사용

### PR 5 — 스코프 확장 (B6) — 착수 전 별도 설계 필요

`work_history_workspace_id`, projection scope, epoch guard, stale 방지 규칙이 전부
현재 워크스페이스 단일 scope 위에 서 있다. 저장 조회·워커 계약·stale 검증을 함께
바꿔야 하므로 설계 문서를 먼저 쓴다. 이번 라운드에서는 착수하지 않는다.

## D. 진행 상황 (2026-08-14 갱신)

| PR | 내용 | 상태 |
|---|---|---|
| #129 | 이력 pane 보조 탭 (A1·A4 포함) | **머지** `dbb11ef` |
| #134 | webbrowser 1.2.4 (B 외 — 게이트 정리) | **머지** `c9754f5`, closes #131 |
| #132 | provider 필터 + 정렬 (B2·B3) | **머지** `3f65ddf` |
| #133 | 지시문·요약 복사 (B4) | **머지** `4e02150` |
| — | 세션 그룹 헤더 (A2·B1·B5) | **미착수** — 아래 참조 |
| — | 스코프 3단 (B6) | 보류 |

착수 전 확정할 것으로 남겼던 세 질문은 전부 확정됐다 — worktree 격리로 진행, PR 1~4를 이번 라운드에 포함, PR 5는 보류.

### 남은 연쇄 — 순차 진행이 강제된다

```
#130 뷰모델 리팩터 (동작 변경 없음)  →  세션 그룹 헤더 (A2 해소)
```

둘 다 `work_history.rs`를 구조적으로 재편해 병렬이 불가능하다. #130을 먼저 독립 PR로 빼면 「동작 변경 없는 리팩터」와 「기능 추가」가 분리되어 리뷰가 기계적이 되고, 통과 시 저장소 전체 게이트가 처음으로 전부 초록이 된다.

### 이번 라운드에서 새로 드러난 것

- **A1이 구현 중 발견됐다** — 세션 없는 워크스페이스에서 이력이 아예 뜨지 않던 High 결함. #129에 포함해 수정했다.
- **세션 이동 10곳이 죽은 클릭이었다** — 적대적 리뷰가 2곳을 지적했고 전수 조사로 10곳으로 확대. `reveal_terminal_session`으로 일원화했다.
- **`check-boundary`의 `db.` 규칙이 주석까지 오탐한다** — #133이 doc 주석에 파일 경로를 쓰는 바람에 위반이 1→2건이 됐다. 주석 표기를 바꿔 되돌렸다. 규칙이 줄 단위 순수 텍스트 매칭이라 앞으로도 같은 함정이 있다.
- **핸드오프의 게이트 목록이 CI보다 좁다** — 자세한 내용은 `docs/CODEX_HANDOFF.md`의 gate-list correction 항목.

## E. 공통 검증

각 PR은 아래를 통과해야 한다.

```bash
cargo test -p deppy-sijo --locked work_history -- --nocapture
cargo test -p deppy-sijo --locked ui::workspace::tests:: -- --nocapture
cargo test -p deppy-sijo --locked
cargo clippy -p deppy-sijo --all-targets --locked -- -D warnings
cargo check -p deppy-sijo --all-targets --locked
cargo fmt --all -- --check
cargo run -p xtask --locked -- i18n-check
git diff --check
```

UI가 바뀌는 PR은 빌드·재기동으로 화면을 먼저 보이고, 게이트는 커밋 직전에 한 번
돌린다(프로젝트 CLAUDE.md 규칙).
