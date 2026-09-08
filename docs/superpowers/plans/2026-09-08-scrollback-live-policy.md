# Scrollback Live Policy Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** 저장된 scrollback 설정을 현재·새·복원 세션에 안전하게 적용하고 실제 완료/미지원/재시도 상태를 표시한다.

**Architecture:** PR #157의 terminal::policy 범위를 그대로 사용한다. backend는 요청·실효 한도·삭제 수를 반환하고 Session이 dirty 상태를 함께 갱신한다. runtime은 append-only 명령/결과로 worker 적용을 확인하며 app은 runtime lifetime과 generation을 구분한 최신값 한 슬롯으로 유계 재시도한다. 숨김 전환은 cold 압축만 수행하고 삭제는 명시적 축소·전역 예산 압박 경로에 맡긴다.

**Tech Stack:** Rust 1.96.1, Alacritty vendored backend, serde/postcard protocol, egui 0.36.

**안전 의존성 교정:** 최종 독립 리뷰 후 #160의 bounded streaming reflow가 100k 설정의 필수 의존임을 확인했다. root 승인으로 #160 exact head `18a06f1`을 일반 merge `75b8dcf`로 포함한다. 아래의 과거 “PR C 비포함” 실행 기록보다 이 최종 결정이 우선한다.

---

### Task 1: backend live setter와 숨김 보존
**Files:** Modify `crates/terminal/src/{backend,alacritty_backend,ghostty_backend,lib}.rs`; Test alacritty_backend tests.
- [x] 충분한 기록을 만든 뒤 hidden으로 전환해 줄 수가 감소하지 않는 RED를 확인한다.
- [x] `ScrollbackApplyResult::{Applied { requested, effective, trimmed }, Unsupported}`와 `TerminalBackend::set_scrollback_limit` 기본 Unsupported를 추가한다.
- [x] Alacritty 요청값을 갱신하고 기존 class/압박 한도를 재계산한다. 낮출 때 즉시 oldest를 제거하고 올릴 때 보관 용량만 늘린다.
- [x] Hidden budget은 Visible과 보관 한도를 맞추고 primary/inactive grid cold 압축을 유지한다. Exited도 archive 쓰기 전 고정1000줄로 삭제하지 않도록 사용자 상한을 따른다. 실제 압축 footprint와 전역 압박의 hidden-first 순서는 유지한다.
- [x] `CARGO_BUILD_JOBS=2 cargo test -p terminal --locked`로 shrink/grow/no resurrection/hidden/alt-screen/pressure 회귀를 확인한다.

### Task 2: Session 위임과 snapshot 무효화
**Files:** Modify `crates/session/src/session.rs`; Test session tests.
- [x] backend 결과를 반환하는 Session setter를 만들고 Applied 결과에서 dirty/snapshot 갱신이 발생하는 회귀 테스트를 먼저 작성한다.
- [x] `CARGO_BUILD_JOBS=2 cargo test -p session --locked`로 기존 PTY/선택/스크롤 경로를 검증한다. PTY 재생성은 하지 않는다.

### Task 3: append-only wire 계약
**Files:** Modify `crates/runtime/src/{command,event,protocol,remote}.rs`.
- [x] `SetScrollbackLimit { generation: u64, requested: u32 }`를 명령 끝에 추가하고 `ScrollbackLimitApplied` 결과를 이벤트 끝에 추가한다. 결과는 generation/requested/applied/unsupported/trimmed/effective_min/durable/restored의 고정 크기 집계다.
- [x] generation=0과 설정 범위 밖을 admission에서 거부한다. 기존 spawn wire0..100000과 SetTerminalCachePolicy 필드는 유지한다.
- [x] protocol v12→v13으로 올리고 구버전은 handshake에서 거부한다. enum 순서·legacy postcard bytes·새 메시지 round-trip 테스트를 추가한다.
- [x] `CARGO_BUILD_JOBS=2 cargo test -p runtime command::tests --locked`, event/protocol focused와 remote wire 검증을 실행한다.

### Task 4: worker 현재·새·복원 세션 반영
**Files:** Modify `crates/runtime/src/in_process.rs`.
- [x] worker는 최신 요청 한 개와 현재 세션 수로 제한된 적용 결과를 보관한다. 실제 setter 호출 뒤 ACK를 보낸다. 같은 요청 재전송은 결과를 다시 보내고 불필요한 압축을 반복하지 않는다.
- [x] 모든 spawn/split/respawn/restore/inflate 경로가 최신 한도로 backend를 만든 뒤 feed하도록 연결한다. 오래된 archive의 작은 실효 한도를 증가가 자동으로 되돌리지 않도록 복원 한도를 유지한다.
- [x] 종료/삭제 시 결과 state를 정리하고 새 세션 편입 뒤 집계 결과를 갱신한다. viewport는 추가 PTY 출력 없이 다시 전송한다.
- [x] 현재·미래·아카이브 복원, 중복 요청, hidden-first trim focused RED/GREEN을 실행한다. Ghostty는0.2binding 소스에서setter 부재와trait 기본Unsupported를확인하며네이티브실행은대기로남긴다.

### Task 5: app latest-slot delivery와 설정 상태
**Files:** Create `crates/app/src/scrollback_policy.rs`; Modify `crates/app/src/{main,app}.rs`, `crates/app/src/ui/settings.rs`.
- [x] 순수 delivery 상태 테스트로 runtime_instance/generation stale ACK 거부, 연속 편집 최신값 유지, queue-full 지연 재시도, 유계 횟수 종료와 명시적 재시도를 먼저 검증한다.
- [x] 각 WorkspaceRuntime과 별도 TLS worker가 delivery 한 개를 소유하고 active/warm/TLS 모두 같은 pump/ACK 경로를 사용한다. 새 runtime은 최신 설정을 받으며 종료 runtime의 state는 함께 해제된다. TLS 시작/종료 시 전역 캐시 예산 분모와 방송도 갱신한다.
- [x] 기존 설정 적용과 resident 변경 broadcast에서 요청값을 갱신하고 logic tick에서만 제한적으로 전송한다. 렌더에서 runtime 작업을 수행하지 않는다.
- [x] Settings에는 순수 DTO로 적용 중/완료/미지원/실패와 재시도 버튼을 표시한다. 실제 ACK 전 완료라고 표시하지 않는다.
- [x] app delivery/settings focused tests를 실행한다.

### Task 6: locale·리뷰·게이트
**Files:** Modify five `crates/i18n/locales/*/messages.txt`, `docs/CODEX_HANDOFF.md`.
- [x] 5개 locale에 기존 세션 적용, 축소 삭제·증가 용량 확대, hidden 압축·압박 시 정리, 미지원·실패 안내를 반영한다.
- [x] 읽기 전용 codex review의 확정 P2 세 건(압박 floor 상속, TLS worker 누락, 복원 중 예산 지연)을 수정한다. backend 이동 시 floor 초기화, 기존 dispatcher/ACK로 TLS 연결, 세션 편입마다 hidden-first 예산 적용을 검증한다.
- [x] terminal/session/runtime 적절한 전체/집중 테스트, app focused, i18n, strict clippy, fmt/diff, xtask check-boundary를 실행하고 실제 결과만 기록한다.
- [x] 앱 재빌드/재실행·시각 검증과 Linux/Windows/Ghostty 실환경 검증은 실행하지 않았으면 대기로 남긴다.

### Task 7: stacked PR 게시
- [x] 한국어 구현 커밋 `4ca0ef7` 후 `git push -u origin feat/scrollback-live-policy`.
- [x] `gh pr create --base feat/scrollback-policy-contract --head feat/scrollback-live-policy --body-file ...`로 PR #165를 생성하고 PR #157 의존성을 명시했다. PR C vendor reflow는 선병합하지 않았다.
- [x] handoff에 head/PR/테스트/리뷰/잔여 외부 검증과 다음 명령을 기록했다. force-push/rebase 없음. 원격 Actions는 계정 결제/spending 제한으로 job 미시작 BLOCKED다.

### 최종 복원 계약 (2026-09-08 root 확정)
- [x] 독립 감사/복구 ANSI 로그는 설정 변경으로 삭제하거나 재작성하지 않는다. storage manifest/ceiling diff는 제거한다.
- [x] ACK `durable=false`는 영속 로그 삭제를 보장하지 않는 경계이며 적용 실패가 아니다. non-Unix에서도 backend Applied이면 완료로 표시한다.
- [x] 현재 프로세스 내 lower→raise는 과거를 재생하지 않는다. 초기 복원은 당시 저장된 설정으로 로그를 한 번만 재생하며 이후 증가는 앞으로 들어올 출력의 용량만 늘린다.
- [x] 시작 catalog의 최대256개 pending persistent identity만 개별 낮은 상한을 유지한다. 새 세션과 증가 후 생성 archive에 과거 global minimum을 적용하지 않는다.
- [x] 메모리 archive는 자체 entry limit, 디스크 archive는 해당 SessionId limit을 사용한다. respawn은 현재 materialized Session backend 소유권을 이어받고 원본 ANSI를 재생하지 않는다.
- [x] 복원 명령 안의 ACK는 세션 편입 전체가 끝난 뒤 집계한다. `restored` 표시로 초기 빈 worker ACK와 복원 완료 ACK를 구분한다.
- [x] 초기 HostConfig 정책은 client를 반환하기 전 첫 명령으로 admitted되므로 RestoreWorkspace가 앞지르지 않는다.
- [x] terminal90+session58 전체PASS, runtime287 전체PASS(--test-threads=2). 앱 실화면/IME는 실행하지 않았으므로 대기.
- [x] TLS 포함 app18 PASS. Settings21, i18n8, core/app strictclippy, boundary/fmt/diff의 기존 PASS 후 마지막 TLS 보완 소스에서 관련 gate를 재확인한다.
- [x] 최종 gate 기록, 커밋/push/stacked PR #165. 원격 CI와 앱 시각/플랫폼 검증은 미완료로 구분한다.

### 게시 후 독립 리뷰 보완: 대형 resize와 tail 아카이브
- [x] #160 exact head를 일반 merge하고 Term의 두 grid가 streaming reflow를 사용함을 확인한다.
- [x] 실제 100k 컬러 이력의 32MiB 초과에서 archive 없이 pane을 detach하는 RED를 먼저 확인한다.
- [x] serializer의 Unsupported/LimitExceeded/Unavailable을 구분하고 Session이 history를 절반씩 줄여 최대 18회 안에 최신 tail을 보존한다. 화면 자체가 초과하면 삭제하지 않는다.
- [x] 압축 후 메모리 16MiB 예산도 확인한다. 새 archive가 자기 크기 때문에 즉시 축출되는 RED 후 유계 축소를 적용한다.
- [x] 지원 backend는 보존 성공 후에만 제거한다. 실패한 exited SessionId는 한 번 기억해 반복 pump 직렬화를 막고 제거 시 정리한다. Ghostty 미지원 detach와 독립 ANSI 로그 보존은 유지한다.
- [x] 실제 memory restore와 disk write/read/finish 및 audit 미변경, 화면만 초과하는 회귀를 검증한다. 최종 terminal91/Session60/runtime289 전체, vendor streaming4+memory1, core strict Clippy/fmt/diff PASS.
- [x] 100k 메모리 측정 PASS 및 좁은 CLI 재리뷰 잔여 확정 P1/P2 없음(exit 0)을 기록했다. 최종 runtime292/Session60/terminal91, strict Clippy/fmt/diff PASS. handoff/일지/PR 본문 갱신 후 일반 commit/push한다.
- [x] CLI 추가 P1인 자동 exit→close_pane 경로도 RED 후 보완한다. disk marker 또는 bounded memory tail을 먼저 확보한 뒤 정상 pane close, 둘 다 실패만 fail-closed. 명시적 ClosePane은 폐기를 유지한다. 새 memory tail이 remove_session 이후 남으며 개수/바이트 LRU를 따르는 회귀를 검증한다.
