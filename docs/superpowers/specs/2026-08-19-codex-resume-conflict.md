# codex 「이어서 하기」가 writer 충돌로 실패하는 문제 (2026-08-19)

## 증상

「이어서 하기」를 누르면 새로 뜬 PTY 터미널에 이렇게 뜬다:

```
jr@Mac Design % cd '/Users/jr/Desktop/projects/colon35/Design' && codex resume 01a013c2-62bf-7e91-8fd4-6364048215c4
Error: Failed to resume session from /Users/jr/.codex/sessions/2026/08/18/rollout-2026-08-18T16-25-01-01a013c2-...jsonl:
  thread/resume failed during TUI bootstrap: thread/resume failed:
  thread 01a013c2-62bf-7e91-8fd4-6364048215c4 already has an active writer (code -32600)
```

## 확정한 원인 — 우리 앱이 같은 codex thread를 두 경로로 동시에 열려 한다

deppy-sijo는 codex 세션을 여는 경로가 **둘**이고, 서로 완전히 독립된 상태를 갖는다.

1. **PTY-native 경로** — `crates/app/src/agent_resume.rs`가 CLI 인자를 판정하고
   (`resume_plan`), `crates/app/src/app.rs::dispatch_respawn_archived_agent`가
   `RuntimeCommand::RespawnArchivedAgent`로 PTY 셸에 `codex resume <thread_id>`를
   그대로 실행시킨다. 저장된 바인딩은 `storage::ArchivedAgentResumeRow`
   (`kind`/`session_id` — `session_id`가 codex의 native thread id)이고,
   `crates/storage/src/db.rs`의 `sessions` 테이블에서 온다.
2. **App Server(구조화) 경로** — `crates/app/src/ui/agent_sessions.rs`의 "Agent
   Sessions" 패널이 `codex app-server --listen stdio://`를 자식 프로세스로 띄워
   (`crates/app/src/codex_app_server.rs`) JSON-RPC로 대화한다. 사용자가 이 패널에서
   저장된 스레드를 골라 "재개"를 누르면 `resume_selected_persisted` →
   `CodexAppServerClient::resume_thread` → `thread/resume`을 보낸다
   (`agent_sessions.rs:1117-1134`). 성공하면 그 로컬 세션 id가
   `attached_threads: HashSet<AgentSessionId>`에 들어간다(`agent_sessions.rs:1865-1869`,
   `:3465`) — 이 App Server 프로세스가 살아 있는 한(앱이 켜져 있는 한) 그 thread의
   rollout 파일 writer를 계속 쥐고 있다.

두 경로는 **같은 codex thread id를 참조할 수 있다** — codex CLI로 직접 시작한
세션이든 App Server로 재개한 세션이든, thread id는 codex가 파일시스템(rollout jsonl)에
부여하는 값이라 두 경로가 우연이 아니라 필연적으로 겹칠 수 있다. 사용자가 Agent
Sessions 패널에서 어떤 thread를 재개해 App Server가 writer를 쥔 상태로, **같은
thread에 대응하는 PTY pane**(Work History나 pane 컨텍스트 메뉴의 「이어서 하기」)을
또 누르면 `dispatch_respawn_archived_agent`가 그 사실을 전혀 모른 채 PTY에서 `codex
resume <같은 id>`를 또 실행한다 — codex는 rollout 파일당 writer 하나만 허용하므로
`-32600`으로 거부한다. codex의 에러는 정확하다; 상황을 만든 건 우리 쪽 배선이다.

`dispatch_respawn_archived_agent`(app.rs)가 두 경로를 잇는 **유일한 IO 지점**이다 —
Work History의 `AppWorkHistoryActivation::ResumeArchived`와 pane 하단 「다시 실행」
버튼(workspace.rs → `take_respawn_archived_request`)이 전부 이 함수 하나로 모인다.
file_tree.rs의 pane 컨텍스트 메뉴 「이어가기」도 같은 `respawn_archived_request` 슬롯을
쓴다. 따라서 이 함수 하나만 고치면 모든 PTY resume 진입점을 커버한다.

## 수정

`crates/app/src/app.rs::dispatch_respawn_archived_agent`가 `RuntimeCommand::
RespawnArchivedAgent`를 만들기 **직전**에 충돌 여부를 확인한다.

1. 새 순수 함수 `archived_agent_row_for_session`(app.rs) — `session`이 앉은 pane의
   durable `sessions.id`로 `ArchivedAgentResumeRow`(PTY native binding: kind,
   session_id)를 찾는다. `resolve_work_history_activation`이 이미 쓰던 조회 패턴을
   재사용했다.
2. 새 순수 함수 `attached_app_server_conflict`(app.rs) — 그 행이 `kind == "codex"`이고
   `session_id`(native thread id)가 App Server에 이미 attach돼 있으면 그 local(App
   소유) 세션 id를 돌려준다. codex만 대상이다 — App Server가 다루는 provider가
   codex뿐이라 claude/kimi/qwen-code 바인딩은 애초에 겹칠 수 없다.
3. 새 조회 `AgentSessionsUi::attached_local_session_for_thread`(agent_sessions.rs) —
   `attached_threads`(현재 App Server가 writer로 쥔 로컬 세션 id 집합)를
   `persisted_threads`(로컬 id → thread id 등 메타데이터)와 교차해 thread id로
   역조회한다.
4. 충돌이 확인되면 **PTY `codex resume`을 만들지 않고** `AgentSessionsUi::
   open_session`으로 이미 열려 있는 구조화 세션에 포커스를 옮긴다 — Agent Sessions
   패널이 열리고 그 대화가 선택된 상태로 뜬다. 이것으로 "「이어서 하기」를 누르면
   대화가 이어진다"는 요구를 만족한다(에러 없이, 실제로 이어짐).
5. `open_session`이 실패하는(사실상 불가능에 가까운 레이스 — attach는 됐는데 로컬
   세션 항목이 이미 사라진 경우) 극히 드문 경우에만 새 i18n 키
   `agent_sessions.error.thread_attached_elsewhere`로 원문 codex 에러 대신 이해할 수
   있는 한국어(및 4개 로케일) 안내를 보여준다(`AgentSessionsUi::
   report_thread_attached_elsewhere` — 패널도 함께 연다).

버튼 라벨/프리젠테이션(`ArchivedResumePresentation`, workspace.rs·work_history.rs가
그리는 "이어서 하기" 문구)은 건드리지 않았다 — 그 파일들은 다른 에이전트 소유이고,
enum에 새 variant를 추가하면 그쪽의 exhaustive match를 깰 위험이 있다. 대신 클릭
시점의 **동작**만 바꿨다: 버튼은 여전히 "이어서 하기"로 보이지만, 클릭하면 충돌
상황에서는 PTY 대신 이미 있는 대화로 안전하게 이동한다.

## 테스트

- `crates/app/src/ui/agent_sessions.rs`
  - `attached_local_session_for_thread_finds_only_the_attached_match` — attach 안 된
    thread, 다른 local session에 attach된 thread, 알려지지 않은 thread가 전부 `None`을
    돌려주고, 정확히 일치하는 경우만 그 local id를 찾음을 검증.
  - `report_thread_attached_elsewhere_opens_panel_with_localized_notice` — 패널이
    열리고, 렌더된 안내 문구가 raw 키 문자열 그대로 새지 않음을 검증(과거 키 누락으로
    raw 문자열이 화면에 뜬 사고의 회귀 방지 형태로 작성).
- `crates/app/src/app.rs`
  - `archived_agent_row_for_session은_persistent_session_id로_찾는다`
  - `attached_app_server_conflict은_codex_thread가_attach됐을_때만_잡는다` — codex +
    같은 thread만 충돌로 잡고, provider가 다르면(우연히 문자열이 같아도) 잡지 않고,
    attach 안 된 thread는 충돌 없음(=기존 다수 경로 무변화)을 검증.

## 게이트 결과

- `cargo test -p deppy-sijo` — 1759 passed, 0 failed, 8 ignored(플랫폼 실측 전용).
- `cargo clippy --workspace --all-targets -- -D warnings` — 0 경고.
- `cargo run -q -p xtask -- check-boundary` — OK.
- `cargo run -q -p xtask -- i18n-check` (+`cargo test -p i18n`) — OK, 8 passed
  (`all_bundled_locales_match_fallback_order_and_placeholders` 포함 — 5개 로케일 키
  정렬/placeholder 일치 확인).
- `cargo fmt --all -- --check` — 이 저장소는 이 워크트리를 만들기 전부터 다수 파일에
  포맷 드리프트가 있었다(예: `agent_detect.rs`, `agent_launcher.rs` 등, 이 작업과 무관).
  내가 건드린 5개 파일(`agent_resume.rs`, `codex_app_server.rs`, `agent_session.rs`,
  `agent_actions.rs`, `ui/agent_sessions.rs`) 중 새 diff가 뜨는 파일은 없었고, `app.rs`에
  이미 있던 드리프트도 내가 추가한 줄(신설 함수 3개 + 테스트) 범위와는 전혀 겹치지
  않음을 diff 줄 번호를 직접 대조해 확인했다.

## 미검증 — 실제 codex로 확인하지 못한 것

지시에 따라 사용자의 실제 codex 세션(rollout jsonl)을 열거나 codex 프로세스를 새로
띄우지 않았다. 따라서 아래는 **코드 추적 + 단위 테스트로만** 확인했고, 실제 codex
바이너리로 재현·검증하지 못했다:

- 실제로 App Server에서 스레드를 재개한 뒤 같은 스레드를 PTY 「이어서 하기」로 눌렀을
  때, 이 수정이 화면에서 정말 Agent Sessions 패널로 전환되고 대화가 보이는지(눈으로
  확인 필요 — 앱 빌드/실행 금지 지시로 하지 못함).
- 사용자가 원래 겪은 정확한 시나리오(스크린샷의 thread id)가 "App Server 쪽에서 먼저
  attach된 경우"인지, 아니면 다른 경로(예: 외부에서 별도로 실행 중인 codex 프로세스,
  또는 앱 재시작 사이 좀비 App Server)로 writer를 쥔 경우인지는 구분하지 못했다.
  후자라면(우리 앱이 모르는 외부 writer) 이번 수정으로는 잡히지 않는다 — 그 경우
  PTY는 여전히 `codex resume`을 시도하고 codex가 원문 에러를 그대로 터미널에 출력할
  것이다. 그 원문 에러 텍스트를 감지해 번역된 안내로 바꾸려면 PTY stdout을 스캔해야
  하는데, 이는 `crates/session`/`crates/runtime`(다른 에이전트 소유, 손대지 말라고
  명시된 영역) 쪽 작업이라 이번 수정 범위에 포함하지 않았다. 실제 사용자 화면에서
  여전히 이 원문 에러가 보인다면, 그건 "우리 앱이 두 경로를 동시에 쥔" 경우가 아니라
  이 외부-writer 케이스일 가능성이 높고, 별도 후속 작업(PTY 출력 패턴 감지)이 필요하다.
- `codex app-server`가 `thread/resume` 실패 시 정확히 이 JSON-RPC 에러 포맷/코드
  (`-32600`, "already has an active writer")를 낸다는 것은 사용자가 붙여준 원문
  화면으로만 확인했고, `codex --help`/`codex app-server --help` 조회로 이 메시지
  문구 자체를 재확인하지는 않았다(문구가 아니라 "동시 writer 금지"라는 구조적 사실에
  기대어 수정했다 — 문구가 버전마다 달라져도 구조적 수정은 유효하다).
