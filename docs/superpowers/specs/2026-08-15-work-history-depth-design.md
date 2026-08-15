# 작업 이력 깊이 — 요약 확장 · 메시지 배열 · 원문 보기 설계

날짜: 2026-08-15
상태: 사용자 결정 완료 (1·2·3 전부, 별도 브랜치)
브랜치: `feat/work-history-depth` (워크트리 `/Users/jr/Desktop/projects/deppy-history`)
기준: `feat/git-panel` (`0c1500a`) — 2번이 그 브랜치의 보조 탭·마스터-디테일 기구를 재사용한다.
머지 순서: `feat/git-panel` → 이 브랜치
참조: stablyai/orca (커밋 대조 2026-08-15)

## 문제

작업 이력 카드에 에이전트와 주고받은 내용이 **너무 짧게** 나온다. orca와 코드를 대조해
원인이 세 층으로 갈리는 것을 확인했다.

| | deppy-sijo (현재) | orca |
| --- | --- | --- |
| 글자 수 | `AGENT_SUMMARY_CHARS = 120`, 줄바꿈을 공백으로 접어 **한 줄** | `SESSION_PREVIEW_TEXT_LIMIT = 220` |
| 개수 | 턴당 `instruction` 1개 + `agent_summary` 1개 | `SESSION_PREVIEW_MESSAGE_LIMIT = 5` (role·text·timestamp 배열), 상세에서 3턴 |
| 깊이 | **전체 원문을 볼 수단이 없다** | 「Open log」 → `src/main/native-chat/` 전용 리더·워처가 전체 대화를 증분 렌더 |

핵심은 세 번째다. orca도 preview 자체는 짧다(220자). 차이는 **짧은 요약 옆에 전체를 볼
수단이 있느냐**이고, 우리는 요약이 곧 전부라 "짧다"가 된다.

저장소는 병목이 아니다 — `AGENT_WORK_TURN_INSTRUCTION_BYTES_MAX`/`_SUMMARY_BYTES_MAX`는
각각 32KB인데 지금 120자(≈360바이트)만 쓴다. 병목은 추출기 하나다.

## 유지하는 원칙

`agent_work_turns`(v35) 스키마 주석의 선언을 그대로 지킨다:

> 원문 transcript나 tool payload는 저장하지 않고 bounded 표시 필드만 보존한다.

- **1·3번**은 저장하되 전부 유계다(아래 상한 표).
- **2번은 아무것도 저장하지 않는다** — 볼 때 transcript 파일을 읽고, 닫으면 버린다.
- 폴링 없음. 자동 갱신 없음. 사용자가 열 때만 읽는다(최소 자원 원칙).

## 1. 요약 길이와 줄 보존

**대상:** `crates/app/src/agent_transcript.rs`

```rust
const AGENT_SUMMARY_CHARS: usize = 400;   // 120 → 400
/// 줄바꿈을 보존하되 이 줄 수를 넘기지 않는다. 카드가 세로로 무한정 자라지 않게.
const AGENT_SUMMARY_LINES: usize = 4;
```

`clean_agent_summary`의 규칙을 바꾼다.

- **줄 안**: 연속 공백·탭·제어문자는 지금처럼 한 칸으로 접는다.
- **줄 사이**: `\n`은 **보존**한다(연속 개행은 하나로). 지금은 이것도 공백으로 접혀 한 줄이
  되는 것이 "짧아 보이는" 체감의 절반이다.
- 400자 또는 4줄 중 **먼저 걸리는 쪽**에서 자르고 `…`를 붙인다.
- 노이즈 차단(`<system-reminder>`, `<local-command>`, `<command-name>`, `<task-notification>`
  등 접두 거부)과 `<image …>` 표식 제거는 **그대로 둔다** — 이건 정확도를 올리는 규칙이다.

**렌더** (`crates/app/src/ui/work_history.rs`)

- 접힌 카드: **첫 줄만** 한 줄로 보여준다(기존 `.truncate()` 유지). 목록의 스캔성이 우선이다.
- 펼친 카드: `expanded_text`가 최대 4줄을 그대로 보여준다(줄바꿈 유지).

**소급 적용 범위 (실측으로 확인한 사실)**

`app.rs:12461`의 프로젝션은 **바인딩이 살아 있는 세션의 최근 24턴을 매 주기 재-upsert**한다
(`ON CONFLICT … agent_summary = excluded.agent_summary`). 따라서:

- 지금 pane에 붙어 있는 세션의 최근 24턴 → **다음 수집에서 새 상한으로 갱신된다**.
- 이미 끝난 세션의 행 → **잘린 채 남는다**. 소급 재파싱은 하지 않는다(transcript를 전수
  다시 읽는 비용을 자동으로 치르지 않는다는 뜻이고, 그 자리는 2번이 메운다).

## 2. 원문 보기

**대상:** 신규 `crates/app/src/ui/transcript_viewer.rs`, `agent_transcript.rs`(읽기 함수),
`agent_detect.rs`(경로 해석 노출), `app.rs`(배선), `ui/work_history.rs`(액션)

### 2-1. 배치

작업 이력 탭 본문을 **마스터-디테일**로 바꾼다 — git 패널과 같은 구조다.

- 좌: 지금의 카드 목록. 폭은 `git_tab_list_width`와 같은 규칙(넓으면 360pt 고정, 좁으면
  40%, 220pt 하한). 카드가 git 파일 행보다 정보가 많아 하한이 조금 크다.
- 우: 선택한 턴의 **대화 원문**. 아무것도 고르지 않았으면 안내 한 줄
  (`history.transcript.empty`).
- 사이에 세로 구분선 하나(git 패널과 같은 관례).

카드에 액션 하나를 더한다: **「원문 보기」** → `WorkHistoryAction::ShowTranscript(identity)`.
카드 본문 클릭(펼침)과 충돌하지 않게, git 패널 행이 쓰는
`scope_builder(UiBuilder::sense(click))` + 명시적 `widget_info` 관례를 따른다.

### 2-2. transcript 경로 해석

행에 이미 `kind`와 `agent_session_id`가 있다. 세션이 죽었어도 파일은 남는다.

- Claude: `agent_detect::find_claude_transcript(session_id)` — `~/.claude/projects/*/<sid>.jsonl`.
  현재 `fn`이라 `pub(crate)`로 올린다.
- Codex: `~/.codex/sessions` 아래 rollout 파일. 현재 해석기는 **cwd 기준**이라
  (`find_codex_transcript(cwd)`), 세션ID 기준 경로가 이미 있는지 확인해 재사용하고,
  없으면 행의 `cwd`로 폴백한다. 둘 다 실패하면 우측에
  `history.transcript.not_found`를 표시한다(패널을 죽이지 않는다).
- Kimi도 같은 규칙을 따르되, 해석기가 없으면 이번 범위에서는 `not_found`로 둔다.

### 2-3. 읽기 (off-thread, 유계)

`agent_transcript.rs`에 뷰 전용 함수를 더한다. 상태 판정용 tail 파서와 **다른 상한**을 쓴다
— 그쪽은 "지금 무슨 상태인가"를 싸게 알아내는 것이고, 이쪽은 "사람이 읽는다"가 목적이다.

```rust
pub struct ConversationMessage {
    pub role: ConversationRole,   // User | Assistant
    pub text: String,
    pub at: Option<i64>,          // epoch secs
}

pub struct TranscriptConversation {
    pub messages: Vec<ConversationMessage>,
    /// 파일 앞부분이 잘렸다(오래된 메시지가 창 밖).
    pub truncated: bool,
}

pub fn read_conversation(path: &Path, kind: AgentKind)
    -> Result<TranscriptConversation, TranscriptViewError>;
```

상한:

| 항목 | 값 | 이유 |
| --- | --- | --- |
| 파일 tail | 4 MB | 긴 세션도 최근 대화는 충분히 담긴다 |
| 메시지당 텍스트 | 8 KB | 읽을 수 있는 길이. 넘으면 그 메시지만 잘리고 표시 |
| 메시지 수 | 200 | 화면 가상화와 무관하게 메모리 유계 |
| 총 바이트 | 1 MB | 위 둘의 곱보다 낮은 실효 상한 |

- 실행은 기존 `AppHostIoAction` 경로에 요청 종류를 하나 더해 off-thread로 돌린다
  (git 패널 IO와 같은 latest-only, in-flight 1개).
- tool 호출·thinking·system 레코드는 **거른다**. 역할은 user/assistant 둘만 보여준다
  (orca의 `CONVERSATION_ROLES`와 같은 선택).
- 노이즈 접두 거부 규칙은 1번의 것을 **재사용**한다 — 단, 여기서는 400자로 자르지 않는다.

### 2-4. 렌더

- `ScrollArea::show_rows` 가상화 — 200개여도 프레임 비용이 유계다.
- 역할 라벨 + 역할별 배경(사용자/에이전트). 텍스트는 선택·복사 가능.
- 맨 아래(최신)에서 시작한다.
- 잘림이면 상단에 `history.transcript.truncated` 한 줄.

## 3. 턴당 최근 메시지 배열

**대상:** `crates/storage/src/db.rs`(마이그레이션·검증), `app.rs`(프로젝션),
`agent_transcript.rs`(수집), `ui/work_history.rs`(렌더)

한 턴 안에서 에이전트가 여러 번 말해도 지금은 **마지막 하나**만 남는다(`last_agent_summary`).
orca처럼 최근 몇 개를 함께 남긴다.

### 3-1. 스키마 (forward-only 마이그레이션 1줄)

`MIGRATIONS` 배열 끝에 추가한다. additive라 기존 행·기존 쿼리에 영향이 없다.

```sql
ALTER TABLE agent_work_turns ADD COLUMN messages_json TEXT;
```

기존 행은 NULL이고, NULL이면 지금과 똑같이 `instruction` + `agent_summary`만 보여준다
(하위 호환 — 롤백해도 이 컬럼을 무시할 뿐 데이터가 깨지지 않는다).

### 3-2. 형식과 상한

```json
[{"r":"u","t":"...","at":1755230000},{"r":"a","t":"...","at":1755230012}]
```

| 항목 | 값 |
| --- | --- |
| 메시지 수 | 5 (turn 안 최신 5개) |
| 텍스트 | 1번 규칙 그대로 (400자 / 4줄 / `…`) |
| 컬럼 바이트 | 8 KB — 초과하면 **컬럼을 NULL로 떨어뜨린다**(fail-soft) |
| 행 전체 | 기존 `AGENT_WORK_TURN_ROW_BYTES_MAX = 32 KB` 안 |

`r`은 `"u"`/`"a"` 두 글자만 허용한다. 파싱 실패·미지 값·상한 초과는 전부 **None**으로
떨어뜨리고, 카드는 기존 두 필드로 그린다. 이력 하나 때문에 패널이 죽지 않는다.

저장 검증은 기존 `agent_work_turn` 검증 함수 옆에 붙인다 — NUL 금지, UTF-8, 바이트 상한,
JSON 파싱 성공, 배열 길이 ≤ 5.

### 3-3. 렌더

펼친 카드에서 「최근 작업」 자리를 대체한다.

- `messages_json`이 있으면 역할 라벨(`나` / `에이전트`)을 붙여 최대 5줄 그룹으로.
- 없으면 지금과 같이 `agent_summary` 한 덩이.
- orca가 쓰는 정리 규칙 둘을 차용한다: **인접 동일 역할·동일 텍스트 중복 제거**,
  **`instruction`과 같은 텍스트인 턴 제거**(제목이 본문에 두 번 나오는 것 방지).

## 4. i18n (5로케일 전부)

추가: `history.action.show_transcript`, `history.transcript.empty`,
`history.transcript.not_found`, `history.transcript.truncated`,
`history.transcript.loading`, `history.transcript.error`,
`history.role.user`, `history.role.agent`.

## 5. 테스트

- `clean_agent_summary`: 줄바꿈 보존, 연속 개행 접기, 400자 컷, 4줄 컷, 노이즈 접두 거부
  (기존 테스트가 120자를 고정하고 있으면 새 상한으로 갱신하되 **거부 규칙 테스트는 그대로**).
- `read_conversation`: 역할 필터, 메시지당 8KB 컷, 200개 컷, 총 1MB 컷, 잘림 플래그,
  손상 라인 건너뛰기(파일 전체를 죽이지 않음).
- `messages_json`: 직렬화·역직렬화 왕복, 5개 초과 거부, 8KB 초과 시 NULL, 잘못된 role 거부,
  NULL 행이 기존 렌더로 떨어지는지.
- 마이그레이션: 기존 DB에 컬럼이 붙고 기존 행이 NULL로 읽히는지.
- kittest: 「원문 보기」 클릭 → `ShowTranscript` intent, 목록/원문 분할 폭, 빈 상태 문구.
- 게이트: `cargo test -p deppy-sijo` + `cargo test -p storage` + `cargo test -p i18n` +
  `cargo clippy --workspace --all-targets -- -D warnings` 0건.

## 6. [2026-08-16 추가] 원문에서 그 턴을 선택해 보여준다

1차 구현은 원문을 열면 **항상 최신(맨 아래)**에서 시작한다. 그런데 카드는 특정 **턴**이다 —
오래된 카드를 눌러도 그 턴이 아니라 대화 끝이 뜬다. 사용자 요구: 「원문 보기」를 하면
**그 카드가 가리키는 턴이 원문에서 선택돼** 보여야 한다.

### 6-1. 앵커

`AgentWorkTurnRow.source_offset`은 **그 턴을 연 레코드 줄의 절대 파일 오프셋**이다
(`PendingTurn::new`가 `snapshot_lines`의 오프셋을 그대로 받는다). `read_conversation`도
같은 `snapshot_lines`를 돌므로 **좌표계가 같다** — 별도 인덱스나 재파싱이 필요 없다.

### 6-2. 데이터

`ConversationMessage`에 `pub offset: u64`를 더한다(그 메시지 레코드 줄의 절대 오프셋).
`Debug`는 지금처럼 텍스트만 가린다 — 오프셋은 위치 정보라 가릴 필요가 없다.

### 6-3. 선택 범위

`focus_offset`(= 행의 `source_offset`)이 주어지면:

- **시작** = `offset >= focus_offset`인 **첫** 메시지. 정확히 일치하는 것이 정상이지만,
  턴을 연 줄이 노이즈 규칙으로 걸러졌을 수 있어 부등호로 잡는다.
- **끝** = 시작 다음에 나오는 **첫 User 메시지 직전**(그게 다음 턴의 시작이다). 없으면 끝까지.
- 그 범위 전체를 선택 배경으로 강조하고, **시작 메시지가 화면 위쪽**에 오도록 스크롤한다.
- `focus_offset`이 스냅샷 창(꼬리 4MB)보다 앞이면 찾을 수 없다 → 강조 없이 맨 아래에서
  시작하고 `history.transcript.focus_missing` 안내를 상단에 띄운다. **조용히 최신을
  보여주면 사용자는 그게 그 턴인 줄 안다** — 그래서 반드시 말해 준다.
- `focus_offset`이 없으면(향후 다른 진입점) 지금처럼 맨 아래에서 시작한다.

### 6-4. 상태

`set_conversation(result, focus_offset: Option<u64>)`로 시그니처를 넓힌다. 뷰어는 세대
번호를 이미 올리고 있으므로 `ScrollArea` 상태는 새 대화마다 새로 잡힌다 — 강조 위치로의
스크롤도 그 위에서 한 번만 적용된다.

### 6-5. i18n 추가 (5로케일)

`history.transcript.focus_missing`.

## 범위 외

- 이미 끝난 세션의 요약을 소급 재파싱해 DB를 다시 채우는 일(원문 보기가 대신한다)
- transcript 원문 저장·인덱싱·검색
- tool 호출·thinking 블록 렌더
- 실시간 워처(파일이 자라는 걸 따라가며 갱신) — orca의 `transcript-watch`에 해당하는 것
- 세션 단위 집계(messageCount·totalTokens 같은 orca 필드)
