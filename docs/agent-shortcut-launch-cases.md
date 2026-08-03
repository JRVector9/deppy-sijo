# 강도·모델 단축키: 에이전트 실행 경로 전수와 구현 지침

작성·검증 2026-08-03. `⌃⇧↑/↓`(추론 강도), `⌃⇧←/→`(모델)이 **띄우는 방식에 따라
되다 안 되다** 하는 문제의 조사 기록과 구현 지침.

## 0. 한 줄 요약

단축키 수신은 정상이다. 21:02 실패 로그는 실제로 빈 셸에서 누른 것이어서 감지 실패가
아니었다. 직접 Codex의 분류와 현재값 없는 키 계획도 회귀 테스트를 통과했다. 확인된
결함은 파이프라인이 실패 이유를 알고도 로그만 남겨 사용자가 "에이전트가 없음"과
"버그"를 구분하지 못한 것이다. 이 실패 이유를 하단 상태바에 노출한다.

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

**직접 Codex의 현재 프로세스 형태는 이 분류 규칙에 들어온다.**
`transcript_없이도_프로세스로_종류를_잡는다`는 `/Users/jr/.local/bin/codex --enable hooks`를
Codex로 분류하고, `codex는_현재_강도를_몰라도_키를_보낸다`는 현재 강도 없이 CSI 키를
만드는 것을 각각 1/1로 통과했다. 실제 실패 표본 없이 분류기를 넓히면 오탐으로 다른
셸에 키를 보낼 위험만 커진다.

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

단축키 한 번이 동작하려면 전부 성공해야 한다. 변경 전에는 다섯 단계가 모두 조용했지만,
현재는 아래처럼 상태바에서 구분한다.

| 단계 | 위치 | 실패 시 상태바 피드백 |
|---|---|---|
| 1. pid 있는 리소스 행 | `session_resource_usage` (2초 주기) | pane 프로세스 정보를 기다리는 중 |
| 2. 프로세스를 claude/codex로 분류 | `agent_detect::classify` | 이 pane에서 에이전트를 찾지 못함 |
| 3. 표면 생성 | `app.rs:pty_agent_surfaces` | 에이전트는 감지했지만 제어 준비 안 됨 |
| 4. 현재 값 파악 | 4단 폴백 (아래) | 에이전트의 현재 값을 알 수 없음 |
| 5. 상태 게이트 | `slash_input_is_safe` | 강도/모델 목표값이 예약됐음을 지속 표시 |

4단계의 현재 폴백 체인(`app.rs:11430-11470`), 순서대로:
낙관적 값(`pty_agent_pending`) → statusLine → argv → `~/.claude/settings.json` 전역 기본값.

케이스별로 어디서 깨지는가:

- **A** — 2·3단계만 통과하면 4단계는 argv가 보장한다. 가장 튼튼.
- **B** — argv가 비어 4단계가 전역 기본값에 의존한다. statusLine은 1시간 만료
  (`STATUSLINES_PREFIX_PREFLIGHT`)라 오래 유휴한 세션에선 사라진다.
- **C** — codex는 상대 키(`\x1b[1;2A/B`)를 쓰므로 **4단계가 통째로 불필요하다.**
  현재 직접 실행 argv의 분류와 키 계획은 테스트로 통과했다. 이후 실패 제보는 상태바가
  1·2·3단계 중 실제로 끊긴 곳을 곧바로 구분한다.
- **D** — `selected_surface_snapshot()`가 패널 선택을 읽는데, 포커스가 터미널 pane에
  있으면 대상이 빈다. PTY 폴백은 `AgentSurfaceId::Pty`만 찾으므로 구조화 세션을 못 집는다.
  `AgentCapabilities::for_transport`는 AppServer에만 `effort_control`/`model_control`을
  주므로(`agent_surface.rs:139`) 능력은 있는데 **대상 선택이 없다.**

## 4. 검증 결과 — claude 데몬 재부모화 가설은 이 pane에서 틀렸다

3절 2단계가 케이스 B/C에서 깨지는 구체적 메커니즘 후보였으나 실제 Deppy pane의
프로세스 트리로 반증됐다.

처음 관찰한 다음 트리는 Deppy pane이 아니라 별도 Claude/Desktop 세션이었다.

```
52052  PPID=1      claude daemon run --origin transient --spawned-by {"label":"claude",...,"pid":40738}
52121  PPID=52052  claude bg-pty-host --bg-pty-host /tmp/cc-daemon-501/...
52122  PPID=52052  .../ClaudeCode.app/Contents/MacOS/claude --bg-pty-host ...
52219  PPID=52122  .../claude/versions/2.1.220 --session-id ... --model fable --effort max
```

이 트리만 보면 두 가지가 동시에 문제처럼 보인다.

1. **트리 전체가 PPID=1에 매달려 있다.** 사용자가 pane에서 `claude`를 쳐도 실제 에이전트
   프로세스가 pane 셸의 부분트리 밖에 생기면 `descendant_pids`가 영원히 못 찾는다.
2. **실제 에이전트 프로세스(52219)의 파일명이 `2.1.220`이다.** `classify`는 파일명이
   정확히 `claude`여야 하므로 이 행은 분류에 실패한다. 52121/52122는 파일명이 `claude`라
   통과하지만, `find_map`이라 **어느 행이 먼저 걸리느냐에 따라 결과가 달라진다** —
   "됐다 안됐다"의 유력한 설명이다.

또한 52219의 argv에는 `--model fable --effort max`가 **있다**. 즉 사용자가 맨손으로
`claude`를 쳐도 데몬이 값을 붙여준다. 4단계의 전역 기본값 폴백이 필요 없을 수도 있고,
반대로 잘못된 값을 읽을 수도 있다.

실제 실행 중인 Deppy PID `36159`에서 직접 Claude가 떠 있던 pane의 하위 트리는 다음과
같았다.

```text
36159  ./target/debug/deppy-sijo
├─ 37537  /Users/jr/.local/bin/codex app-server --listen stdio://
└─ 56134  /bin/zsh                         (pane shell, ttys024)
   └─ 65361  claude                        (direct child of pane shell)
```

`claude`는 pane 셸 `56134`의 자손이며 실행 파일명도 정확히 `claude`다. PPID=1의
`52052 → 52121/52122 → 52219` 트리는 다른 세션이라 감지 대상이 아니었다. 따라서 이
측정으로 `descendant_pids`를 데몬 트리까지 넓히거나 버전 번호 실행 파일을 Claude로
오인시키는 변경은 정당화되지 않는다.

재검증 레시피 — deppy pane에서 `claude`를 띄운 직후:

```sh
SHELL_PID=<해당 pane 셸의 pid>
ps -eo pid,ppid,command | awk -v p=$SHELL_PID '$2==p'      # 직계 자식
pgrep -fl claude                                            # 전체 claude 프로세스
```

셸의 부분트리 안에 파일명이 정확히 `claude`인 행이 없을 때만 이 가설을 다시 열어라.

## 5. 구현 지침

### A. 조용한 실패를 끝낸다 (구현·검증 완료)

다섯 단계 중 어디서 끊겼는지 **화면에** 띄운다. 지금은 로그를 봐야만 알 수 있어서
진단 한 번에 왕복이 필요하고, 그게 이 작업이 여덟 라운드를 먹은 주된 이유다.

- 붙일 곳: 이미 있는 하단 상태줄(`ui/agent_terminal.rs`). 새 토스트 체계를 만들지 말 것.
- `web_notice`를 재활용하지 말 것 — web-remote 전용이라 의미가 안 맞는다.
- 문구는 단계별로 구분되어야 한다. 최소한 "이 pane에 에이전트가 없음"과
  "현재 강도를 모름"과 "작업 중이라 대기열에 넣음"은 서로 달라야 한다.

구현은 `AgentShortcutFeedback`을 하단 상태바에 4초 동안 표시한다. 포커스 없음,
프로세스 정보 대기, 에이전트 미감지, 표면 준비 대기, 현재값 미상, 미지원, 대상 소멸,
전송 실패를 구분한다. 작업 중 큐 적재는 기존 `status_bar.queued_effort/model`이 목표값과
함께 지속 표시한다. 새 토스트나 `web_notice`는 만들지 않았다.

### B. 케이스 C(codex 직접 입력)부터 검증한다

4단계가 불필요해 변수가 가장 적다. 여기서 감지를 고치면 B에도 그대로 적용된다.
현재 직접 Codex 형태는 기존 `classify`에 이미 들어오며 집중 테스트가 통과했다. 실제로
분류되지 않는 argv와 pane 셸 하위 트리를 포착하기 전에는 규칙을 넓히지 말라 — 아무
프로세스나 에이전트로 오인하면 엉뚱한 셸에 `\x1b[1;2A`가 날아간다.

실제 pane에서 복수 후보 때문에 오분류된 표본이 잡히면 그때 `find_map`의 순서 의존도
함께 다뤄라. 후보 우선순위는 "가장 깊은 자손"이나 `--session-id` 존재 여부 같은 측정
가능한 규칙으로 고정해야 하며, 현재의 다른 세션 트리만 근거로 바꾸지는 않는다.

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

## 6. 선행 부분 수정 상태

아래 선행 변경은 `a1dcabd`에 커밋되어 있다.

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
이번 조사에서는 감지 결함을 재현하지 못했고, 당시 표본은 빈 셸이었다. 대신 1~4단계와
전송 실패를 상태바에서 구분해 다음 실제 제보가 증거를 남기도록 했다.

## 7. 이번 변경의 검증 기록

- RED: 상태바 피드백 테스트는 `AgentShortcutFeedback`/표시 메서드가 없어 컴파일 실패.
- RED: 단계 분류 테스트는 `pty_shortcut_missing_feedback`이 없어 컴파일 실패.
- RED: 렌더 경계 테스트는 `pty_agent_surfaces`의 동기 Claude 설정 파일
  읽기를 검출해 실패. 설정 읽기를 기존 lazy 런처 worker로 옮기고, 새 직접
  실행 Claude가 감지될 때만 스냅샷을 갱신하도록 한 뒤 GREEN.
- GREEN: 단축키 집중 실행 3/3 통과(새 테스트 2개 포함).
- 직접 Codex 근거: 프로세스 분류 1/1, 현재 강도 없는 키 계획 1/1 통과.
- i18n: `cargo run -p xtask --locked -- i18n-check` 통과(카탈로그 7/7 포함).
- 전체: `cargo test -p deppy-sijo --locked -- --test-threads=1`에서 단위 1,265 통과,
  7 ignore; 통합 4/4, 5/5, 14/14, 15/15 통과; 하드웨어 전용 통합 3 ignore.
- 정적 검사: `cargo clippy -p deppy-sijo --all-targets --locked -- -D warnings` 통과.
- 포맷/whitespace: `cargo fmt --all --check`, `git diff --check` 통과.

## 8. 알려진 무관 이슈

`resource_monitor::tests::capture_timeout_kills_group_and_inherited_descendant_pipe_does_not_block`의
기존 flake는 별도 작업에서 처리됐으며 이 단축키 변경에는 관련 코드가 없다.
