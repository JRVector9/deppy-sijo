# 강도·모델 단축키: 에이전트 실행 경로 전수와 구현 지침

작성 2026-08-03. `⌃⇧↑/↓`(추론 강도), `⌃⇧←/→`(모델)이 **띄우는 방식에 따라 되다 안 되다**
하는 문제의 인수인계 문서. 구현은 아직 안 했다 — 아래 "미커밋 상태" 절 참조.

## 0. 한 줄 요약

단축키 수신은 정상이다. 깨지는 건 **"이 pane이 무슨 에이전트를 돌리는가"를 알아내는
감지 단계**이고, 그게 실행 방식마다 다르게 깨진다. 그리고 다섯 군데 실패 지점이 **전부
조용해서** 사용자가 "에이전트가 없음"과 "버그"를 구분할 수 없다.

## 1. 확정된 사실 (추측 아님)

**키는 어디에도 안 뺏긴다.** 누를 때마다 `handle_configured_shortcut`까지 도달해
`PTY effort 폴백 실패` 로그를 남긴다. 2026-08-03 21:02~21:03 로그에 40여 줄이 연속으로
찍혔다. 등록·수신·디스패치는 정상이고, 의심할 필요 없다.

**그 로그는 빈 셸에서 누른 것이었다.** 같은 시각 deppy(PID 36159)의 자식 프로세스는
`codex app-server --listen stdio://`와 `/bin/zsh` 둘뿐이었다. PTY 에이전트가 아예 없었다.
`agent_kinds=0`은 정상 동작이었지 버그가 아니다. **"안 된다"는 제보 중 어느 정도가 이
경우인지 현재로선 구분 불가능하다** — 그래서 5절 A가 최우선이다.

**감지는 pane 셸의 하위 트리만 훑는다.** `descendant_pids(shell_pid, rows)`
(`agent_detect.rs:1391`)는 pane 셸 pid에서 시작하는 엄격한 BFS다. 그 부분트리 밖의
프로세스는 존재해도 절대 안 잡힌다.

**분류는 argv 앞 두 토큰의 파일명만 본다.** `classify`(`agent_detect.rs:358`)는
`command.split_whitespace().take(2)`의 `file_name()`이 `claude`/`codex`와 정확히 같은지
본다. 세 번째 토큰 이후는 안 본다.

## 2. 에이전트를 띄우는 모든 케이스

사용자 확인: **런처도 쓰고 셸에 직접 입력도 하는데, 직접 입력이 훨씬 많다.**

| # | 케이스 | transport | argv에 `--model/--effort` | 현재 상태 |
|---|---|---|---|---|
| A | 에이전트 런처 → pane | PTY | **있음** (`agent_launcher.rs:751-766`) | 감지되면 동작 |
| B | 셸에 `claude` 직접 입력 | PTY | **없음** | 5절 B 참조 |
| C | 셸에 `codex` 직접 입력 | PTY | **없음** | 값 불필요, 감지만 필요 |
| D | 에이전트 패널 구조화 세션 | AppServer | 해당 없음 | PTY 폴백이 아예 안 걸림 |

케이스 A의 argv 형태(`agent_launcher.rs:751-766`):
- claude — `--model <slug> --effort <level>`
- codex — `--model <slug>` + `model_reasoning_effort="<level>"` (config override)
- 기타 — `--model <slug> --reasoning-effort <level>`

## 3. 다섯 단계 체인과 케이스별 파손 지점

단축키 한 번이 동작하려면 전부 성공해야 한다. **다섯 개 모두 조용한 실패다.**

| 단계 | 위치 | 실패 시 사용자가 보는 것 |
|---|---|---|
| 1. pid 있는 리소스 행 | `session_resource_usage` (2초 주기) | 무반응 |
| 2. 프로세스를 claude/codex로 분류 | `agent_detect::classify` | 무반응 |
| 3. 표면 생성 | `app.rs:11406 pty_agent_surfaces` | 무반응 |
| 4. 현재 값 파악 | 4단 폴백 (아래) | 무반응 |
| 5. 상태 게이트 | `slash_input_is_safe` | 조용히 큐 적재 |

4단계의 현재 폴백 체인(`app.rs:11430-11470`), 순서대로:
낙관적 값(`pty_agent_pending`) → statusLine → argv → `~/.claude/settings.json` 전역 기본값.

케이스별로 어디서 깨지는가:

- **A** — 2·3단계만 통과하면 4단계는 argv가 보장한다. 가장 튼튼.
- **B** — argv가 비어 4단계가 전역 기본값에 의존한다. statusLine은 1시간 만료
  (`STATUSLINES_PREFIX_PREFLIGHT`)라 오래 유휴한 세션에선 사라진다.
- **C** — codex는 상대 키(`\x1b[1;2A/B`)를 쓰므로 **4단계가 통째로 불필요하다.**
  그런데도 안 된다면 원인은 2단계(분류)뿐이다. → 진단이 아주 좁다, 여기부터 파라.
- **D** — `selected_surface_snapshot()`가 패널 선택을 읽는데, 포커스가 터미널 pane에
  있으면 대상이 빈다. PTY 폴백은 `AgentSurfaceId::Pty`만 찾으므로 구조화 세션을 못 집는다.
  `AgentCapabilities::for_transport`는 AppServer에만 `effort_control`/`model_control`을
  주므로(`agent_surface.rs:139`) 능력은 있는데 **대상 선택이 없다.**

## 4. 유력 가설 — claude 데몬 재부모화 (미검증, 최우선 확인)

3절 2단계가 케이스 B/C에서 깨지는 **구체적 메커니즘** 후보다. 확정 아님.

2026-08-03 실측한 claude 프로세스 트리:

```
52052  PPID=1      claude daemon run --origin transient --spawned-by {"label":"claude",...,"pid":40738}
52121  PPID=52052  claude bg-pty-host --bg-pty-host /tmp/cc-daemon-501/...
52122  PPID=52052  .../ClaudeCode.app/Contents/MacOS/claude --bg-pty-host ...
52219  PPID=52122  .../claude/versions/2.1.220 --session-id ... --model fable --effort max
```

두 가지가 동시에 문제다:

1. **트리 전체가 PPID=1에 매달려 있다.** 사용자가 pane에서 `claude`를 쳐도 실제 에이전트
   프로세스가 pane 셸의 부분트리 밖에 생기면 `descendant_pids`가 영원히 못 찾는다.
2. **실제 에이전트 프로세스(52219)의 파일명이 `2.1.220`이다.** `classify`는 파일명이
   정확히 `claude`여야 하므로 이 행은 분류에 실패한다. 52121/52122는 파일명이 `claude`라
   통과하지만, `find_map`이라 **어느 행이 먼저 걸리느냐에 따라 결과가 달라진다** —
   "됐다 안됐다"의 유력한 설명이다.

또한 52219의 argv에는 `--model fable --effort max`가 **있다**. 즉 사용자가 맨손으로
`claude`를 쳐도 데몬이 값을 붙여준다. 4단계의 전역 기본값 폴백이 필요 없을 수도 있고,
반대로 잘못된 값을 읽을 수도 있다.

주의: 위 트리는 `cwd=/Users/jr/Desktop/Serenity`로, deppy pane이 아니라 별도 Claude Code
데스크톱 앱일 가능성이 있다. **deppy pane에서 직접 재현해 확인해야 한다.**

**검증 레시피** — deppy pane에서 `claude`를 띄운 직후:

```sh
SHELL_PID=<해당 pane 셸의 pid>
ps -eo pid,ppid,command | awk -v p=$SHELL_PID '$2==p'      # 직계 자식
pgrep -fl claude                                            # 전체 claude 프로세스
```

셸의 부분트리 안에 파일명이 정확히 `claude`인 행이 없으면 가설이 확정된다.

## 5. 구현 지침

### A. 조용한 실패를 끝낸다 (최우선, 나머지와 독립)

다섯 단계 중 어디서 끊겼는지 **화면에** 띄운다. 지금은 로그를 봐야만 알 수 있어서
진단 한 번에 왕복이 필요하고, 그게 이 작업이 여덟 라운드를 먹은 주된 이유다.

- 붙일 곳: 이미 있는 하단 상태줄(`ui/agent_terminal.rs`). 새 토스트 체계를 만들지 말 것.
- `web_notice`를 재활용하지 말 것 — web-remote 전용이라 의미가 안 맞는다.
- 문구는 단계별로 구분되어야 한다. 최소한 "이 pane에 에이전트가 없음"과
  "현재 강도를 모름"과 "작업 중이라 대기열에 넣음"은 서로 달라야 한다.

이것만 해도 이후 모든 제보가 2초 만에 분류된다.

### B. 케이스 C(codex 직접 입력)부터 고친다

4단계가 불필요해 변수가 가장 적다. 여기서 감지를 고치면 B에도 그대로 적용된다.
`classify`를 실제 프로세스 형태에 맞게 넓히되, **화이트리스트를 유지**하라 —
아무 프로세스나 에이전트로 오인하면 엉뚱한 셸에 `\x1b[1;2A`가 날아간다.

`find_map`이 순서 의존이라 여러 후보 중 아무거나 집는 것도 같이 손봐야 한다.
"가장 깊은 자손" 또는 "argv에 `--session-id`가 있는 행 우선" 같은 결정적 규칙이 필요하다.

### C. 케이스 D(구조화 세션)는 별건으로 분리

포커스가 터미널에 있을 때 구조화 세션을 어떻게 대상으로 삼을지는 UX 결정이 필요하다
(마지막 상호작용 세션? 패널 선택 유지?). PTY 문제와 섞지 말 것.

### D. 하지 말 것

- **`agent_sessions_ui.open()`을 게이트 실패 지점에서 부르지 말 것.** 원래 "패널에서
  고르라"는 안내였으나 지금 단축키는 포커스된 pane을 대상으로 하므로 방해일 뿐이고,
  사용자 화면을 덮어 진짜 원인을 가렸다. 2026-08-03에 두 군데서 제거했다.
- **Codex에 legacy Alt 인코딩(`\x1b,`)을 쓰지 말 것.** Codex는 kitty 프로토콜(`CSI > 7 u`)을
  켜므로 ESC 접두는 **진행 중인 턴을 중단시킨다**. CSI 화살표(`\x1b[1;2A/B`)만 쓴다.
  이를 고정하는 회귀 테스트가 이미 있다.
- **Codex에 `/model <slug>`를 보내지 말 것.** 슬래시 인자가 프롬프트가 되어 턴을 태운다.
  `pty_effort.rs`의 `supports_model = false`가 이걸 막고 있다.
- **승인 프롬프트에 슬래시를 쓰지 말 것.** y/n 답으로 먹힌다. 4절의 문구 화이트리스트
  (`waiting_is_idle_prompt`)가 유휴 프롬프트만 통과시킨다 — fail-closed를 유지할 것.

## 6. 미커밋 상태 (작업 시작 전 처리 필요)

워크트리 `~/deppy-worktrees/designall`에 커밋 안 된 변경이 있다. 전부 게이트 통과
(테스트 1,261건, clippy `-D warnings`, fmt):

1. `~/.claude/settings.json`의 `effortLevel`을 4단계 마지막 폴백으로 추가
   (`agent_model_catalog.rs`, `app.rs`).
2. 게이트 실패 시 에이전트 패널 자동 열기 제거 2곳 (`app.rs`).
3. hook `needsInput`의 **문구**로 유휴 프롬프트와 승인 프롬프트를 분리
   (`waiting_is_idle_prompt`, `slash_input_is_safe`, `hook_waiting_message`) + 테스트 2건.
   근거: DB `agent_needs_input` 행이 `waiting=1`,
   `message="Claude is waiting for your input"`인데 `merge_agent_status`가 이걸 승인
   대기와 같은 `NeedsApproval`로 접어, 프롬프트에서 놀고 있는 claude가 영원히
   "작업 중"으로 막혔다.
4. `shortcuts.rs` 테스트의 `type_complexity` 경고 정리(타입 별칭).

**이 변경들은 3절 4·5단계에 대한 부분 수정이고 1·2단계(감지)는 손대지 않았다.**
근본 원인이 감지라면 이것만으로는 안 고쳐진다.

## 7. 알려진 무관 이슈

`resource_monitor::tests::capture_timeout_kills_group_and_inherited_descendant_pipe_does_not_block`은
전체 실행에서 간헐 실패하는 기존 flake다(단독 실행 3회 연속 통과 확인). 별도 브랜치
`fix/freeze-flake`에서 다루는 중이며 이 작업과 무관하다.
