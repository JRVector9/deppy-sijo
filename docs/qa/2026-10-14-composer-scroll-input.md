# 스크롤백 열람 중 하단 입력 거절 수정

기준: `6a06d319`, canonical `/Users/jr/Desktop/projects/deppy-sijo-performance`.
사용자 요청: 터미널이 맨 아래로 스크롤되지 않았을 때 하단 입력이 되지 않는다.

## 확인한 원인

실행 앱의 2026-10-14 로그에서 03:33:52~55Z의 Codex `ExplicitPrompt` 거절 7건이 모두 `current_choice_dialog`였다. 내용 없는 진단 태그만 확인했으며, 실제 사용자 세션에 시험 입력을 보내지 않았다.

`Session::input_guard_snapshot()`은 기존에 `backend.viewport_snapshot()`을 반환했다. 이 값은 실제 입력 화면 대신 사용자가 보고 있는 과거 화면이다. `prompt_row()`는 스크롤 중이면 현재 편집기를 확인하지 못하고, `choice_dialog()`는 과거 문구를 검사하거나 화면 밖 커서 때문에 잘못된 범위를 현재 선택창으로 취급한다. 실제 초안이 있는 현재 화면을 스크롤로 가리면 반대로 입력을 허용하는 경우도 재현했다.

## 수정

- `TerminalBackend::input_snapshot()`으로 입력용 native screen 읽기 계약을 분리했다.
- 기본 Alacritty backend는 실제 grid의 현재 행과 편집기 커서를 직접 읽는다. 렌더 cache, scroll offset, pending damage를 변경하지 않으며 style와 Unicode grapheme을 같은 공통 셀 변환으로 보존한다.
- 스크롤백 질의의 65,536셀 표시 제한을 입력 화면에 적용하지 않는다. runtime 치수 제한 내의 500×132 native grid도 검사했다.
- Session 입력 검사만 이 경로를 사용한다. 입력 근거를 제공하지 않는 backend는 `live_screen_unavailable`로 거절하고 과거 화면을 현재 입력 근거로 사용하지 않는다.
- 전송 권한·실행 identity·현재 초안/선택창 보호·저장 및 재전송 정책은 유지했다. 입력 검사를 위해 사용자의 화면을 맨 아래로 이동하지 않는다.
- 기존 이력 질의 테스트가 viewport 관측에 입력용 helper를 사용하던 부분은 native snapshot을 사용하도록 교정했다. 질의 전 dirty를 복원하고 최종 native offset3·dirty 보존 및 입력용 offset0을 검증한다.

## 실제 검증

| 검사 | 결과 | 로그 |
| --- | --- | --- |
| runtime 새 회귀, 수정 전 | 0 통과 / 2 실패 | `/private/tmp/deppy-composer-scroll-red-runtime.log` |
| Session 새 회귀, 수정 전 | 0 통과 / 1 실패, offset200 vs0 | `/private/tmp/deppy-composer-scroll-red-session.log` |
| runtime 새 회귀, 수정 후 | 2 통과 | `/private/tmp/deppy-composer-scroll-green-runtime.log` |
| 최종 runtime 전체 | 392 통과 / 1 실패 | `/private/tmp/deppy-composer-scroll-affected-final.log` |
| 최종 Session 전체 | 81 통과 | `/private/tmp/deppy-composer-scroll-terminal-session.log` |
| 최종 terminal 전체 | 137 통과 / 기존 수동 검사4개 ignored | 같은 로그 |
| 최종 Session geometry 보정 후 | 81 통과 / doc0 | `/private/tmp/deppy-composer-scroll-session-final.log` |
| App 하단 입력 관련 | 97 통과 | `/private/tmp/deppy-composer-scroll-app.log` |
| 4crate strict clippy all-targets | exit0, 경고 없음 | `/private/tmp/deppy-composer-scroll-clippy.log` |
| crate boundary | exit0 | `/private/tmp/deppy-composer-scroll-boundary.log` |
| macOS 메모리 비교 단독 | 0 통과 / 1 실패 | `/private/tmp/deppy-composer-scroll-memory-isolated.log` |

새 runtime 회귀는 별도 실제 `/bin/cat` PTY에서 Claude/Codex 입력 표시, 5줄/최상단 스크롤, 과거 선택창, 긴 한글·개행 전송을 검사한다. 입력이 PTY 출력에 도달하는 것과 전송 판정 직후 과거 viewport가 유지되는 것을 확인한다. 현재 단일·다중행 초안과 선택창의 거절도 검사한다. Session 회귀는 화면 스타일·한글·커서·큰 geometry와 렌더 damage를, terminal 회귀는 vi 탐색 커서와 alternate screen을 검사한다.

첫 전체 runtime 실행은 391 통과 / 2 실패였다. 하나는 입력 helper 계약이 바뀐 기존 이력 테스트였으며 위와 같이 교정했다. 다른 하나는 이번에 수정하지 않은 `crates/runtime/src/resource_monitor.rs:1013`의 macOS `phys_footprint < ps RSS` 가정이다. 해당 파일은 기준 커밋 대비 변경 없고 단독 실행도 실패했다(footprint6,407,344 / RSS6,389,760). 전체 runtime 통과로 기록하지 않는다.

최종 고유 테스트 707개 통과, runtime의 기존 메모리 비교 1개 실패, 기존 수동 검사4개 ignored다. 새 회귀와 반복 실행을 중복 합산하지 않았다. `cargo fmt --all --check`와 `git diff --check`도 실제 exit0이었다. Root가 모든 변경과 입력용 snapshot 호출부를 대조했고 별도 소스 수정이 필요한 추가 결함은 확인하지 못했다.

실제 실행 명령:

```sh
CARGO_BUILD_JOBS=2 cargo test --locked -p runtime composer_scrollback -- --nocapture
CARGO_BUILD_JOBS=2 cargo test --locked -p session input_guard_reads_live_screen -- --nocapture
CARGO_BUILD_JOBS=2 cargo test --locked -p terminal -p session -p runtime
CARGO_BUILD_JOBS=2 cargo test --locked -p runtime resource_monitor::tests::macos_자식_다수의_footprint_합은_ps_rss_합보다_작다 -- --exact --nocapture
CARGO_BUILD_JOBS=2 cargo test --locked -p terminal -p session
CARGO_BUILD_JOBS=2 cargo test --locked -p session
CARGO_BUILD_JOBS=2 cargo test --locked -p deppy-sijo --bin deppy-sijo composer
CARGO_BUILD_JOBS=2 cargo clippy --locked -p terminal -p session -p runtime -p deppy-sijo --all-targets -- -D warnings
CARGO_BUILD_JOBS=2 cargo run --locked -p xtask -- check-boundary
cargo fmt --all --check
git diff --check
```

## 범위

소스 및 격리 테스트만 실행했다. 실행 중인 사용자 앱을 종료·재실행하거나 사용자 DB/프롬프트/클립보드를 변경하지 않았다. 실험 Ghostty와 live cell 읽기가 없는 외부 backend의 스크롤 중 입력 허용은 검증하지 않았으며, 이 backend들은 근거가 없으면 거절한다. 실제 Claude/Codex 계정의 E2E 테스트로 주장하지 않는다.

사용자용 artifact를 빌드·배포하지 않았으므로 source/test version0.12.1을 유지한다. 다음 앱 배포는 별도 버전 증가 및 binary/bundle 버전 검증이 필요하며, 재실행은 현재 요청의 명시적 승인이 필요하다.
