# 전체 리뷰 결함·성능 후보 수정 Implementation Plan

> **For agentic workers:** 사용자가 지정한 GPT-6.1-Sol/xhigh 병렬 작업으로 아래 분리 범위를 실행한다. Root가 회귀 결과와 코드를 검토한 후 canonical에 통합한다.

**Goal:** 2026-10-14 전체 리뷰의 R01–R12를 해결하고 P01을 개선하며, C01/C02는 실행 가능한 실측·지원 범위 확인 후 필요한 수정을 적용한다.

**Architecture:** 기존 worker/authority/세션 독립성을 유지한다. 파일·DB는 동일 객체와 쓰기 순서를 검증하고, UI는 I/O 대기·무제한 입력을 피한다. lifecycle/document/data 3개 detached worktree가 현재 dirty 소스를 그대로 가진 상태에서 서로 다른 파일 또는 명시한 App 영역을 수정한다. Root만 canonical과 Cargo slot을 소유한다.

**Tech Stack:** Rust 2024, egui, sync PTY/tungstenite, SQLite/rusqlite, macOS 및 기존 플랫폼 cfg. 외부 사용자 세션·DB·키·서비스는 수정하지 않는다.

## 기준과 통합 규칙

- Canonical `/Users/jr/Desktop/projects/deppy-sijo-performance`, HEAD d8826f0b, source/test0.12.1. 기존 dirty 및 9개 수정/신규 제품 파일을 보존한다.
- 시작 snapshot `/private/tmp/deppy-full-fix-baseline-20261014/source-baseline.json` 및 `products-before.patch`, 412개 source/manifests. 비교·통합은 HEAD가 아니라 이 snapshot 기준이다.
- lifecycle `/private/tmp/deppy-full-fix-lifecycle-20261014`: R01/R09/R10.
- documents `/private/tmp/deppy-full-fix-documents-20261014`: R02 문서 부분/R03/R04/R08/R11/C01.
- data `/private/tmp/deppy-full-fix-data-20261014`: R05/R06/R07/R12 coordinator/C02.
- Root: R02 UI usage, R12 App completion 보존, P01, 통합·교차 리뷰·검사·공통 문서.
- 모든 worker는 canonical/Cargo/app/commit/push를 변경하지 않는다. 초기 test-only 변경을 먼저 전달하고 Root가 실제 compile/RED를 검증한다. 이전 실제 RED도 동일 baseline SHA일 때 근거로 재사용한다. 컴파일 실패/0 selected는 RED/PASS로 인정하지 않는다.
- 현재 사용자 요청은 코드 수정이다. 재실행·배포·커밋·푸시 요청은 없고 해당 작업을 자동 수행하지 않는다. 테스트 빌드는 source version 유지; 사용자용 release 시 별도 버전 증가와 binary/bundle 검증이 필요하다.

## Task 1: 종료·정지·후속 셸 경계 — lifecycle

**Files:** `crates/app/src/codex_app_server.rs`, `crates/pty/src/lib.rs`, `crates/runtime/src/agent_wrapper.rs`, `crates/session/src/session.rs` 및 필요한 동일 담당 crate의 전용 테스트/내부 helper. App main은 Root 소유다.

- [x] 기존 tests-only 재현의 Codex stalled stdin, foreground freeze, late-query token 비교 교정 테스트를 영구 회귀로 추가한다.
- [x] Root가 각 actual regression compile→RED를 확인하고 결과를 전달한다.
- [x] R01: blocking child write가 stop/deadline을 관측하도록 바꾼다. payload queue에 대한 기존 byte/cap 제한, 출력 event drain, child reap을 보존한다. child를 먼저 종료할 수 있는 소유권과 bounded join을 적용하고 정상 전송도 확인한다.
- [x] R09: 초기 shell PGID 외의 실제 foreground 작업도 identity를 검증해 동결·재개한다. stopped 상태만 frozen 성공으로 표시하며 무관한 프로세스를 대상으로 하지 않는다.
- [x] R10: 종료 전 출력의 query와 fallback 입력 소유권을 연결한다. time-0 drain에 지연을 임의로 더해 사용자 입력을 삼키지 않는다. 지연 pump, marker 분할/동일 chunk, 새 shell/agent의 legitimate query, 실제 입력 보존을 검증한다.
- [x] Root targeted GREEN, PTY/session/runtime/Codex 기존 회귀, strict Clippy로 검증하고 patch를 검토한다.

**Commands:** Root 단일 slot에서 `cargo test --offline --locked -p deppy-sijo --bin deppy-sijo full_review_20261014_codex_stalled_stdin_shutdown_is_bounded -- --nocapture --test-threads=1`, `cargo test --offline --locked -p pty full_review_20261014_freeze_stops_foreground_job_not_only_shell -- --nocapture --test-threads=1`, `cargo test --offline --locked -p runtime full_review_20261014_late_agent_query_cannot_reach_fallback_shell -- --nocapture --test-threads=1`. 첫 실행은 보호 기대 실패, 수정 후 통과가 기준이다.

## Task 2: 문서 load/save·소유권·입력 한계 — documents

**Files:** `crates/app/src/document_io.rs`, `session_aux.rs`, `ui/document.rs`, `ui/workspace.rs`, `folder_identity.rs`, 필요한 문서 helper/tests. `app.rs`는 문서 open/save/scope/close/editor 및 SetProjectPath 영역만. 새/변경 popup은 `docs/design/popup-components.md`와 사례26 HTML을 읽고 기존 공용 dialog를 재사용한다.

- [x] document FIFO·symlink·temp-write 중 외부 수정, closed-owner dirty 복구, egui oversized paste를 실제 생산 경로 회귀로 추가한다.
- [x] Root compile→RED 및 fixture cleanup을 확인한다.
- [x] R02: nonblocking open 후 동일 handle의 regular-file/byte 경계를 검사한다. 일반 symlink 허용 정책은 R08과 함께 일관되게 처리한다.
- [x] R03/R08: temp 작성 완료 후 commit 직전 original target identity/revision을 재검증한다. 링크 문서는 읽은 target에 저장하거나 명시적으로 편집을 거부한다. 교체·외부 수정에 conflict를 반환하고 source를 유지한다.
- [x] R04: session 독립성을 유지하며 dirty owner가 닫혀도 사용자가 내용에 접근·복구할 수 있게 한다. 다른 살아 있는 세션의 문서를 자동으로 공유하지 않는다. 닫기 보호와 명시적 same-path 복구, 새 owner의 scope 처리 및 종료 손실 경계를 검사한다.
- [x] R11: insert 전에 UTF-8 byte/전체 document budget을 적용한다. 과대 paste가 기존 선택을 지우거나 undo로 우회하지 않도록 실제 egui 입력을 검증한다. 현재 1MiB 초과 편집 보존 정책과 8MiB save 상한을 일관되게 유지한다.
- [x] C01: 현재 Windows cfg의 실패 조건을 측정한다. 설치된 Windows target의 cross-check 가능성도 확인한다. 비 Unix 경로 검증과 identity 지원을 분리하되 invalid path 선택은 기존 경로를 보존한다. Windows 실행이 불가능하면 host 실행 helper와 cross compile 결과를 구분해 기록한다.
- [x] Root document/scope/editor/path targeted GREEN 및 full App/기존 popup 회귀를 실행한다.

**Commands:** 우선 `cargo test --offline --locked -p deppy-sijo --bin deppy-sijo full_fix_20261014_document -- --nocapture --test-threads=1`. 0 selected는 인정하지 않는다. 실제 production document probe의 `fifo`, `symlink`, `save-race`를 baseline→수정 후 비교한다. editor는 egui Paste 경로를 사용하고 메모리·레이아웃 전후 측정을 기록한다.

## Task 3: 저장·상태 순서·import 완료·Relay — data

**Files:** `crates/storage-core/src/lib.rs` 및 필요한 crate manifest, `crates/core/src/fs.rs`, `crates/runtime/src/persistence.rs`, `crates/storage/src/write_worker.rs`(필요 시), `crates/connector-service/src/coordinator.rs`, `crates/web-remote/src/relay_client/{tls,worker}.rs` 및 전용 회귀. App R12 연결은 Root가 담당한다.

- [x] actual storage-core WAL backup 누락, fixed-temp symlink/동시 저장, queued exit→respawn, full command queue import 재현을 영구 회귀로 먼저 추가한다.
- [x] Root 각 compile→RED를 확인한다.
- [x] R05: reader transaction+WAL writer가 있어도 migration 전 backup이 일관된 snapshot을 보관하게 한다. 실패하면 기존 backup을 훼손하지 않고 migration을 진행하지 않는다. backup byte copy/checkpoint 결과 무시는 제거한다.
- [x] R06: 고유 temp를 배타적으로 생성하고 flush/rename 및 실패 cleanup을 안전하게 처리한다. 원래 파일 mode와 정상 설정 저장 호환성을 검증한다.
- [x] R07: 동일 persistent UUID의 queued/direct write 순서 또는 generation을 보장한다. 기본 interval·flush·오류·respawn 후 기존 archive 연결을 유지한다.
- [x] R12: rejected completion의 pending ownership/payload를 보존한다. retry/cancel/stale/worker unavailable을 구분하고 enqueue 성공 전 consume하지 않는다. Root App completion lane과 인터페이스를 합의한다.
- [x] C02: byte drip receive 및 실제 worker shutdown을 Plain과 가능한 실제 loopback Rustls stream에서 측정한다. 재현된 total-deadline/cancel 문제는 수정하되 production endpoint/credential BLOCKED를 해제하지 않는다. normal send/receive 및 fragmented message 유지를 검증한다.
- [x] Root targeted GREEN, 해당 crate full tests, strict Clippy를 실행한다.

**Commands:** `cargo test --offline --locked -p runtime all_review_20261014_respawn_must_retire_queued_exit_status -- --nocapture --test-threads=1`; `cargo test --offline --locked -p connector-service all_review_20261014_import_completion_survives_command_backpressure -- --nocapture --test-threads=1`; `cargo test --offline --locked -p storage-core -- --test-threads=1`; `cargo test --offline --locked -p deppy-core -- --test-threads=1`; `cargo test --offline --locked -p web-remote relay -- --test-threads=1`. RED/protection failure와 fixture 자체 오류를 구분한다.

## Task 4: App 연결·탐지 성능 — Root

**Files:** `crates/app/src/app.rs`의 usage UI/host completion lane, `crates/app/src/agent_detect.rs`, 필요한 usage worker helper/tests, locale/문서는 필요한 경우만.

- [x] R02 UI usage: FIFO probe의 실제 RED를 재사용하고 worker/cache의 nonblocking frame test를 추가한다. UI frame은 파일 I/O·join 없이 마지막 bounded snapshot을 표시한다. failure/retry/정상 2초 cache와 fresh window 동작을 유지한다.
- [x] R12 App: dispatch Backpressure/일시 unavailable 완료를 bounded lane에 보관한다. 사용자 cancel/stale owner 이후 payload는 버린다. 결과를 이미 take/join한 뒤 tracing만 남기지 않게 한다. coordinator 테스트와 App 실제 완료 재전달을 함께 검증한다.
- [x] P01: 변경 전 actual function optimized benchmark를 기록한다. process snapshot당 children adjacency를 한 번 생성하고 각 session 탐지에서 재사용한다. 기존 cap·cycle·empty/oversized snapshot·동일 workspace 독립 session binding을 보존한다. 동일 rows/session 수의 전후 비용을 비교한다.
- [x] lifecycle/document/data의 patch를 시작 baseline 기준으로 대조하고 소유 영역 외 diff를 거부한다. App 인접 변경은 manual merge 후 호출부·수명·예산을 재검토한다.

## Task 5: 최종 통합·검증·보고

- [x] 모든 수정 항목과 C01/C02 실측 결과를 report ID별로 대조한다. 수정하지 않은 조건부 항목은 이유·증거·남은 실행 범위를 명시한다.
- [x] Root `cargo test --offline --locked --workspace --no-fail-fast -- --test-threads=1` 실행. 실제 test 결과만 집계하고 ignored/동일 subprocess 중복은 제외한다.
- [x] 변경 crate all-targets strict Clippy, fmt, UI boundary, diff 검사 및 필요한 JS syntax를 실행한다.
- [x] worker 교차 read-only review를 병렬 수행한다. 새 결함은 담당 lane에서 수정하고 영향을 받는 검사만 재실행한다.
- [x] plan 완료 체크, QA/evidence·source/patch SHA·실패 접근·실측 제한·handoff를 업데이트한다. 기존 변경 보존을 시작 snapshot과 대조한다.
- [x] 사용자에게 수정 결과·조건부 실측 결과·테스트·버전/재실행 범위를 보고한다. 이번 요청 없이 commit/push/restart/release하지 않는다.

## 최종 검증 기록 — 2026-10-15

- 전체 workspace 명령은 실제 exit101(5679 PASS / 22 FAIL / 65 ignored)이다. 이력 source-scan 범위와 SQLite NOFOLLOW의 부모 링크 처리 두 원인을 교정하고 App/storage/storage-core 전체 재검사 exit0(3747 PASS / 42 ignored)를 확인했다. target별 최신 결과 5702 PASS / 0 FAIL / 65 ignored이며 전체 단일 명령 exit0로 표현하지 않는다.
- workspace all-targets strict Clippy, fmt, boundary, deps 및 popup inlineJS 검사가 통과했다. 최종 스타일 정리 후 App 신규32 / SessionAux4 / 이력1 / coordinator83(2ignored)도 각 exit0로 재확인했다.
- Windows offline cross-check는 미캐시 curve25519-dalek-derive0.1.1에서 컴파일 전 중단했다. C01 host helper만 통과로 기록한다. C02는 Plain baseline RED 및 실제 loopback TLS 수정 후 실측으로 판단했고 production BLOCKED 유지.
- 최종 QA/source snapshot/증분 patch/실행 로그/실패 접근/한계는 docs/qa/2026-10-14-full-review-fixes.md 및 evidence README에 보관했다. 현재 요청에 따른 소스 수정·검증을 완료했으며 commit/push/release/restart는 수행하지 않는다.
