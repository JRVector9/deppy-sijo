# Rust AI Agent Workspace — Remaining Parallel Implementation PR Plan v3.3

작성일: 2026-07-05  
대상: v2.5/v2.6/v2.8/v3.2 기반 구현 완료 코드베이스  
문서 성격: **남은 Follow-up PR을 병렬 구현하기 위한 최종 구현 계획서**  
입력 문서:
- `update-findings-summary.md`
- `remaining-follow-up-code-triage.md`
- `ai_agent_workspace_v3_2_update_only_final_pr_plan.md`
- `ai_agent_workspace_final_architecture_v2_6_FOLDER_TREE.md`
- `ai_agent_workspace_v2_8_persistence_store_improvement_FINAL.md`

---

## 0. 현재 상태 요약

리뷰와 대부분의 Build Track은 완료되었다.  
새 작업은 이미 완료된 PR을 건드리지 않고, 아래 남은 follow-up만 진행한다.

완료된 중요 항목:

```text
- PR-U05b, PR-U06b, PR-U08b, PR-U09b, PR-U10b, PR-U20b 구현/커밋 완료
- PR-U11 security-scan gate 통과
- PR-U19 remote slow consumer backpressure 완료
- PR-U21~U24 i18n / CJK gate 완료
- PR-U25 Global Activity View 완료
- PR-U26 Terminal Dirty-Range Partial Render 완료
```

남은 항목:

```text
PR-U12c:
  Child Process Tree CPU/RSS Aggregation

PR-U18b:
  Runtime/App Hot-path DB Write Batching Wiring

PR-U15c:
  PTY Input Queue Policy / Visible Backpressure Badge

PR-U17b:
  Status Detector Confidence / User Override

PR-U20c:
  Release-hardware Scenario A-E measurement execution
```

중요 원칙:

```text
- 이미 완료된 PR에 남은 작업을 섞지 않는다.
- 각 follow-up은 독립 PR로 진행한다.
- 공유 파일 충돌을 줄이기 위해 파일 ownership을 명시한다.
- PR-U20c baseline 측정은 즉시 가능하지만, final release 측정은 U12c/U18b/U15c/U17b 이후 다시 실행한다.
```

---

## 1. Non-Regression Baseline

모든 남은 PR은 아래 기능을 깨면 안 된다.

```text
- Pane-level workspace/session operation
- Folder tree rendering for valid workspace roots
- Folder tree/sidebar path insertion into terminal
- Drag/drop path insert must not auto-execute
- Terminal selection/copy/paste
- Bracketed paste
- Required CJK/emoji path fixtures
- UI terminal actions through RuntimeClient boundary
- Hidden panes/workspaces must not create TerminalViewportSnapshot
- Raw plaintext logs disabled by default
- Secret/env/API key not persisted in DB/config/log/export plain text
- mcp/audit/persist/storage crate cycles absent
```

---

## 2. 병렬 구현 전략

## 2.1 병렬화 판단

완전 병렬 가능한 작업:

```text
Lane A:
  PR-U18b Runtime/App Hot-path DB Write Batching Wiring

Lane B:
  PR-U17b Status Detector Confidence / User Override

Lane C:
  PR-U20c Release Measurement Baseline
```

부분 병렬 가능한 작업:

```text
Lane D:
  PR-U12c Child Process Tree CPU/RSS Aggregation

Lane E:
  PR-U15c PTY Input Queue Policy / Visible Backpressure Badge
```

주의:

```text
PR-U12c와 PR-U15c는 둘 다 pty/runtime/activity 경계를 건드릴 수 있다.
따라서 병렬 진행 전 PR-PAR-00 Parallel Contract Freeze를 먼저 merge한다.
```

## 2.2 필수 선행 PR

### PR-PAR-00 — Parallel Contract Freeze

목표:

```text
남은 PR들이 같은 파일을 무질서하게 수정하지 않도록 공통 타입/이벤트/파일 ownership을 확정한다.
```

범위:

```text
- 새 기능 구현 금지
- 타입/이벤트 자리만 확정
- 문서/테스트 scaffold만 추가
- 기존 동작 변경 금지
```

작업:

```text
1. docs/update/remaining-parallel-pr-map.md 생성
2. PR별 file ownership 표 추가
3. RuntimeEvent 확장 지점 문서화
4. ResourceUsage / InputPressure / StatusWithConfidence 이벤트 naming 확정
5. pty 모듈 분리 계획 문서화:
   - process_identity.rs
   - input_queue.rs
6. cargo check --workspace --all-targets 통과
```

완료 기준:

```text
- 각 PR의 수정 가능 파일이 명확함
- shared enum/type naming 충돌 없음
- PR-U12c / PR-U15c / PR-U17b가 서로 다른 모듈 중심으로 작업 가능
```

---

## 3. 병렬 Lane 구성

## Lane A — PR-U18b Runtime/App Hot-path DB Write Batching Wiring

병렬 가능성:

```text
독립 실행 가능.
PR-U12c/U15c/U17b와 직접 충돌 낮음.
```

소유 파일:

```text
crates/storage/src/write_worker.rs
crates/runtime/src/persistence.rs
crates/runtime/src/in_process.rs
crates/persist/src/repo.rs
docs/build/PR-U18b-summary.md
```

금지:

```text
- layout save를 무조건 async로 바꾸지 않는다.
- read-after-write semantics가 필요한 경로를 batch로 바꾸지 않는다.
- UI thread에서 DB write를 새로 추가하지 않는다.
```

목표:

```text
Runtime persistence hot path의 직접 rusqlite write를 DbWriteWorker/DbWriteHandle로 우회한다.
```

작업:

```text
1. runtime persistence config에 optional DbWriteHandle 추가
2. PersistPipe에서 burst write 경로 식별
3. status update / log offset update / notification burst를 DbWriteHandle로 route
4. layout save는 synchronous 유지
   - 단, 별도 coalescing contract가 명확해진 경우에만 변경
5. shutdown flush 정책 추가
6. read-after-write가 필요한 경로는 direct/sync 유지
7. rollback behavior 문서화
```

Acceptance Criteria:

```text
- DbWriteWorker foundation을 실제 runtime hot path에 연결
- status/log-offset burst path가 direct DB write하지 않음
- layout save synchronous 유지 또는 별도 contract 명시
- pending write shutdown flush 보장
- high-output smoke에서 SQLite write burst 감소
```

검증:

```bash
cargo check --workspace --all-targets
cargo test --workspace --no-run
cargo test -p storage write_worker
cargo run -p xtask -- smoke-db-migrations
```

산출물:

```text
docs/build/PR-U18b-runtime-db-write-batching-summary.md
```

---

## Lane B — PR-U17b Status Detector Confidence / User Override

병렬 가능성:

```text
독립 실행 가능.
단, RuntimeEvent 변경 시 PR-PAR-00의 naming을 따른다.
```

소유 파일:

```text
crates/session/src/status.rs
crates/session/src/status_detector.rs
crates/runtime/src/in_process.rs
crates/app/src/ui/workspace.rs
crates/app/src/ui/notifications.rs
crates/i18n/
locales/*/
docs/build/PR-U17b-summary.md
```

주의:

```text
SessionStatus는 현재 단순 enum으로 runtime, workspace UI, notifications,
remote events, i18n message IDs가 소비한다.
따라서 기존 enum을 무리하게 대체하지 말고, 확장 모델을 추가한다.
```

권장 모델:

```rust
pub struct SessionStatusView {
    pub status: SessionStatus,
    pub confidence: Option<StatusConfidence>,
    pub source: StatusSource,
    pub override_state: Option<UserStatusOverride>,
}

pub enum StatusSource {
    ProcessExit,
    StreamRegex,
    ScreenText,
    IdleHeuristic,
    UserOverride,
}

pub struct StatusConfidence {
    pub score: f32,
    pub reason: String,
}

pub enum UserStatusOverride {
    MarkRunning,
    MarkWaiting,
    MarkDone,
    MarkError,
    ClearOverride,
}
```

작업:

```text
1. 기존 SessionStatus enum 유지
2. SessionStatusView 또는 유사한 확장 모델 추가
3. confidence score/source 추가
4. user override command 추가
5. notification semantics 정의:
   - user override가 notification을 재발송하는지
   - MaybeWaiting 상태가 알림을 발생시키는지
6. UI controls 추가:
   - Mark as waiting
   - Mark as done
   - Clear override
   - Ignore pattern
7. i18n keys 추가
8. pseudo-locale layout coverage 추가
```

Acceptance Criteria:

```text
- 기존 SessionStatus 소비 경로가 깨지지 않음
- confidence/source가 status view에 표시 또는 내부 기록됨
- user override가 workspace UI / notification / global activity view에 반영됨
- override controls의 i18n key 존재
- pseudo-locale layout check 통과
```

검증:

```bash
cargo check --workspace --all-targets
cargo test --workspace --no-run
cargo run -p xtask -- i18n-check
```

산출물:

```text
docs/build/PR-U17b-status-confidence-user-override-summary.md
```

---

## Lane C — PR-U20c Release-hardware Scenario A-E Measurements

병렬 가능성:

```text
즉시 baseline 측정 가능.
final release approval 측정은 U12c/U18b/U15c/U17b 이후 재실행.
```

소유 파일:

```text
docs/performance/final-gate.md
docs/performance/release-hardware-measurements.md
xtask/src/perf.rs, 필요 시 측정 스크립트만
```

목표:

```text
실제 release hardware에서 GUI/remote soak 측정을 수행한다.
코드 변경이 아니라 측정/리포트 PR이다.
```

Scenario:

```text
A. 빈 앱
B. workspace 5개 / pane 20개 / session 10개
C. hidden session 10개 / 3개 대량 output
D. folder tree 100k files
E. remote slow consumer
```

작업:

```text
1. release hardware 명시
2. OS / CPU / RAM / disk / display scale 기록
3. GUI 실제 실행 측정
4. remote slow consumer soak 측정
5. idle CPU / RSS / active pane frame time / queue growth 기록
6. baseline과 final 결과 분리
```

Acceptance Criteria:

```text
- docs/performance/final-gate.md 업데이트
- release hardware measurement table 추가
- pass/fail/pending 상태 명확화
- PR-U20c baseline은 즉시 가능
- final approval은 U12c/U18b/U15c/U17b 이후 재측정
```

검증:

```bash
cargo run -p xtask -- perf-smoke
```

산출물:

```text
docs/performance/release-hardware-measurements.md
```

---

## Lane D — PR-U12c Child Process Tree CPU/RSS Aggregation

병렬 가능성:

```text
PR-PAR-00 이후 병렬 가능.
단, PR-U15c와 pty/in_process/activity 파일 충돌 주의.
```

소유 파일:

```text
crates/pty/src/process_identity.rs
crates/pty/src/lib.rs, export only
crates/session/src/session.rs
crates/runtime/src/resource_monitor.rs
crates/runtime/src/in_process.rs, resource event wiring only
crates/app/src/ui/activity.rs, resource tree display only
docs/build/PR-U12c-summary.md
```

목표:

```text
앱 프로세스 단위 resource monitor를 세션별 child process tree CPU/RSS 집계로 확장한다.
```

문제:

```text
현재 PtySession은 child pid/process group metadata를 안정적으로 노출하지 않는다.
portable_pty::MasterPty::process_group_leader()는 내부 Drop에서만 사용되고,
runtime/session boundary에는 child process tree sampling용 API가 없다.
```

권장 타입:

```rust
#[derive(Clone, Debug)]
pub struct ProcessIdentity {
    pub pid: Option<u32>,
    pub process_group: Option<u32>,
    pub source: ProcessIdentitySource,
}

#[derive(Clone, Debug)]
pub enum ProcessIdentitySource {
    PortablePty,
    PlatformFallback,
    Unavailable,
}
```

작업:

```text
1. pty boundary에 redacted ProcessIdentity API 추가
2. PtySessionHandle 또는 spawn result에 ProcessIdentity 포함
3. Session metadata로 ProcessIdentity 전파
4. runtime resource monitor가 session → process tree 매핑
5. platform별 child process tree aggregation
6. app UI activity view에 session child tree 표시
7. fake process identity test 추가
8. platform-gated real process tree smoke 추가
```

금지:

```text
- UI가 직접 sysinfo/process tree를 ownership하지 않는다.
- UI가 pty handle에서 pid를 직접 읽지 않는다.
- process command/env 전체를 노출하지 않는다.
```

Acceptance Criteria:

```text
- session별 child process tree CPU/RSS 표시
- workspace별 child tree RSS aggregation
- app process RSS와 child RSS 구분
- ProcessIdentity unavailable일 때 graceful fallback
- high resource session UI 표시
```

검증:

```bash
cargo check --workspace --all-targets
cargo test --workspace --no-run
cargo test -p runtime resource_monitor
cargo run -p xtask -- perf-smoke
```

산출물:

```text
docs/build/PR-U12c-child-process-tree-resource-summary.md
```

---

## Lane E — PR-U15c PTY Input Queue Policy / Visible Backpressure Badge

병렬 가능성:

```text
PR-PAR-00 이후 병렬 가능.
단, deadlock-safe design PR로 시작한다.
Implementation은 design approval 후 진행한다.
```

소유 파일:

```text
crates/pty/src/input_queue.rs
crates/pty/src/lib.rs, export only
crates/runtime/src/in_process.rs, pressure event wiring only
crates/app/src/ui/workspace.rs
crates/app/src/ui/activity.rs
docs/build/PR-U15c-summary.md
```

문제:

```text
PTY output은 sync_channel(64)로 bounded.
PTY input은 현재 unbounded channel을 의도적으로 사용해 full-duplex deadlock을 피한다.
단순히 cap을 추가하면 paste/input reliability가 퇴행할 수 있다.
```

목표:

```text
deadlock 없이 PTY input queue policy를 정의하고,
사용자가 pressure 상태를 볼 수 있게 한다.
```

필수 설계 원칙:

```text
- blocking write_all 단일 IO loop 금지
- input queue cap은 byte budget 기준
- overflow는 명시적 error/result로 반환
- bracketed paste / terminal DnD paste / CJK input 회귀 금지
- visible badge는 RuntimeEvent 또는 activity view로 노출
```

권장 모델:

```rust
pub struct PtyInputQueuePolicy {
    pub max_bytes: usize,
    pub max_messages: usize,
    pub large_paste_threshold: usize,
}

pub enum PtyInputEnqueueResult {
    Accepted,
    Backpressured { queued_bytes: usize, max_bytes: usize },
    Rejected { reason: PtyInputRejectReason },
}

pub enum PtyInputRejectReason {
    QueueFull,
    SessionClosed,
    WriterUnavailable,
    PayloadTooLarge,
}
```

작업:

```text
1. byte-budgeted input queue 설계
2. explicit overflow result 추가
3. large paste chunking policy 정의
4. bracketed paste preserve
5. terminal DnD paste preserve
6. CJK input fixture preserve
7. runtime pressure event 추가
8. workspace/activity UI badge 표시
9. write failure propagation
```

Acceptance Criteria:

```text
- unbounded input queue 제거 또는 bounded policy 명시
- input queue full 시 조용한 유실 없음
- send_command/enqueue success와 actual write failure 구분
- visible backpressure badge 표시
- large paste가 UI freeze를 유발하지 않음
- bracketed paste / DnD paste / CJK input tests 통과
```

검증:

```bash
cargo check --workspace --all-targets
cargo test --workspace --no-run
cargo test -p pty input_queue
cargo test -p terminal bracketed_paste
cargo run -p xtask -- perf-smoke
```

산출물:

```text
docs/build/PR-U15c-pty-input-backpressure-summary.md
```

---

## 4. Merge / Parallel Rules

## 4.1 Merge order

권장 merge 순서:

```text
1. PR-PAR-00 Parallel Contract Freeze
2. PR-U20c Baseline Measurement, can merge anytime
3. PR-U18b DB Write Batching, independent
4. PR-U12c Child Process Tree
5. PR-U15c PTY Input Queue
6. PR-U17b Status Confidence / Override
7. PR-U20c Final Release Measurement
```

단, PR-U12c / PR-U15c / PR-U17b는 review 결과에 따라 순서 교환 가능.

## 4.2 Conflict prevention

```text
- PR-U12c는 ProcessIdentity와 ResourceUsage만 소유한다.
- PR-U15c는 PtyInputQueuePolicy와 InputPressure만 소유한다.
- PR-U17b는 SessionStatusView와 StatusOverride만 소유한다.
- RuntimeEvent에 새 variant를 추가할 경우 PR-PAR-00 naming을 따른다.
- app/src/ui/activity.rs를 동시에 수정할 경우 section 단위로 충돌을 최소화한다.
```

## 4.3 Shared tests

모든 PR은 아래 baseline을 깨면 안 된다.

```bash
cargo check --workspace --all-targets
cargo test --workspace --no-run
cargo run -p xtask -- security-scan
cargo run -p xtask -- i18n-check
```

성능 관련 PR은 추가:

```bash
cargo run -p xtask -- perf-smoke
```

---

## 5. Orchestrator Task Prompts

## 5.1 공통 Build Agent Prompt

```text
너는 Rust AI Agent Workspace의 남은 follow-up PR 구현 에이전트다.

이번 작업은 이미 완료된 v2.5/v2.6/v2.8/v3.2 기반 구현을 보완하는 것이다.
새 아키텍처로 갈아엎지 말고, 지정된 PR 범위 안에서만 구현하라.

반드시 먼저 아래 문서를 읽어라.

1. update-findings-summary.md
2. remaining-follow-up-code-triage.md
3. ai_agent_workspace_v3_2_update_only_final_pr_plan.md
4. ai_agent_workspace_final_architecture_v2_6_FOLDER_TREE.md
5. ai_agent_workspace_v2_8_persistence_store_improvement_FINAL.md

공통 불변 원칙:
1. UI는 RuntimeClient만 본다.
2. Runtime은 mux / session / env / terminal / pty를 조율한다.
3. Active pane만 render한다.
4. Hidden workspace/session은 log/status만 처리한다.
5. TerminalViewportSnapshot은 visible pane에만 만든다.
6. Raw log 평문은 기본 저장하지 않는다.
7. Session은 secret store를 직접 모른다.
8. folder tree / DnD / terminal copy-paste UX는 회귀 금지다.
9. mcp/audit/persist/storage 계층에서 crate 순환 의존을 만들지 않는다.
10. secret/env/API key는 DB/config/log/export에 평문 저장하지 않는다.

금지:
- 이미 완료된 PR의 범위를 섞지 마라.
- unrelated refactor 금지.
- large architecture rewrite 금지.
- raw plaintext log 활성화 금지.
- secret/debug leak 금지.
- blocking write_all 단일 IO loop 금지.

산출물:
- 코드 변경
- 테스트
- docs/build/<PR-ID>-summary.md
- risk notes
- rollback plan
- follow-up list
```

## 5.2 PR-U12c Agent Prompt

```text
PR-ID:
  PR-U12c

Title:
  Child Process Tree CPU/RSS Aggregation

Goal:
  Add per-session child process tree CPU/RSS aggregation.

Scope:
  - Add ProcessIdentity API at pty boundary.
  - Propagate process identity into session/runtime metadata.
  - Aggregate child process tree CPU/RSS in runtime.
  - Show per-session and workspace child resource usage in activity/resource UI.
  - Add fake identity tests and platform-gated real process smoke.

Own files:
  crates/pty/src/process_identity.rs
  crates/pty/src/lib.rs, export only
  crates/session/src/session.rs
  crates/runtime/src/resource_monitor.rs
  crates/runtime/src/in_process.rs, resource wiring only
  crates/app/src/ui/activity.rs, resource display only

Do not:
  - Modify PTY input queue policy.
  - Implement backpressure badge.
  - Move resource aggregation into UI.
  - Expose full command/env in UI.

Acceptance:
  - session child tree RSS/CPU visible
  - workspace aggregate RSS includes child tree
  - app RSS and child RSS separated
  - missing pid/process_group gracefully handled
```

## 5.3 PR-U18b Agent Prompt

```text
PR-ID:
  PR-U18b

Title:
  Runtime/App Hot-path DB Write Batching Wiring

Goal:
  Wire DbWriteWorker/DbWriteHandle into runtime/app persistence hot paths.

Scope:
  - Add optional DbWriteHandle to runtime persistence config.
  - Route status/log-offset burst paths through handle.
  - Keep layout save synchronous unless coalescing contract is explicit.
  - Define flush/read-after-write/rollback behavior.

Own files:
  crates/storage/src/write_worker.rs
  crates/runtime/src/persistence.rs
  crates/runtime/src/in_process.rs
  crates/persist/src/repo.rs

Do not:
  - Convert all layout saves to async by default.
  - Change DB schema.
  - Add UI thread DB writes.

Acceptance:
  - runtime hot path no longer direct-writes status/log-offset bursts
  - graceful shutdown flush
  - high output scenario reduces DB write churn
```

## 5.4 PR-U15c Agent Prompt

```text
PR-ID:
  PR-U15c

Title:
  PTY Input Queue Policy / Visible Backpressure Badge

Goal:
  Add deadlock-safe PTY input queue policy and visible input pressure signal.

Scope:
  - Design byte-budgeted input queue.
  - Explicit overflow result.
  - Preserve bracketed paste, terminal DnD paste, and CJK input.
  - Surface pressure through runtime events or Activity View.
  - Propagate write failure instead of silent loss.

Own files:
  crates/pty/src/input_queue.rs
  crates/pty/src/lib.rs, export only
  crates/runtime/src/in_process.rs, pressure wiring only
  crates/app/src/ui/workspace.rs
  crates/app/src/ui/activity.rs

Do not:
  - Use blocking write_all in a single IO loop.
  - Add a cap that can deadlock full-duplex IO.
  - Drop input silently.
  - Break paste/DnD/CJK tests.

Acceptance:
  - bounded/byte-budgeted policy exists
  - queue full returns visible error/pressure
  - no silent command loss
  - pressure badge appears
  - paste reliability preserved
```

## 5.5 PR-U17b Agent Prompt

```text
PR-ID:
  PR-U17b

Title:
  Status Detector Confidence / User Override

Goal:
  Extend status model with confidence/source and user override.

Scope:
  - Keep existing SessionStatus compatibility.
  - Add SessionStatusView or equivalent.
  - Add confidence/source.
  - Add user override controls and runtime commands.
  - Define notification semantics.
  - Add i18n keys and pseudo-locale coverage.

Own files:
  crates/session/src/status.rs
  crates/session/src/status_detector.rs
  crates/runtime/src/in_process.rs
  crates/app/src/ui/workspace.rs
  crates/app/src/ui/notifications.rs
  crates/i18n/
  locales/*/

Do not:
  - Break existing SessionStatus consumers.
  - Change notification behavior without defining semantics.
  - Add UI controls without i18n keys.
  - Remove detector cost controls.

Acceptance:
  - confidence/source available
  - user can override status
  - override reflected in UI/notification/global activity
  - i18n-check passes
```

## 5.6 PR-U20c Agent Prompt

```text
PR-ID:
  PR-U20c

Title:
  Release-hardware Scenario A-E Measurements

Goal:
  Run and document release-hardware GUI/remote soak measurements.

Scope:
  - Baseline measurement can run immediately.
  - Final measurement must run after PR-U12c/U18b/U15c/U17b.
  - Update docs/performance/final-gate.md and release-hardware report.

Scenarios:
  A. empty app
  B. workspace 5 / pane 20 / session 10
  C. hidden session 10 / 3 heavy output
  D. folder tree 100k files
  E. remote slow consumer

Own files:
  docs/performance/final-gate.md
  docs/performance/release-hardware-measurements.md
  xtask/src/perf.rs only if measurement script needs small additions

Do not:
  - Hide pending measurements as passed.
  - Change runtime behavior.
  - Treat perf-smoke pass as release approval.

Acceptance:
  - release hardware specified
  - baseline and final separated
  - pass/fail/pending explicit
  - GUI/remote soak recorded
```

---

## 6. Final Gate

After all implementation PRs merge:

```text
1. Run PR-U20c final release-hardware Scenario A-E.
2. Run final gates:
   - PR-U11 security-scan
   - PR-U20 perf-smoke + release hardware report
   - PR-U24 i18n-check
3. Confirm:
   - no crate cycle
   - no raw plaintext log default
   - no hidden pane snapshot
   - no unbounded queue growth
   - required locale completeness
   - release-hardware measurements pass or explicitly documented as pending
```

---

## 7. Final Orchestrator Prompt

```text
너는 Rust AI Agent Workspace의 남은 follow-up PR 병렬 구현 오케스트레이터다.

입력 문서:
1. update-findings-summary.md
2. remaining-follow-up-code-triage.md
3. ai_agent_workspace_v3_2_update_only_final_pr_plan.md
4. ai_agent_workspace_final_architecture_v2_6_FOLDER_TREE.md
5. ai_agent_workspace_v2_8_persistence_store_improvement_FINAL.md

목표:
- 이미 완료된 PR은 건드리지 않는다.
- 남은 follow-up만 병렬로 구현한다.
- PR-U12c, PR-U18b, PR-U15c, PR-U17b, PR-U20c를 독립 작업으로 배정한다.
- PR-U12c/PR-U15c/PR-U17b가 RuntimeEvent/UI/activity 파일에서 충돌하지 않도록 PR-PAR-00으로 공통 계약을 먼저 고정한다.
- PR-U20c는 baseline 측정을 즉시 수행하고, final 측정은 구현 PR merge 후 다시 수행한다.

실행 순서:
1. PR-PAR-00 Parallel Contract Freeze를 먼저 배정한다.
2. PR-U20c baseline measurement를 바로 배정한다.
3. PR-U18b를 독립 lane으로 배정한다.
4. PR-U12c를 resource lane으로 배정한다.
5. PR-U15c를 pty input/backpressure lane으로 배정한다.
6. PR-U17b를 status UX lane으로 배정한다.
7. 각 PR이 끝나면 해당 Build Summary를 확인한다.
8. 충돌 파일이 있으면 PR-PAR-00의 ownership 규칙을 기준으로 조정한다.
9. 모든 구현 PR 후 PR-U20c final measurement를 실행한다.
10. 최종 gate를 PR-U11, PR-U20, PR-U24 순서로 실행한다.

공통 불변 원칙:
1. UI는 RuntimeClient만 본다.
2. Runtime은 mux/session/env/terminal/pty를 조율한다.
3. Active pane만 render한다.
4. Hidden workspace/session은 log/status만 처리한다.
5. TerminalViewportSnapshot은 visible pane에만 만든다.
6. Raw log 평문은 기본 저장하지 않는다.
7. Session은 secret store를 직접 모른다.
8. folder tree/DnD/terminal copy-paste UX는 회귀 금지다.
9. mcp/audit/persist/storage crate cycle 금지.
10. secret/env/API key는 DB/config/log/export에 평문 저장 금지.

각 에이전트에게는 다음을 반드시 전달한다:
- PR-ID
- Title
- Goal
- Scope
- Own files
- Do not list
- Acceptance criteria
- Required commands
- docs/build/<PR-ID>-summary.md 산출물 경로

병렬 Merge 정책:
- PR-PAR-00은 먼저 merge한다.
- PR-U18b는 독립 merge 가능.
- PR-U20c baseline은 언제든 merge 가능.
- PR-U12c와 PR-U15c는 pty/runtime/activity 충돌 여부를 확인하고 순차 merge할 수 있다.
- PR-U17b는 RuntimeEvent/notification/i18n 충돌을 확인한 뒤 merge한다.
- PR-U20c final은 마지막 gate로 실행한다.
```
