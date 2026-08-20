# 비활성(warm) 워크스페이스 행에서도 「이어가기」가 되게 한다 (2026-08-20)

## 배경 — 왜 지금은 버튼이 안 뜨는가

사이드바는 비활성 워크스페이스의 세션도 나열한다(`app.rs`의
`if workspace.id == active_workspace_id { continue; }` 루프). 그 행의
「이어가기」는 코드 리뷰(2026-08-20)로 **의도적으로 꺼져 있었다** —
`entry.resumable = false;`로 못박혀 있고, 그 자리 주석이 이유를 적고 있었다.
이유는 두 겹이다.

1. **데이터가 없었다.** `self.restore_agents`는 활성 워크스페이스 한 곳만
   담는다(`request_agent_state_scope`가 `self.active.id` 하나만 싣는다).
   storage 쿼리(`AGENT_SESSIONS_BOUNDED_SELECT`)도
   `WHERE workspace_id = ?1`이다.
2. **실행부가 활성 전용이었다.** `WorkspaceControllerAction::ResumeAgent`
   핸들러가 `self.restore_agents`를 보고 없으면 `false`를 반환할 뿐,
   **워크스페이스를 전환하지 않았다.**

이 문서는 그 둘을 채워 비활성 행의 「이어가기」를 실제로 동작하게 만든 작업을
기록한다.

## ① 워크스페이스별 재개 가능 여부 — 전 워크스페이스 스코프 쿼리

### 선례를 그대로 따랐다

같은 문제(활성 전용 캐시로는 비활성 워크스페이스의 상태를 못 본다)를 이미
`global_waiting`/`global_working`/`global_turn_done`이 풀어 놨다 — `agent_needs_input`의
`session_key`(`workspace_id:session_id`)를 **워크스페이스 필터 없이** 전역으로
읽고, `parse_session_key`로 워크스페이스를 되뽑아 활성용/전역용으로 나눠
담는다. `agent_sessions` 테이블에는 그런 합성 키가 없지만(별도
`workspace_id`/`pane_id` 컬럼), 같은 전역·유계 패턴은 그대로 옮길 수 있다 —
가장 가까운 선례는 사실 `ACTIVITY_PANES_BOUNDED_*`(`crates/storage/src/db.rs`)다:
`mux_panes`를 워크스페이스 필터 없이 통째로 읽어 `(workspace_id, pane_id, ...)`를
LIMIT+tie-breaker로 뽑는다. `AGENT_SESSIONS_GLOBAL_BOUNDED_PREFLIGHT`/`_SELECT`는
이 두 선례를 합친 것 — `ACTIVITY_PANES_BOUNDED_*`처럼 전역·pane 단위이고,
`global_waiting` 계열처럼 "존재 여부만" 필요하다는 최소 원칙을 따른다(실제
`kind`/`session_id`는 담지 않는다 — 전환 후 `restore_agents`가 다시 정확히
읽는다).

### 유계를 어떻게 지켰나

- **PREFLIGHT+SELECT 짝**: 기존 관례(`AGENT_SESSIONS_BOUNDED_PREFLIGHT`와 동일한
  `WITH selected AS MATERIALIZED (...) LIMIT ?`, tie-breaker로
  `substr(CAST(... AS BLOB), 1, ?)` + `rowid`)를 그대로 따랐다. preflight가
  COUNT/invalid_rows/retained_bytes/max_row_bytes를 먼저 검사하고, 통과해야만
  SELECT를 돈다.
- **LIMIT**: `AGENT_SESSION_ROWS_MAX`(256, 기존 워크스페이스 스코프 조회와 동일
  상수)를 재사용했다 — 이 기능은 warm 워크스페이스 사이드바 행 전용이고, warm
  워크스페이스 수 자체가 `max_live_warm` 설정으로 이미 훨씬 작게 상한 잡혀
  있어 새 상수를 만들 필요가 없었다.
- **바이트 상한**: `checked_agent_state_vec_allocation` +
  `checked_agent_state_string_capacity`를 `AgentStateSnapshot::retained_bytes`에
  추가해 기존 필드들과 동일하게 총 유계 예산에 들어가게 했다.
- **opt-in 플래그**: `AgentStateJob.include_global_agent_sessions`(기본
  `false`)를 새로 추가해, `AgentStateSection::Restore` 프로젝션이 돌 때만
  이 전역 쿼리를 태운다 — `BindingSync`(바인딩 쓰기 후 재조회, 활성
  워크스페이스만 필요) 등 다른 `include_agent_sessions` 사용처는 이 비용을
  지지 않는다.
- **테스트로 상한 자체를 검증**: `agent_state_global_agent_sessions는_상한을_넘지_않는다`가
  `AGENT_SESSION_ROWS_MAX`(256)개 워크스페이스에 pane을 하나씩 심어 정확히
  256개가 돌아옴을 확인하고, 워크스페이스 하나에 pane을 하나 더 얹어(전역
  257개, 단일 워크스페이스 자체 상한 256은 아직 안 넘김) `BOUNDED_READ_LIMIT_EXCEEDED`로
  통째로 거부됨을 확인한다 — "전역 상한이 워크스페이스별 상한의 합이 아니라
  진짜 전역"이라는 계약을 고정했다.

### App 쪽 배선

- `AgentStateSnapshot.global_agent_sessions: Vec<(String, String)>`
  (workspace_id, pane_id)을 새 필드로 추가.
- `AgentStateSection::Restore` 핸들러(`app.rs`)가 이 필드를
  `self.global_resumable_panes: HashSet<(String, String)>`로 통째로
  재구성한다 — `global_waiting`과 동일 관례(스냅샷마다 전체 교체).
- 사이드바의 비활성 행 루프에서
  `entry.resumable = entry.agent_line.is_none() &&
  self.global_resumable_panes.contains(&(workspace.id.clone(), entry.pane.0.clone()))`로
  판정한다 — 활성 행이 `self.restore_agents.contains_key(&entry.pane.0)`를 쓰는
  것과 구조적으로 대칭이고, `workspace.id`로 명시적으로 스코프를 좁혀 다른
  워크스페이스의 pane_id와 섞이지 않게 했다(`pane_id`는 UUID라 사실상 전역
  유일하지만, 계약으로 쌍을 강제한다).

## ② 「전환 후 재개」 경로

### 선례

`WorkspaceControllerAction::FocusPty`가 `if workspace_id != self.active.id {
self.switch_workspace(&workspace_id); }`를 한다 — 그 관례를 그대로 `ResumeAgent`에
옮겼다. 이를 위해 `WorkspaceControllerAction::ResumeAgent`와
`ui::file_tree::SidebarAction::ResumeAgent`에 `workspace_id: String` 필드를
추가했다(사이드바 행의 `SessionRowTarget::workspace_id()`에서 채운다 — 이미
있던 값이라 새 배선이 필요 없었다).

### 지연 실행 — 가장 위험한 지점

`switch_workspace` 직후에는 새 활성 워크스페이스의 `restore_agents`가 **아직
안 채워져 있다**(agent state worker가 비동기로 채운다 —
`AgentStateSection::Restore` 프로젝션이 한 틱 뒤에나 도착한다). 그 상태에서
바로 `stage_agent_resume`을 부르면 조용히 `false`로 떨어져 아무 일도 안
일어난다 — 지금까지의 증상과 똑같아진다.

기존에 이미 있는 지연 실행 관례 세 가지(`pending_focus`,
`resume_probe_pending_panes`, `pending_dotenv_continuation`)를 검토했다.
가장 가까운 것은 `pending_focus`/`PendingPaneFocus`다 — "전환 직후 아직
준비 안 된 대상을 매 프레임 폴링하다가, 준비되면 실행하고, 대상이 바뀌면
조용히 버린다"는 정확히 같은 모양이다. 그래서 새 타입을 만들었다.

```rust
struct PendingResumeAgent {
    workspace_id: String,
    runtime_instance: u64,
    pane_key: String,
    title: String,
    session: runtime::SessionId,
    requested_at: std::time::Instant,
}
```

- `WorkspaceControllerAction::ResumeAgent` 핸들러가 활성 워크스페이스면 즉시
  `stage_agent_resume`을 부르고(기존과 동일한 빠른 경로), 비활성 워크스페이스면
  `switch_workspace`한 뒤 `self.pending_resume_agent`에 담아 둔다.
- `poll_pending_resume_agent`(신규)가 `poll_pending_workspace_focus` 바로 뒤,
  같은 `logic()` 틱에서 돈다 — `poll_agent_state_worker`(같은 틱에서 이보다
  먼저 돌아 `restore_agents`/`restore_loaded_for`를 갱신한다)가 이미 반영한
  최신 상태를 그 프레임 안에서 바로 쓸 수 있다.
- 매 폴링마다 `workspace_focus_target_matches_runtime(workspace_id,
  Some(runtime_instance), self.active.id, self.active.runtime_instance)`로
  "지금도 여전히 그 워크스페이스·그 runtime을 보고 있는가"를 다시 확인한다 —
  `pending_focus`가 `runtime_instance`까지 확인하는 것과 같은 이유:
  워크스페이스 id는 닫혔다 재사용될 수는 없지만(UUID), **재개장 시 새
  `runtime_instance`를 받는다**(`next_runtime_instance` 증가) — 그래서
  workspace_id만으로는 "같은 워크스페이스의 다른 런타임 세대"를 걸러낼 수
  없다.
- `self.restore_loaded_for.as_deref() == Some(&pending.workspace_id)`가 될 때까지
  기다린다. 타임아웃은 기존 `PRIMARY_PANE_MATERIALIZATION_TIMEOUT`(10초)을
  재사용했다 — 무한 대기를 만들지 않기 위한 새 상수를 따로 만들 이유가
  없었다.

### 매 폴링마다 재검증하므로 별도 취소 배선이 필요 없었다

`pending_pane_focus` 계열은 워크스페이스 전환·pane 닫기 등 여러 지점에서
명시적으로 `cancel_terminal_focus_intents()`를 호출해 취소한다. 반면
`pending_resume_agent`는 폴링할 때마다 "지금 활성 워크스페이스·runtime이
여전히 그 대상과 일치하는가"를 처음부터 다시 검사하므로, 사용자가 그 사이
다른 곳으로 옮기거나 워크스페이스가 닫혀 다른 워크스페이스로 폴백되면 다음
폴링에서 자연히 불일치를 감지하고 조용히 버린다 — 별도 취소 호출 지점을
여러 곳에 심을 필요가 없었다(더 단순한 설계).

## ③ 실패 모드에서 사용자가 보는 것

「조용한 실패 금지」 요구사항에 따라 아래 세 경로 모두 OS 알림을 낸다
(`self.notify_resume_failed(&title)` → `platform::notify(...)`,
`worktree.create_failed` 등 기존 실패 알림과 동일한 패턴 —
`self.i18n.t("sidebar.resume_failed", &[("title", title)])`, 5개 로케일 전부에
새 키 추가):

1. **활성 워크스페이스인데도 재개 실패** — 그 사이 pane이 정리되는 등으로
   `restore_agents`에 더는 없음. 즉시 알림.
2. **전환 자체가 실패** — 예: `switch_workspace` 호출 뒤에도
   `self.active.id != workspace_id`(warm 한도 초과 등으로 전환이 거부됨,
   `warm_limit_warning` 경로). 즉시 알림.
3. **전환은 됐지만 데이터 도착 전에 타임아웃**(10초) 또는 **데이터 도착 후에도
   재개할 게 없어짐**(그 사이 pane이 다른 경로로 정리됨) — 타임아웃/재확인
   시점에 알림.

패닉이나 좀비 상태(무기한 대기, 잘못된 pane에 잘못 주입)는 없다 — 매 경로가
버튼을 누른 결과를 사용자에게 보여주거나(성공: pane으로 포커스 이동), 실패
이유를 OS 알림으로 보여준다.

## ④ 기존 계약 테스트를 어떻게 고쳤나

`비활성_워크스페이스_행은_이어가기를_띄우지_않는다`(app.rs)는 `entry.resumable = false;`
상수를 소스 스캔으로 고정하고 있었다 — 지우지 않고
`비활성_워크스페이스_행의_이어가기는_전역_resumable_집합으로_판정한다`로
이름과 단언을 새 계약에 맞게 고쳤다:

- (신규) `entry.resumable = false;`가 **더는 없어야** 한다.
- (신규) `self.global_resumable_panes`를 참조**해야** 한다.
- (신규) `workspace.id.clone()`으로 스코프를 좁혀**야** 한다.
- (유지) `self.restore_agents.contains_key`를 **여전히 쓰면 안 된다** — 활성
  범위 캐시로 비활성 행을 판정하면 옛날처럼 우연히 맞는 위험한 배선이 된다는
  경고는 여전히 유효해서 그대로 남겼다.

## ⑤ 추가한 테스트

`crates/storage/src/db.rs`:

- `agent_state_global_binding_omission은_corrupt_rows를조회하거나할당하지않는다`
  — `include_global_agent_sessions=false`면 corrupt pane_id가 있어도 조회조차
  안 함(옴 계약, `agent_state_binding_omission...`과 동일 패턴).
- `agent_state_global_agent_sessions는_다른_워크스페이스의_pane도_담는다` —
  핵심 계약: 워크스페이스 스코프 `agent_sessions`는 `job.workspace_id`만,
  `global_agent_sessions`는 여러 워크스페이스를 다 담음.
  `agent_state_global_agent_sessions는_상한을_넘지_않는다` — 위 "유계" 절 참고.

`crates/app/src/app.rs`:

- `비활성_워크스페이스_행의_이어가기는_전역_resumable_집합으로_판정한다`
  (개명·재작성, 위 ④).

인스턴스 메서드(`poll_pending_resume_agent`,
`WorkspaceControllerAction::ResumeAgent` 핸들러)는 직접 테스트하지 않았다 —
`2026-08-19-resume-without-pane.md`가 기록한 것과 같은 이유: 이 파일에는
`App` 전체를 생성해 인스턴스 메서드를 직접 호출하는 테스트가 애초에 없다.
`workspace_focus_target_matches_runtime`(순수 함수, 재사용)은 기존 테스트가
이미 커버한다.

## ⑥ 게이트 결과

1. `cargo test -p deppy-sijo` — 1784 passed, 0 failed, 8 ignored(플랫폼 실측
   전용, 기존과 동일) + 통합 스위트(`agent_state_boundary` 4, `alloc_phys_
   footprint_release` 0/1 ignored, `dotenv_launch_boundary` 5,
   `lazy_bounded_worker` 14, `logging_policy` 15, `scrollback_rss_end_to_end`
   0/2 ignored) 전부 통과.
2. `cargo test -p storage` — 311 passed, 0 failed(신규 3건 포함).
3. `cargo test -p i18n` — 8 passed, 0 failed.
4. `cargo clippy --workspace --all-targets -- -D warnings` — 0 경고.
5. `cargo run -q -p xtask -- check-boundary` — OK("zero allowlist capability").
6. `cargo run -q -p xtask -- i18n-check` — OK(리터럴 키 호출 1013건을 로케일
   5개와 대조, 동적 키 62건은 기존 문서화된 예외).
7. `cargo fmt --all -- --check` — 최초 실행에서 신규 코드 3곳(app.rs 2곳,
   db.rs 1곳)에 포맷 드리프트가 있어 `cargo fmt --all`로 정리, 재실행 0건.

DB 스키마 변경은 없었다(`agent_sessions` 테이블은 그대로, 새 쿼리만 추가) —
`smoke-db-migrations`는 필요하지 않았다.

## ⑦ 화면에서 검증하지 못한 것

지시에 따라 앱을 빌드해 실행하지 않았다(사용자의 앱이 떠 있음). 코드
추적 + 단위 테스트로만 확인했고, 실제 화면으로 재현·검증하지 못한 것:

- 비활성(warm) 워크스페이스 행에서 「이어가기」 메뉴 항목이 실제로 보이는지,
  누르면 실제로 그 워크스페이스로 전환되고 잠깐 뒤 해당 pane에서 native
  resume이 실행되는지(체감 지연 포함).
- 전환 실패(warm 한도 초과)·타임아웃 경로에서 실제 macOS 알림 배너가 뜨는지,
  문구가 잘리지 않는지.
- 여러 번 빠르게 다른 워크스페이스로 옮겨 다니며 「이어가기」를 눌렀을 때
  `pending_resume_agent`가 항상 올바른 대상 하나만 유지하고 경합 없이
  동작하는지(단위 테스트로 로직은 확인했지만 실제 프레임 타이밍은 아니다).
- 5개 로케일 전부에서 `sidebar.resume_failed` 알림 문구가 실제 macOS 알림
  배너에서 잘리지 않고 자연스러운지.
