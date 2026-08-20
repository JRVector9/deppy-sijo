# 에이전트 완료/실패 알림 겹겹이 (2026-08-20)

## 요구사항

"완료·실패 알림이 절대 놓치지 않게, 한 가지 수단에 걸지 말고 겹겹이 깔아라."

## 이미 확정된 원인 (재조사 없이 여기서 출발)

기본 설정으로 띄운 에이전트는 작업이 끝나도 완료/실패 알림이 오지 않는다:

1. `agent_launcher::agent_then_shell_script()` — `"$@"; ...; exec "${SHELL:-/bin/sh}"`.
   에이전트의 `$?`를 버린다. `SessionExited`는 **폴백 셸이 나중에 종료할 때** 오고,
   `exit_code`는 **셸의 것**이다.
2. `session::status::BUILTIN`에 `approval`/`waiting`만 있고 `done`/`error`가 없다.
3. `agents.rs`의 사용자 regex 기본값이 빈 문자열이고, provider별 자동 채움이
   어디에도 없다.
4. `notifications.rs`는 Done/Error를 오직 `exit_code == Some(0)`으로만 판정한다
   (`on_pty_exit`). `Idle`은 명시적으로 알림 제외.

## 적용한 겹

### 겹① — 에이전트의 진짜 종료 코드를 exit sentinel로 흘려보낸다 (최우선, 가장 정확)

**메커니즘.** `agent_then_shell_script()`가 `"$@"` 직후 `$?`를 잡아 파일에 쓴다:

```sh
"$@"; __deppy_exit=$?; printf '%s' "$__deppy_exit" \
  > "${TMPDIR:-/tmp}/deppy-agent-exit-$$" 2>/dev/null || true; \
  stty sane ...  # 이하 기존 그대로
```

파일명은 **이 래퍼 셸 자신의 PID**(`$$`)로 결정된다. 이 PID는 runtime이
`session.process_identity().pid`로 이미 알고 있는 값과 **정확히 같은 프로세스**를
가리킨다 — PTY가 직접 스폰하는 게 바로 이 `/bin/sh`이기 때문이다. 그래서 새 IPC
채널(소켓/named pipe/env 토큰)을 만들지 않고도 스크립트(셸)와 runtime(Rust)이 파일
경로 하나에서 만난다. 경로 계산 공식(`{temp_dir}/deppy-agent-exit-{pid}`)은
`session::agent_exit_sentinel_path` + `AGENT_EXIT_SENTINEL_PREFIX`로
`crates/session/src/status.rs`에 한 번만 정의하고, `runtime`이 재수출해 `app`
(스크립트 쪽)과 `runtime`(폴링 쪽) 양쪽이 같은 상수를 쓴다 — `DRAIN_PENDING_TTY_INPUT`
과 같은 관례.

`crates/runtime/src/in_process.rs`:

- `SpawnAgent`(신규 실행)와 "이어서 하기"(respawn-into-pane) 두 경로에서 세션의
  detector를 설치한 직후 `register_agent_exit_watch(id)`를 호출한다. pid를
  `process_identity()`로 못 구하면(플랫폼 제약 등) 조용히 건너뛴다 — 실패해도 기존
  동작(idle heuristic·`SessionExited`)이 그대로 남는다.
- `pump_sessions()`가 매 tick 시작에서 `poll_agent_exit_sentinels()`를 부른다.
  감시 중인 세션의 sentinel 파일이 나타나면 `StatusDetector::note_exit_sentinel(code)`
  로 즉시 latch하고 파일을 지운다(1회성). **새 이벤트 타입을 추가하지 않았다** —
  `evaluate()`가 그 자리에서 `SessionStatusChanged`/`SessionStatusViewChanged`를
  평소처럼 emit하므로, `notifications.rs`의 기존 배선(`process_ws_notifications`가
  `SessionStatusChanged`를 구독)을 그대로 탄다. `app.rs`를 단 한 줄도 건드리지
  않고 알림이 나가는 이유다.
- 세션이 실제로 종료되면(`exited` 정리 루프) 감시 항목과 남은 파일을 지운다 —
  temp dir에 흔적을 남기지 않는다.

**latch 의미론(`crates/session/src/status.rs`).** `note_exit_sentinel`은
`StatusSource::ProcessExit`(신뢰도 1.0, 기존 `SessionStatusView::process_exit`과
동급)로 상태를 확정하고 `screen_derived = false`로 둔다 — 이후 idle 휴리스틱이나
화면 재스캔이 덮어쓰지 않고, 사용자가 폴백 셸에 실제로 타이핑할 때(`on_input`/
`on_turn_start`)만 해제된다. 기존 error/done 화면·regex latch와 동일한 규칙이다.

**틀릴 수 있는 경우.**

- temp dir가 쓰기 불가(샌드박스·읽기 전용 파일시스템)면 `printf`가 조용히
  실패하고(`|| true`) sentinel이 영영 안 나타난다 → 겹④(최후의 그물)가 받는다.
- `$?`가 128+signal인 SIGKILL 등은 **정확히 그대로** 기록된다(의도된 동작 —
  강제 종료도 "0이 아닌 종료"로 올바르게 Error가 된다).
- non-unix(Windows)는 `wrap_agent_then_shell`이 애초에 no-op이라(에이전트를
  그대로 spawn) 이 sentinel 자체가 필요 없다 — 그 세션의 `SessionExited` exit_code가
  이미 에이전트 자신의 것이다.
- custom agent(AgentsUi로 등록한, `wrap_agent_then_shell`을 안 거치는 것)도
  `register_agent_exit_watch`가 걸리지만 sentinel이 절대 안 나타난다 — 세션
  수명 동안 매 tick 파일 하나 `open()` 실패만 반복하는 무해한 낭비다(세션 종료
  시 정리됨). "어떤 명령이 wrap됐는지" 별도 플래그를 두지 않은 트레이드오프.

**화면·redaction 안전성.** `printf`는 파일로만 쓴다 — PTY로 나가는 바이트가
없으므로 화면에 아무것도 안 찍히고, 세션 로그 redaction과 무관하다(redaction은
PTY 출력 스트림에만 적용된다). 실측 테스트(아래)가 stdout/stderr에 흔적이
없음을 고정한다.

**테스트.**

- `crates/session/src/status.rs`: `sentinel_경로는_temp_dir와_pid로_결정된다`,
  `exit_sentinel은_종료코드로_done_error를_latch한다`,
  `exit_sentinel_상태는_idle_휴리스틱에_덮이지_않는다`,
  `exit_sentinel_상태도_입력으로_해제된다`.
- `crates/app/src/agent_launcher.rs`: `에이전트의_진짜_종료코드가_보이지_않게_sentinel_파일에_남는다`
  (exit 7, 실제 `/bin/sh` 프로세스로 sentinel 생성 + 화면 무흔적 확인),
  `정상_종료코드_0도_sentinel에_그대로_남는다`.
- `crates/runtime/src/in_process.rs`:
  `exit_sentinel은_폴백_셸이_살아있어도_에이전트의_진짜_종료코드를_즉시_반영한다`
  — 폴백 셸을 일부러 `sleep 30`으로 살려 둔 채로, `SessionExited`가 오기 전에
  `SessionStatusChanged{Error}`가 먼저 온다는 것과 `SessionExited`가 아직
  안 왔다는 것을 함께 고정한다(진짜 PTY, 실제 프로세스).

### 겹② — 화면 감지 BUILTIN done/error 패턴 → **적용하지 않음(실증으로 확인)**

`status.rs`의 `BUILTIN.approval`/`waiting`과 같은 방식으로 `done`/`error` 내장
패턴을 추가하는 안을 먼저 시도했다. 실제 Claude Code CLI(`v2.1.237`, 사용자 로컬
설치)를 pty(`pty.fork`)로 직접 띄우고 `pyte`(터미널 에뮬레이터)로 화면을
정확히 렌더링해 검증했다:

```
15 '⏺ PROBEDONE'
17 '✻ Crunched for 1s'          ← 완료 표시(랜덤 동사: Worked/Crunched/...)
19 '────────────────'
20 '❯'
21 '────────────────'
22 '  ⚠ Transcript saving is off ...'
23 '  …// · Opus 5 (1M context) · 96%'
24 '  ⏵⏵ auto mode on (shift+tab to cycle)'
```

`StatusPatterns::match_screen`은 **마지막 비어있지 않은 5줄**만 스캔한다
(`crates/session/src/status.rs:146`, `SCAN_TAIL_LINES`). 완료 표시(`✻ Crunched
for 1s`, row 17)는 항상 렌더되는 하단 고정 푸터(transcript 경고 + 모델/컨텍스트
줄 + auto-mode 줄, row 22–24) 때문에 tail-5줄 밖으로 밀려난다 — 응답 완료
직후(re: `Worked for 2s`)와 55초 뒤 idle 상태(`Crunched for 1s`) 둘 다 동일한
구조였다. 즉 이 위치에 어떤 regex를 넣어도 **절대 매치되지 않는 죽은 코드**가
된다 — 매치 안 되는 패턴을 넣느니 안 넣는 게 정직하다.

추가로 완료 표시 자체가 "Crunched"/"Worked"/"Garnished" 등 **고정 어휘가 아닌
랜덤 flavor 단어**라 스캔 위치 문제가 없었어도 패턴 커버리지가 불완전했을
것이다.

Codex CLI(`codex-cli 0.148.0`)도 같은 방식으로 검증을 시도했으나 실제 로그인
세션(`codex app` 랜딩 페이지로 리다이렉트)과 터미널 질의 응답(CPR/OSC 10·11/DA)
을 흉내 낸 프로브의 한계로 정상 화면을 재현하지 못했다 — **Codex의 정확한
tail-5줄 구조는 실증하지 못했다.** 다만 기존 코드에 이미 있는 codex 승인 푸터
패턴 주석("옵션이 위쪽·푸터가 꼬리에 온다")과 일반적인 codex CLI 하단 고정
상태줄(모델/토큰) 구조를 볼 때 같은 문제(완료 텍스트가 고정 푸터에 밀려남)가
있을 개연성이 높다고 판단해, **추측 패턴을 넣지 않았다.**

### 겹③ — provider별 done/error regex 기본값 → **적용하지 않음(②와 같은 근본 원인)**

`crates/app/src/ui/agents.rs:333`의 빈 기본값(`done_regex: String::new()`)을
provider 기본값으로 채우는 안을 검토했다. 하지만 코드 추적 결과, 이 필드는
`AgentsUi`(설정 화면의 **커스텀 에이전트 등록 폼**) 전용이고 builtin(Agent
Launcher로 띄우는 claude/codex/kimi) 실행 경로와는 무관하다:

- `crates/storage/src/db.rs`의 `upsert_builtin_agent_config`는 **앱 시작마다**
  builtin agent config row의 `waiting_regex`/`approval_regex`/`error_regex`/
  `done_regex`를 `NULL`로 강제한다(`ON CONFLICT ... SET ... = NULL`).
  builtin에 대해 이 필드를 사용자가 지속적으로 설정할 경로 자체가 없다.
- Settings→Agents 목록은 `!is_builtin_config_id(&row.id)`로 builtin을 **아예
  걸러낸다** — `AgentsUi` 폼은 builtin 행을 보여주지도 편집하지도 않는다.
- `app.rs`의 `prepare_quick_agent_launch`/`dispatch_resume_archived_agent_new_pane`
  둘 다 builtin 실행 시 `waiting_regex: None, ..., done_regex: None`을 직접
  하드코딩해 넘긴다.

즉 "빈 기본값" 문제의 실제 발원지는 `agents.rs`가 아니라 이 세 지점이었다.
그런데 설령 여기(또는 `agent_launcher::AgentKind`)에 provider 기본값을
컴파일해 넣어 위 경로로 흘려보내도, 그 값은 결국 같은
`StatusPatterns::match_screen`(tail-5줄)로 매치된다 — **②에서 실증한 것과
정확히 같은 구조적 한계**에 부딪힌다. "done" 텍스트가 화면에 있어도 못 보는
문제를, regex 출처를 config에서 provider 기본값으로 바꾼다고 해결되지 않는다.
그래서 ③은 ②와 같은 이유로 적용하지 않았다 — 서로 "중복돼서" 하나를
고른 게 아니라, **둘 다 같은 근본 원인으로 무효**라 판단했다.

### 겹④ — 최후의 그물: 에이전트 프로세스 소멸 감지 → **중립 알림으로 적용**

①이 실패하는 극히 드문 경우(temp dir 쓰기 불가 등)를 위한 마지막 안전망.
`agent_detect.rs`가 이미 매 감지 주기(ps 스캔)마다 "이 세션에서 claude/codex/kimi
프로세스가 보이는가"를 계산한다(`agent_kinds` — 사이드바 Off 표시가 이미 이
신호를 쓴다, `agent_surface::AgentVisualState::from_pty_with_agent`).

`crates/app/src/agent_detect.rs`에 순수 함수
`agent_vanished_sessions(previously_present, now_present, still_alive, resolved)`
를 추가했다:

- **"있었는데 없어짐"**(`previously_present`에는 있고 `now_present`엔 없음)
- **그리고 세션 자체는 살아있음**(`still_alive`에 있음 — 아니면 `SessionExited`
  경로가 이미 처리)
- **그리고 아직 아무 결과도 확정 안 됨**(`resolved(session)`이 false —
  겹①의 exit sentinel이나 regex가 이미 Done/Error를 냈으면 제외)

`app.rs::poll_agent_detect`의 `agent_kinds` 갱신 직전(교체되기 전의 이전 값과
새 값을 둘 다 가진 유일한 지점)에서 이 함수를 호출하고, 해당하면
`notifications_ui.on_agent_vanished(title, catalog)`를 부른다. `resolved`
판정은 `WorkspaceUi::last_session_status`(regex/exit-sentinel이 갱신하는 바로
그 값)로 겹①과 자연히 연동된다.

**완료도 실패도 모른다는 문제를 어떻게 풀었나.** `on_agent_vanished`
(`notifications.rs`)는 **`SessionStatus`를 전혀 쓰지 않는다** — Done/Error
전용으로 짜인 `NotificationItem`/아이콘/색/`retain_*` 정리 로직에 억지로
끼워 넣지 않고, native OS 알림(`NativeNotificationIntent`) 하나만 큐에
넣는다. 알림 문구("에이전트 프로세스 종료 - 결과 확인 필요")도 완료/실패를
단정하지 않는다. history 목록에도 안 올린다 — 나중에 진짜 결과(사용자가
폴백 셸에서 `exit`을 치거나, 뒤늦게 exit sentinel이 쓰였거나)가 오면 그건
`on_pty_status`/`on_pty_exit`이 각자 정상적으로 알린다.

**틀릴 수 있는 경우.** 감지가 활성 워크스페이스에서만 돈다(기존 한계,
`agent_surface.rs` 주석과 동일) — warm/백그라운드 워크스페이스에서 죽은
에이전트는 이 그물에 안 걸린다. ps 스캔 자체가 한 tick 흔들리면(일시적
미검출) 오탐 가능성이 있으나, `previously_present`/`now_present` 비교가
그 감지 주기 자체의 스냅샷이라 기존 "감지 흔들림에도 상태 유지" 정책과
동일한 수준의 신뢰도다(새로 만든 게 아니라 이미 검증된 `agent_kinds`
파이프라인을 재사용).

## 우선순위 정리 (겹치는 순간 무엇이 이기나)

1. **exit sentinel/regex/화면 패턴이 낸 Done/Error**(겹①, 기존 규약)가 최종
   권위 — latch되어 사용자 입력 전까지 안 바뀐다.
2. **겹④(중립)**은 1이 확정되지 않았을 때만 발화하고(`resolved` 필터),
   1이 나중에 확정돼도 4는 SessionStatus가 없어 dedup 대상이 아니다 — 그냥
   "결과 확정 전 안내" + "결과 확정 알림" 두 개가 순서대로 온다. 이건
   **중복 알림이 아니다** — 서로 다른 사실(프로세스 소멸 vs 최종 결과)을
   알리는 별개 사건이다.
3. **`on_pty_exit`(SessionExited, 폴백 셸의 exit code)**은 기존 dedup
   (`push_status_with_dedupe`의 "같은 target·같은 status 직전 항목" 판정)이
   그대로 막는다 — 겹①이 이미 같은 Done/Error를 냈으면 재발화하지 않는다
   (기존 테스트 `exit_알림과_status_중복_방지`가 이미 고정).

## 반영하지 않은 것 — `crates/app/src/ui/agents.rs`

이 파일은 owned 영역으로 지정됐지만, 위 ③ 분석대로 builtin 실행 경로와
무관해 실제 변경을 만들지 않았다. `AgentRegistration`/`AgentsUi` 자체(커스텀
에이전트 등록 폼)의 기존 동작은 이 작업의 버그(builtin 기본 실행)와 관련이
없어 건드리지 않았다 — surgical-changes 원칙.
