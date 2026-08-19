# exit 시 pane 닫힘 + pane 없이도 이어서 하기 (2026-08-19)

## 요구사항

두 개가 동시에 성립해야 한다.

1. **exit 누르면 pane이 그냥 닫힌다** — agent pane도 셸처럼.
2. **「이어서 하기」는 앱을 재시작해도 계속 동작한다** — pane이 이미 닫혀 있어도.

## 배경 — 왜 agent pane만 exit 후에도 열려 있었나

`crates/runtime/src/in_process.rs`의 `pump_sessions`는 셸 세션이 자연 종료
(SessionExited)하면 그 pane을 자동으로 닫는다(tmux 관례, 2026-07-05). agent
세션은 결과 배지(✅/❌)와 scrollback을 봐야 한다는 이유로 이 자동 닫힘에서
제외돼 있었다(`is_shell` 게이트).

그런데 실제 에이전트 실행은 전부 `crates/app/src/agent_launcher.rs`의
`wrap_agent_then_shell`로 감싸여 있다:

```sh
sh -c '"$@"; stty sane 2>/dev/null || true; unset DEPPY_AGENT_EXECUTABLE DEPPY_SHIM_GUARD; exec "${SHELL:-/bin/sh}"' deppy-agent-session <에이전트> <인자...>
```

에이전트가 끝나면 `exec`로 **같은 PTY 세션 안에서** 평범한 셸이 그 자리를
차지한다 — 세션 자체는 안 죽는다. 즉 **agent 세션의 SessionExited가 온다는
것 자체가 "그 폴백 셸에서 사용자가 이미 exit을 쳤다"는 뜻**이고, 그 시점에는
결과를 이미 pane에서 다 본 뒤다. "결과 배지·scrollback을 위해 유지"라는 원래
이유가 이 조건과 충돌하지 않는다 — shim이 세션을 에이전트 프로세스보다 오래
살리는 한 계속 성립한다.

## ① exit 시 agent pane도 닫는다

`pump_sessions`(`crates/runtime/src/in_process.rs`)의 pane 자동 닫힘 루프에서
`is_shell` 조건을 지웠다. 이제 셸이든 agent든 `SessionExited`가 오면 그 pane을
닫는다.

### SessionRestored를 어떻게 걸러냈나

`SessionRestored`(재시작 시 열람 전용으로 복원된 pane, `restore_pane` →
`restore_archived_pane`)는 **완전히 다른 코드 경로**에서 emit된다 — 이 루프가
도는 `exited_sessions`는 이번 tick `pump_sessions`가 만든 로컬 `events` Vec에서
`SessionExited`만 뽑은 것이라, `SessionRestored`가 여기 섞일 여지가 구조적으로
없다. 재조사하지 않고 지레짐작하지 않기 위해 한 단계 더 파고들었다:
`Session::restore_archived`가 만드는 세션은 `pty: None`으로 생성되고,
`Session::pump`의 `just_exited`는 `self.pty.is_some()`이 참일 때만 `true`가 될
수 있다(`crates/session/src/session.rs:238-330`). 즉 복원된 archived 세션은
**pty가 애초에 없어서** 다시 "방금 종료됨"으로 관측될 수 없다 — 타입 수준의
불변식이지 우연이 아니다.

이 불변식을 코드 주석으로 남기고, 실제로 여러 pump tick(150ms 실제 대기 +
`FocusPane`으로 새 `MuxUpdated` 유도)에 걸쳐 관찰하는 회귀 테스트
(`복원된_archived_agent_pane은_시간이_지나도_자동으로_닫히지_않는다`)로도
고정했다 — 나중에 누가 `exited_sessions` 판정 조건을 잘못 건드려도 이 테스트가
먼저 깨진다.

### 깨진 기존 테스트 3개와 수정 이유

- **`셸_exit시_pane_자동_닫힘_agent는_유지`** → `...agent도_동일하게_닫힌다`로
  개명·역전. agent 명령을 `sleep 5`(살려둠)에서 `true`(즉시 종료)로 바꿔 두
  세션 모두 pane이 닫힘을 검증한다.
- **`세션과_layout이_영속된다`** — "pane→session_id가 저장됐는지"를 exit **후**
  확인하던 부분이 이제 성립하지 않는다(pane 자체가 사라진다). exit 전(살아
  있는 동안, `attach_in_new_tab` 직후 `emit_mux_snapshot`이 이미 `save_layout`을
  거친 시점)으로 그 확인을 옮기고, "exit 상태가 영속되는지"는 그대로 exit 후에
  별도로 확인한다 — 두 관심사를 분리했다.
- **`재시작시_agent_pane은_열람전용으로_복원된다`** — 이전엔 "agent 실행 →
  자연 종료(SessionExited) → 워커 재시작"으로 픽스처를 만들었는데, 이제
  자연 종료 시점에 pane이 바로 닫혀 mux_panes 행이 지워지므로 재시작해도
  복원할 게 없다. 이 기능이 **실제로** 의미를 갖는 시나리오를 다시 따져보면:
  진짜 agent는 wrap_agent_then_shell로 감싸여 있어 SessionExited가 오는 시점엔
  이미 사용자가 그 pane에서 exit을 친 뒤다 — 그때는 셸처럼 pane이 사라지는 게
  맞고, 다시 열람 전용으로 살아날 필요도 없다. "재시작 시 열람 전용 복원"이
  진짜 필요한 경우는 **pane이 아직 열려 있는 도중 앱이 통째로 꺼진 경우**
  (정상 종료든 비정상 종료든) 뿐이다. 그래서 세션을 절대 exit시키지 않고
  `sleep 30`으로 살려 둔 채 워커(client)를 drop해 그 상황을 흉내내도록 픽스처를
  바꿨다. 아카이브 기록은 워커 종료 루프가 "아직 running인 agent"도 예외 없이
  기록하므로(§14.3, running 셸만 제외) exit을 기다리지 않아도 아카이브가
  남는다.
- **`종료_후에도_scrollback_열람_가능`** →
  `복원된_archived_agent_pane도_scroll로_스크롤백을_볼_수_있다`로 개명.
  §14.3 "종료 후에도 scrollback 열람 가능" 계약은 이제 "살아 있는 pane으로 exit
  직후 관찰"로는 검증할 수 없다 — 검증 시도(자연 종료를 기다리는 것) 자체가 그
  pane을 없앤다. 이 계약이 실제로 남아 있는 유일한 자리는 재시작 시 열람
  전용으로 복원된 archived pane이다(위와 같은 이유로 pty가 없어 자동 닫힘
  대상이 아니다) — 그 자리로 옮겨 `Scroll`이 여전히 `Viewport`로 응답하는지
  고정했다.

## ② pane 없이도 이어서 하기

### 막힌 지점 — 왜 기존 경로를 그대로 못 쓰나

기존 「이어서 하기」(`dispatch_respawn_archived_agent`)는 두 가지를 전제한다.

1. `runtime::in_process::respawn_archived_agent`가 **살아 있는 pane**(`self.mux.panes`에서
   그 `session`을 참조하는 pane)을 먼저 찾는다 — 없으면 즉시 실패.
2. App 쪽 `archived_resume_targets_from_mux`가 쓰는
   `storage::ArchivedAgentResumeRow`(`self.archived_agent_resume`)는
   `ARCHIVED_AGENT_RESUME_SELECT`(`crates/storage/src/db.rs`)가 `sessions`를
   `mux_panes`와 INNER JOIN해서 만든다 — pane이 DB에서 사라지면 이 투영에서도
   빠진다.

①에서 pane이 닫히면 `save_layout`(`crates/persist/src/repo.rs`)이 그 pane의
`mux_panes` 행을 DELETE+재삽입 방식으로 지운다. 즉 pane이 닫히는 순간
**두 전제 모두 무너진다** — 살아 있는 세션도 없고, DB 투영도 더는 이 세션을
찾지 못한다.

### 버린 대안

- **`respawn_archived_agent`가 pane 없이도 동작하게 확장** — 이 함수는 persisted
  `sessions` 행(정확한 원래 command/args/cwd/regex)을 그대로 재사용해 가장
  정확하다. 하지만 그러려면 `crates/runtime`에 "mux_panes 없이 sessions.id로
  직접 조회" 경로를 새로 만들어야 하는데, 그 `persistent_id`(durable UUID)를
  App이 pane 없는 work history 행에서 얻을 방법이 없었다: `AgentWorkTurnRow`는
  `pane_id`(durable 문자열이지만 mux_panes가 지워지면 참조할 데이터가 없다)만
  갖고, `archived_agent_resume`도 위와 같은 이유로 비어 있다. `crates/storage`의
  JOIN을 LEFT JOIN으로 바꿔 이 경로를 살릴 수도 있었지만, `crates/storage`는
  내 소유 영역(`crates/runtime/**`, `crates/app` 일부)이 아니고 다른 에이전트가
  동시 작업 중이라 건드리지 않았다.
- **`SettingsJobAction::PrepareQuickAgentLaunch`(비동기 job queue) 경로 재사용** —
  Agent Launcher UI의 "실행" 버튼이 실제로 타는 전체 경로(`build_launch_spec` →
  `PrepareQuickAgentLaunch` → `prepare_agent_launch` → `agent_configs` DB
  upsert/조회 → `SpawnAgent`)를 그대로 재사용하는 안도 검토했다. 그러나 이
  경로는 `pending_agent_launcher_launch`라는 **단일 슬롯**(동시 1개 launch만
  허용)을 공유하고, `extra_arg: Option<String>`은 위치 인자 하나만 붙일 수
  있어(배치 스폰의 초기 프롬프트용) 이어가기 인자(`["resume", "<id>"]`처럼
  2개짜리)를 표현할 수 없다. work history activation 배선 범위를 넘어 Agent
  Launcher 소유 코드까지 건드려야 해서 포기했다.

### 실제로 쓴 재료

`self.restore_agents: HashMap<String(pane_id), storage::AgentSessionRow{pane_id,
kind, session_id}>`(app.rs)가 로드하는 `AGENT_SESSIONS_BOUNDED_SELECT`
(`crates/storage/src/db.rs`)는 `mux_panes`와 **JOIN하지 않는다** —
`SELECT * FROM agent_sessions WHERE workspace_id = ?1` 그대로다. pane이 닫혀도
이 바인딩(agent 종류 + native session id)은 그대로 살아 있다 — 작업 지시서의
"(d) 재료는 이미 있다"가 가리키던 게 바로 이것이다. 여기에 이력 행의 `kind`
(`launcher_kind_from_history`로 built-in `AgentKind`까지 복원 가능)를 더하면,
mux/pane 상태 없이도 CLI 플래그를 판정할 수 있다.

### 설계

1. **순수 함수 `resume_without_pane_plan(kind, binding)`**(app.rs) — `kind`는
   built-in `stable_config_id()`(항상 `ResumeProvider::from_agent_id`에
   매칭되도록), `binding`은 `self.restore_agents.get(&row.pane_id)`에서 뽑은
   `(native_kind, native_session_id)`. `agent_resume::resume_plan`에 그대로
   위임한다 — CLI별 정확한 플래그 표(`--resume`/`resume <id>`/`--session` 등)를
   여기서 다시 만들지 않는다. `ResumeMode::Exact`만 `Some(extra_args)`로
   인정하고, `RecentInCwd`(바인딩이 없거나 무효할 때의 강등 결과)는 `None` —
   "이 turn을 이어간다"는 약속은 정확한 native session id가 있을 때만 지킬 수
   있어서다.
2. **`AppWorkHistoryActivation::ResumeArchivedNoPane { kind, extra_args }`** —
   판정(`resolve_work_history_activation`)이 `resume_without_pane_plan`을
   호출해 `extra_args`를 그 자리에서 확정해 들고 다닌다. 실행 시점에 다시
   계산하지 않아 판정↔실행 사이에 상태가 어긋날(TOCTOU) 여지가 없다.
   `.presentation()`은 기존 `ResumeArchived`(살아 있는 pane 경로)와 똑같이
   `Resume` 버튼으로 매핑한다 — 사용자에게는 구분 없이 그냥 "이어서 하기"다.
3. **`dispatch_resume_archived_agent_new_pane(kind, extra_args)`**(app.rs) —
   Agent Launcher가 새 실행에 쓰는 `build_launch_spec`(executable 감지, shim
   배선, `wrap_agent_then_shell`)을 그대로 호출해 `LaunchSpec`을 얻고,
   `into_parts()`로 풀어낸 `args` 뒤에 `extra_args`를 붙인 뒤
   `RuntimeCommand::SpawnAgent`로 보낸다. `SpawnAgent`는 이미 `attach_in_new_tab`
   으로 **새 pane**을 만들어 붙이므로 별도 pane 생성 로직이 필요 없다. 모델/
   강도는 재지정하지 않는다(빈 model, effort 없음) — 이력 행에는 원래 세션의
   정확한 model/effort가 없고, CLI가 이어받는 대화 자체가 이미 자기 모델을
   기억한다.

`resolve_work_history_activation`은 기존 살아 있는 pane 루프(Focus/ResumeLive/
ResumeArchived)를 그대로 두고, 그 루프가 아무것도 못 찾았을 때만(즉 pane이
없을 때만) `resume_without_pane_plan`을 확인한다 — 살아 있는 pane이 있으면
여전히 그쪽이 우선한다(더 정확한 원래 command/args를 쓰므로).

### 명령 조립 중복을 어떻게 피했나

CLI별 이어가기 플래그(provider 판정, exact/recent 인자 표)는
`crates/app/src/agent_resume.rs::resume_plan` **한 곳**에만 있다.
`dispatch_respawn_archived_agent`(살아 있는 pane 경로, `archived_resume_target`
경유)와 `resume_without_pane_plan`(이번 새 경로) 둘 다 이 함수를 호출만 할 뿐,
직접 만들지 않는다. `agent_resume.rs` 자체는 건드리지 않았다 — 이미 필요한
인터페이스(`agent_id: &str`, `binding: Option<(&str, &str)>`)를 제공하고 있었고,
빈/미인식 `agent_id`를 넘겨도 `binding`만으로 정확한 provider를 추론하는 동작이
이미 테스트로 고정돼 있었다(`custom_agent는_검증된_binding이_있을_때만_
exact_resume한다`).

새 pane 생성/spawn 자체는 기존 `RuntimeCommand::SpawnAgent`(수정 없음)와
`build_launch_spec`(수정 없음, `AgentLauncherIntent::Launch` 핸들러가 쓰는
것과 동일 함수)을 그대로 재사용했다 — `crates/runtime`에 새 커맨드를 추가하지
않았다.

### 「새로 실행」으로 떨어질 때 이유 표시

`resolve_work_history_activation`에서 `resume_without_pane_plan`이 `None`을
돌려주면(바인딩이 없거나 provider가 안 맞거나 native session id가 무효) 기존과
동일하게 `NewRun(kind)`으로 떨어진다. 이 지점에 도달했다는 것 자체가 이제
"정확히 이어갈 근거가 없다"는 뜻이 됐으므로(예전엔 여러 이유로 NewRun에
도달했지만, ②를 넣은 뒤로는 이 한 가지 이유로만 도달한다), 「새로 실행」
버튼에 hover 안내를 붙였다(`crates/app/src/ui/work_history.rs`,
`history.action.new_run_hint` — 5개 로케일 전부에 새 키 추가, i18n-check로
누락 없음을 확인).

## 테스트

`crates/runtime/src/in_process.rs`(①):

- `셸_exit시_pane_자동_닫힘_agent도_동일하게_닫힌다` — 셸·agent 둘 다 자연
  종료 시 pane이 닫힘.
- `복원된_archived_agent_pane은_시간이_지나도_자동으로_닫히지_않는다`(신규
  회귀 가드) — 복원된 pane이 여러 pump tick 뒤에도(150ms 실제 대기 +
  `FocusPane`으로 새 스냅샷 유도) 그대로 남아 있음.
- `복원된_archived_agent_pane도_scroll로_스크롤백을_볼_수_있다`(옛
  `종료_후에도_scrollback_열람_가능` 대체) — §14.3 계약이 재시작 복원 pane
  자리에서 성립.
- `세션과_layout이_영속된다`, `재시작시_agent_pane은_열람전용으로_복원된다`
  — 위 "깨진 기존 테스트" 절 참고, 새 동작에 맞게 픽스처/확인 시점을 고쳤다.

`crates/app/src/app.rs`(②) — `resume_without_pane_plan` 순수 함수:

- `...exact_인자를_돌려준다` — 정확한 native session id → Exact 인자.
- `...바인딩이_없으면_none이다` — 바인딩 부재 → RecentInCwd로 강등 → None.
- `...provider가_다른_바인딩을_무시하고_none이다` — kind 불일치(오래된/잘못된
  결속) → None.
- `...무효한_native_session_id를_none으로_떨어뜨린다` — 빈 문자열/개행/1025
  바이트 초과 세 경우 모두 None.
- `...builtin_kind별로_올바른_exact_플래그를_고른다` — claude/codex/kimi 세
  provider의 정확한 플래그 표 확인(agent_resume의 표를 그대로 위임하는지).

`crates/app/src/ui/work_history.rs`(②) — 기존 51개 테스트(kittest 클릭
히트테스트 포함) 전부 그대로 통과 — hover tooltip 추가가 클릭/접근성 라벨에
영향 없음을 확인. tooltip **문구 자체**를 검증하는 새 테스트는 추가하지
않았다 — 이 파일에 이미 있는 다른 hover 안내(`history.action.show_diff_hint`
등) 어느 것도 문구 자체를 kittest로 검증하지 않는 기존 관례를 따랐다(마우스
hover 시뮬레이션 없이 접근 가능한 검증 수단이 없었다).

`resolve_work_history_activation`/`dispatch_resume_archived_agent_new_pane`
자체(App 인스턴스 메서드)는 직접 테스트하지 않았다 — 이 파일에는 애초에 `App`
전체를 생성해 인스턴스 메서드를 직접 호출하는 테스트가 하나도 없다(기존
`dispatch_respawn_archived_agent`/`resolve_work_history_activation`도 이번
작업 전까지 마찬가지였다). 이 코드베이스의 관례대로 판단 로직은 순수 함수로
빼서(`resume_without_pane_plan`) 그것만 철저히 테스트했다.

## 게이트 결과

1. `cargo test -p deppy-sijo` — 1767 passed, 0 failed, 8 ignored(플랫폼 실측
   전용, 기존과 동일).
2. `cargo test -p runtime` — 276 passed, 0 failed.
3. `cargo clippy --workspace --all-targets -- -D warnings` — 0 경고.
4. `cargo run -q -p xtask -- check-boundary` — OK("zero allowlist capability").
5. `cargo run -q -p xtask -- i18n-check` — OK(리터럴 키 호출 1007건을 로케일
   5개와 대조, 동적 키 61건은 기존 문서화된 예외). `cargo test -p i18n` —
   8 passed(그 중 `all_bundled_locales_match_fallback_order_and_placeholders`가
   5개 로케일 키 정렬/placeholder 일치를 확인).
6. `cargo fmt --all -- --check` — 최초 실행에서 이번에 새로 추가한 코드 3곳
   (app.rs 1곳, work_history.rs 1곳, in_process.rs 1곳)에 포맷 드리프트가
   있어 `cargo fmt --all`로 정리했다. 재실행 결과 0건.

## 커밋

- `3305e63` fix(runtime): exit 시 agent pane도 셸처럼 닫는다 (①)
- `7482b4a` feat(app): 살아 있는 pane 없이도 work history에서 이어서 하기 (②)

## 화면에서 검증하지 못한 것

지시에 따라 앱을 빌드해 실행하지 않았다(사용자의 앱이 떠 있음). 따라서 아래는
코드 추적 + 단위 테스트로만 확인했고, 실제 화면으로 재현·검증하지 못했다:

- exit을 실제로 쳤을 때 agent pane이 눈으로 봐도 즉시 접히고 이웃 pane이
  공간을 차지하는지(레이아웃 애니메이션/포커스 이동 포함).
- work history 카드에서 pane이 닫힌 뒤 「이어서 하기」를 눌렀을 때 실제로 새
  pane이 뜨고, 그 안에서 CLI(`claude`/`codex`/`kimi`)가 정말 이전 대화의
  맥락을 이어서 응답하는지 — 이건 CLI 자체의 `--resume`/`resume <id>` 동작에
  달려 있고, 여기서는 "정확한 플래그로 정확한 native session id를 넘겼다"까지만
  코드/테스트로 보장한다.
- `history.action.new_run_hint` tooltip이 실제 hover에서 잘림 없이, 다른
  요소와 겹치지 않고 보이는지(레이아웃 확인).
- `dispatch_resume_archived_agent_new_pane`이 `agent_config_id`로 넘기는
  `kind.stable_config_id()`(예: `deppy-builtin-codex`)에 대응하는
  `agent_configs` 행이 실제로 매 경우 존재하는지. 이 행이 없으면 FK 제약으로
  `persist::upsert_session`이 실패하지만, `session_spawned`은 fail-soft라
  (경고 로그만 남기고) **살아 있는 세션 자체는 정상 동작한다** — 다만 이번
  spawn은 DB에 영속되지 않아 다음 재시작에서 사라진다. `dispatch_
  respawn_archived_agent`(기존 경로)도 동일한 fail-soft 성격을 이미 갖고
  있어 새로 만든 위험은 아니지만, 실제로 이 행이 항상 미리 존재하는지는
  (Agent Launcher 최초 실행 시 `db.upsert_builtin_agent_config`가 언제
  호출되는지 전체 경로를 다 추적하지 않아) 실기기로 확인하지 못했다.
- `SpawnAgent`는 항상 워크스페이스 현재 cwd에서 뜬다(cwd 인자가 없다) —
  원래 대화가 다른 폴더에서 진행됐다면 그 폴더가 아니라 워크스페이스 cwd에서
  재개된다. 이는 「새로 실행」도 이미 갖고 있던 기존 제약이라 새로 만든
  회귀는 아니지만, 사용자 체감상 아쉬울 수 있어 기록해 둔다.
