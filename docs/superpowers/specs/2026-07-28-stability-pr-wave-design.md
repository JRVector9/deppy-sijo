# Stability Parallel PR Wave Design

작성일: 2026-07-28  
상태: 사용자 승인 완료  
대상 기준: local `main`의 `74289810c603c63aacdb3873fd773efaf6456f09`

## 1. 목표

세션 유지, 장시간 실행, 메모리 보유, 원격 통신 실패와 관련된 현재의 고신뢰 findings를
서로 충돌하지 않는 PR로 나누고, 최대 다섯 개 구현 에이전트가 하나의 frozen base에서
동시에 작업할 수 있게 한다.

이번 wave는 현재 안정성 결함만 닫는다. 외부 SSH 세션을 앱 재시작이나 TCP 재연결 뒤에도
유지하는 relay-owned PTY 구현은 안정성 wave가 통합 검증을 통과한 뒤 `PR-SSH00+`로 시작한다.

## 2. 재검토 결론

### 2.1 구현 대상으로 유지

1. warm/hidden `pending_events`에서 일부 lifecycle 이벤트가 하드캡 없이 보존된다.
2. 웹푸시 `jobs`와 `retry_jobs`는 전송 지연보다 생산이 빠를 때 admission 상한이 없다.
3. archived session row가 아카이브 검증 완료 전에 소비·재결속된다.
4. `RemoteRuntimeClient` 초기 TCP connect에는 앱 수준 deadline이 없고, 수신 heartbeat가
   끊긴 상태를 판정하는 client-side deadline도 없다.
5. session log/archive GC 스캔은 바이트·깊이 제한은 있지만 디렉터리 항목 수 상한이 없다.
6. `workspace_git_label`의 process-global cache는 2초 freshness만 있고 고유 경로 수 상한이 없다.

### 2.2 구현 대상에서 제거

이전 리뷰의 “저장된 cwd가 복원되지 않는다”는 finding은 현재 코드에서 성립하지 않는다.
`PersistPipe::save_layout`은 `PaneState.cwd`를 저장하지 않지만,
`persist::load_workspace_restore_bounded`가 `mux_panes.session_id`와 `sessions.cwd`를 join해
복원용 `PaneState.cwd`를 다시 채운다.

- 조회: `crates/persist/src/repo.rs:615`
- cwd projection: `crates/persist/src/repo.rs:642`
- `PaneState.cwd` 구성: `crates/persist/src/repo.rs:669`
- runtime 적용: `crates/runtime/src/in_process.rs:2292`

따라서 cwd에는 프로덕션 수정 PR을 배정하지 않는다. `PR-SF03`이 실제 DB restore 경로를
검증하는 회귀 테스트만 추가한다. 테스트가 현재 HEAD에서 실패할 때에만 같은 PR 안에서
최소 수정한다.

## 3. 공통 비회귀 규칙

- 기존 local PTY, terminal scrollback, input queue, runtime command/event queue 상한을
  완화하지 않는다.
- raw plaintext log, secret/env/credential 값의 저장·로그·Debug 노출을 추가하지 않는다.
- UI가 PTY/process handle이나 terminal backend 구현 타입을 직접 소유하지 않는다.
- hidden pane/workspace가 새 `TerminalViewportSnapshot`을 만들지 않는 기존 정책을 유지한다.
- 자동 재시도는 bounded 횟수, 명시적 취소, generation fencing이 없는 상태로 추가하지 않는다.
- 프로덕션 경로에 무제한 채널, detached thread, periodic polling loop를 추가하지 않는다.
- public API를 바꿔 다른 PR 소유 파일까지 수정하지 않는다. 필요하면 owning PR 안에서 wrapper를
  유지한다.
- 각 PR은 자신이 실제 실행한 테스트만 build summary에 기록한다.

## 4. 기준 SHA와 브랜치 정책

`PR-SF00`은 이 문서와 `docs/CODEX_HANDOFF.md`만 포함하는 coordination commit이다.
구현 에이전트는 dirty worktree나 현재 `origin/main`에서 시작하지 않는다.

1. `PR-SF00`을 커밋한다.
2. 그 commit SHA를 `SF_BASE`로 기록한다.
3. 원격 협업 전에는 `SF_BASE`를 `origin/main` 또는 전용 base branch에 push한다.
4. `PR-SF01`~`PR-SF05`는 모두 정확히 `SF_BASE`에서 worktree/branch를 만든다.
5. 한 에이전트의 dirty worktree를 다른 에이전트가 읽거나 수정하지 않는다.
6. 구현 PR은 다른 병렬 PR을 임의 cherry-pick하지 않는다.
7. 병합 직전 base가 이동했으면 owning agent가 최신 통합 branch 위로 rebase하고 focused gate를
   다시 실행한다.

브랜치 이름은 다음으로 고정한다.

- `codex/sf01-app-retention`
- `codex/sf02-web-push-admission`
- `codex/sf03-restore-atomicity`
- `codex/sf04-remote-liveness`
- `codex/sf05-storage-scan-budget`
- `codex/sf06-integration-evidence`

## 5. 의존성 그래프

```text
PR-SF00 coordination freeze
 ├─ PR-SF01 app retention ─────────┐
 ├─ PR-SF02 web-push admission ────┤
 ├─ PR-SF03 restore atomicity ─────┤
 ├─ PR-SF04 remote liveness ───────┤─> PR-SF06 integration evidence
 └─ PR-SF05 storage scan budget ───┘

PR-SF06 pass -> PR-SSH00 relay design freeze -> PR-SSH01+ implementation
```

`PR-SF01`~`PR-SF05` 사이에는 코드 의존성이 없다. 모든 병렬 PR이 완료된 뒤에만
`PR-SF06`에서 통합한다.

## 6. PR별 소유권과 계약

### PR-SF00 — Stability Freeze Map

목적: 발견사항, 제외사항, 파일 소유권, 기준 SHA와 검증 규칙을 동결한다.

소유 파일:

- `docs/superpowers/specs/2026-07-28-stability-pr-wave-design.md`
- `docs/CODEX_HANDOFF.md`

금지:

- 프로덕션 Rust 변경
- Cargo manifest/lock 변경
- 테스트 결과를 새로 통과한 것으로 기록

완료 조건:

- cwd 오탐 제거가 문서에 반영된다.
- 다섯 병렬 PR의 소유 파일이 겹치지 않는다.
- `git diff --check`가 통과한다.

### PR-SF01 — App Retention Bounds

목적: 장시간 warm/hidden 상태에서 app-owned replay와 Git label cache가 프로세스 수명 동안
계속 증가하지 않게 한다.

소유 파일:

- `crates/app/src/app.rs`
- `docs/build/PR-SF01-summary.md`

구현 계약:

- `pending_events`의 state-like 이벤트는 현재처럼 latest-wins coalesce한다.
- 알림은 기존처럼 replay admission 전에 처리한다.
- replay retained item은 workspace당 최대 1,024개로 제한한다.
- 상한 도달 시 과거 `SpawnFailed`/spawn acknowledgement처럼 렌더 재구성에 불필요한 이벤트를
  먼저 버리고, 최신 `MuxUpdated`가 참조하는 세션에 한해 세션별 최신 status/view/exit와
  viewport 상태를 보존한다.
- 필수 상태를 안전하게 보존할 수 없으면 overflow flag를 세우고 활성화 시 fresh mux/viewport
  resync를 요청한다. silent partial replay는 금지한다.
- `workspace_git_label` cache는 최대 256개 경로만 보유한다. 기존 2초 freshness는 유지하고,
  초과분은 가장 오래 접근하지 않은 항목부터 제거한다.

필수 테스트:

- 1,024개 정확 경계와 1,025번째 admission.
- 반복 `SpawnFailed`/spawn/exit churn 뒤 retained count가 상한을 넘지 않는다.
- overflow 뒤 최신 mux와 최종 session state가 재활성 replay에서 복원된다.
- 257개 고유 Git 경로 조회 뒤 cache retained entries가 256개다.

필수 gate:

```bash
cargo test -p deppy-sijo coalesce_mux_updated --locked -- --test-threads=1
cargo test -p deppy-sijo workspace_git_label --locked -- --test-threads=1
cargo clippy -p deppy-sijo --all-targets --locked -- -D warnings
git diff --check
```

### PR-SF02 — Bounded Web Push Admission

목적: 느리거나 실패하는 push endpoint가 session notification producer보다 느릴 때도 queue가
무한히 증가하지 않게 한다.

소유 파일:

- `crates/web-remote/src/push.rs`
- `docs/build/PR-SF02-summary.md`

구현 계약:

- pending session job은 session별 latest 상태로 coalesce한다.
- `jobs`와 `retry_jobs`를 합친 retained session job 수는 최대 256개다.
- 같은 session/kind 중복은 enqueue 전에 제거한다.
- 같은 session의 `Waiting -> Done`은 `Done`이 최종 상태로 남는다.
- 재시도는 기존 총 3회 상한과 polling cadence를 유지한다.
- queue가 가득 찬 경우 block하거나 무제한 allocation하지 않는다. 가장 오래된 best-effort
  상태 알림을 제거하고 low-cardinality warning/counter를 남긴다.
- approval DB polling과 subscription 상한은 변경하지 않는다.

필수 테스트:

- stalled fake sender 상태에서 257개 session admission 후 retained job이 256개다.
- 같은 session 상태 1,000회 입력이 1개 job으로 합쳐진다.
- `Waiting -> Done`은 최종 `Done` 한 건만 발송한다.
- 전량 실패 재시도는 3회 뒤 제거되고 shutdown join을 막지 않는다.

필수 gate:

```bash
cargo test -p web-remote --locked push -- --test-threads=1
cargo clippy -p web-remote --all-targets --locked -- -D warnings
git diff --check
```

### PR-SF03 — Restore Atomicity

목적: archived session 복원이 완전히 검증된 뒤에만 persistent row binding을 소비하도록 한다.

소유 파일:

- `crates/runtime/src/persistence.rs`
- `crates/runtime/src/in_process.rs`
- `crates/persist/src/repo.rs` 테스트만
- `docs/build/PR-SF03-summary.md`

구현 계약:

- restored row의 kind/cwd 조회는 비파괴 peek다.
- archive metadata 검증, bounded stream finish, log-tail fallback session 생성 중 어느 경로도
  실패하기 전에는 `restored_rows`에서 row를 제거하지 않는다.
- 최종 archived/read-only `Session`과 pane binding이 준비된 뒤 한 번만 commit-rebind한다.
- commit 실패 시 half-bound `rows` entry나 session/pane을 남기지 않는다.
- 기존 UUID, exited status, agent kind, archived log key를 보존한다.
- cwd 프로덕션 동작은 변경하지 않는다. 실제 DB join projection과 runtime spawn cwd를 검증하는
  회귀 테스트만 추가한다.

필수 테스트:

- invalid archive metadata가 row를 소비하지 않는다.
- truncated archive가 log-tail fallback 또는 명시적 실패 뒤에도 row identity를 보존한다.
- 성공한 archive restore는 정확히 한 session id에 row를 결속한다.
- `sessions.cwd`가 저장된 DB restore에서 loaded `PaneState.cwd`와 shell spawn cwd가 일치한다.

필수 gate:

```bash
cargo test -p persist --locked load_workspace_restore -- --test-threads=1
cargo test -p runtime --locked restore -- --test-threads=1
cargo clippy -p persist -p runtime --all-targets --locked -- -D warnings
git diff --check
```

### PR-SF04 — RemoteRuntime Deadlines And Liveness

목적: 현재 exported `RemoteRuntimeClient`가 OS connect timeout이나 무기한 silent peer에
의존하지 않고 bounded 실패를 surface하게 한다. 이 PR은 SSH relay나 자동 reconnect를 만들지 않는다.

소유 파일:

- `crates/runtime/src/remote.rs`
- 새 `crates/runtime/src/remote/liveness.rs`
- `docs/build/PR-SF04-summary.md`

구현 계약:

- plain/TLS initial connect는 30초 deadline을 사용한다.
- 기존 `attach`, `attach_tls`, `attach_tls_tofu` public entry는 호환 wrapper로 유지한다.
- server heartbeat 간격 15초를 기준으로 client는 45초 동안 어떤 frame도 받지 못하면
  disconnected로 전환한다.
- heartbeat frame도 last-received 갱신에 포함한다.
- explicit shutdown과 protocol violation은 reconnect 대상으로 오인하지 않는다.
- IO thread 종료는 socket shutdown과 join으로 끝나며 detached retry thread/timer를 남기지 않는다.
- automatic reconnect/backoff/generation manager는 `PR-SSH00+` 범위로 남긴다.

필수 테스트:

- fake connector가 connect deadline을 넘으면 bounded error를 반환한다.
- silent connected peer가 45초 fake-clock 경계에서 disconnect된다.
- heartbeat가 들어오면 deadline이 연장된다.
- explicit client drop은 liveness worker와 IO worker를 모두 join한다.
- bad token/protocol violation은 기존 fail-closed 동작을 유지한다.

필수 gate:

```bash
cargo test -p runtime --locked remote -- --test-threads=1
cargo clippy -p runtime --all-targets --locked -- -D warnings
git diff --check
```

### PR-SF05 — Session Storage Scan Budgets

목적: 장기간 누적되거나 hostile하게 구성된 session log directory가 startup/GC에서 무제한
항목 탐색과 allocation을 만들지 않게 한다.

소유 파일:

- `crates/storage/src/logs.rs`
- `crates/storage/src/scrollback_archive.rs`
- `docs/build/PR-SF05-summary.md`

구현 계약:

- 한 session-log 또는 archive scan은 최대 4,096 directory entries만 검사한다.
- 재귀 session-log scan의 4,096개 상한은 디렉터리별이 아니라 한 작업 전체의 aggregate다.
- 4,097번째 entry를 만나면 partial GC 결과를 성공으로 반환하지 않고 bounded error로 끝낸다.
- symlink/non-regular file 정책과 기존 depth/byte budget을 완화하지 않는다.
- public GC 함수 signature는 유지해 `runtime/src/in_process.rs`를 수정하지 않는다.
- 삭제 후보 목록도 4,096개를 넘지 않는다.
- 이 PR은 빈 디렉터리 제거 동작을 새로 추가하지 않는다.

필수 테스트:

- 정확히 4,096개 entry scan 성공.
- 4,097개 entry에서 정해진 bounded error.
- over-limit scan이 일부 파일만 삭제한 성공 상태를 만들지 않는다.
- byte budget GC의 oldest-first 결과가 기존과 동일하다.

필수 gate:

```bash
cargo test -p storage --locked logs -- --test-threads=1
cargo test -p storage --locked scrollback_archive -- --test-threads=1
cargo clippy -p storage --all-targets --locked -- -D warnings
git diff --check
```

### PR-SF06 — Integration And Release Evidence

목적: 다섯 PR을 하나의 integration branch에 병합하고 deterministic gate와 실제 장기 실행
측정을 분리해 기록한다.

소유 파일:

- `docs/build/PR-SF06-summary.md`
- `docs/performance/final-gate.md`
- `docs/performance/release-hardware-measurements.md`

SF06은 기존 gate와 측정 절차만 사용한다. 새 `xtask`/script나 프로덕션 코드는 추가하지 않는다.

병합 순서:

1. `PR-SF03`
2. `PR-SF05`
3. `PR-SF04`
4. `PR-SF02`
5. `PR-SF01`

순서는 위험도와 shared composition root 변경을 뒤로 미루기 위한 것이며 코드 의존성은 아니다.

필수 deterministic gate:

```bash
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo run -p xtask --locked -- check-deps
cargo run -p xtask --locked -- check-boundary
cargo run -p xtask --locked -- perf-smoke
cargo run -p xtask --locked -- bg01-deterministic-gate
git diff --check
```

필수 release evidence:

- release build에서 30분 hidden/warm lifecycle churn을 실행한다.
- 1분 간격 RSS, child RSS, thread 수, open fd/socket 수, pending replay count, push job count를
  기록한다.
- queue/item count는 각 설계 상한을 한 번도 넘지 않아야 한다.
- 종료 뒤 worker/thread/socket count가 실행 전 baseline으로 돌아오는지 기록한다.
- RSS는 숫자와 추세를 기록하며 allocator page retention과 live-object 증가를 구분한다.
- 실제 측정이 실행되지 않았으면 Pass로 표시하지 않고 Pending으로 남긴다.
- 기존 Scenario A-E 절차를 재사용하고 slow consumer 및 hidden-session 결과를 함께 갱신한다.

## 7. 파일 소유권 행렬

| PR | Production files | 같은 base 병렬 가능 | 금지된 shared touch |
|---|---|---|---|
| SF01 | `crates/app/src/app.rs` | 예 | runtime/web/storage 변경 |
| SF02 | `crates/web-remote/src/push.rs` | 예 | dashboard/runtime protocol 변경 |
| SF03 | `crates/runtime/src/persistence.rs`, `crates/runtime/src/in_process.rs` | 예 | storage GC signature 변경 |
| SF04 | `crates/runtime/src/remote.rs`, `crates/runtime/src/remote/liveness.rs` | 예 | app UI, SSH relay 구현 |
| SF05 | `crates/storage/src/logs.rs`, `crates/storage/src/scrollback_archive.rs` | 예 | runtime call-site 변경 |

SF03과 SF04는 같은 crate지만 소유 파일이 다르다. SF03은 `remote.rs`를 수정하지 않고 SF04는
`in_process.rs`와 `persistence.rs`를 수정하지 않는다.

## 8. Build Summary 형식

각 구현 PR은 `docs/build/PR-SFxx-summary.md`에 다음 순서로 기록한다.

1. Input Findings
2. Scope
3. Changes
4. Tests
5. Acceptance Criteria Check
6. Regression Risks
7. Resource Impact
8. Security Impact
9. I18n/CJK Impact
10. Rollback Plan
11. Follow-up

테스트 명령과 실제 결과 수치를 함께 기록한다. 실행하지 않은 gate는 `Not run`으로 쓴다.

## 9. 리뷰와 병합 규칙

- owning agent는 focused tests와 strict package Clippy까지 책임진다.
- 각 PR은 별도 read-only reviewer가 correctness/resource/shutdown 관점으로 재검토한다.
- reviewer finding이 다른 PR 소유 파일을 요구하면 해당 PR을 직접 수정하지 않고 coordinator에게
  cross-lane dependency로 반환한다.
- coordinator만 integration branch에서 병합 충돌을 해결한다.
- integration 중 behavior 변경이 필요하면 원래 owning PR로 되돌려 수정하고 재검증한다.
- `PR-SF06` 전에는 full release soak을 개별 agent가 중복 실행하지 않는다.

## 10. 롤백 경계

- SF01 rollback: app replay/cache 정책만 이전 구현으로 복귀한다.
- SF02 rollback: push notification admission만 이전 구현으로 복귀한다.
- SF03 rollback: archived restore rebind 순서만 복귀하며 DB schema rollback은 없다.
- SF04 rollback: client deadline/liveness만 복귀하며 wire protocol version은 바꾸지 않는다.
- SF05 rollback: scan entry cap만 복귀하며 로그 파일 포맷은 바꾸지 않는다.

모든 PR은 migration, wire-format, persistent schema 변경 없이 독립 revert 가능해야 한다.

## 11. 후속 SSH Wave 경계

`PR-SF06`가 deterministic gate를 통과한 뒤에만 `PR-SSH00`을 시작한다.

`PR-SSH00`은 design-only이며 다음을 고정한다.

- remote relay-owned PTY
- host-scoped PTY id와 incarnation id
- detach와 dispose의 분리
- bounded replay와 attach identity validation
- sleep/wake liveness probe와 generation-fenced reconnect
- SSH user 권한이 shell-equivalent authority라는 보안 경계

`PR-SSH00` 전에는 remote relay binary, SSH install/deploy UX, lease persistence, remote
`PtyBackend` 구현을 추가하지 않는다.

## 12. 성공 조건

- 다섯 구현 PR이 동일 `SF_BASE`에서 독립적으로 시작 가능하다.
- production file ownership이 겹치지 않는다.
- 이전 cwd 오탐 때문에 불필요한 프로덕션 변경을 만들지 않는다.
- 두 실제 unbounded queue와 두 process-lifetime/filesystem retention 경로가 명시적 상한을 갖는다.
- archived restore는 검증 성공 전 persistent row를 소비하지 않는다.
- RemoteRuntime attach와 silent-peer failure가 bounded 시간 안에 surface된다.
- 통합 결과가 장기 실행 측정 전까지 완료로 과장되지 않는다.
- SSH continuity는 안정성 유지보수와 분리된 후속 architecture wave로 남는다.
