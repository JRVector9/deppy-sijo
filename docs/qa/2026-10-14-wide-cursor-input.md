# 한글 안내문 커서 좌표에 따른 하단 입력 거절 수정

기준 커밋: `d8826f0bdb8d89255f3448b5b61b6eee83faa87a`.
작업 경로: `/Users/jr/Desktop/projects/deppy-sijo-performance`.
직전 [리뷰 R1](../reviews/2026-10-14-composer-scroll-input-review.md)에 대한 사용자 수정 요청.

## 원인과 변경

입력용 snapshot이 native cursor를 그대로 복사하면서 기존 렌더 경로의 `WIDE_CHAR_SPACER` 보정을 누락했다. Codex의 dim 한글 안내문 첫 글자의 뒷칸에 커서가 있으면 col2 대신 col3이 전달되어 빈 안내문을 실제 초안으로 판정하고 전송을 거절했다. 맨 아래에서도 재현됐다.

`AlacrittyBackend::input_snapshot()`에서 반환할 좌표만 문자 소유 칸으로 보정했다. 실제 native cursor나 사용자의 스크롤 위치를 이동하지 않으며 vi 탐색 커서를 입력 근거로 사용하지 않는다. 입력 guard 정책은 변경하지 않았다.

## 실제 검사

| 검사 | 결과 | 로그 |
| --- | --- | --- |
| 새 terminal 회귀, 수정 전 | 0 통과 / 1 실패: col3 vs2 | `/private/tmp/deppy-wide-cursor-red-terminal-20261014.log` |
| 새 runtime 회귀, 수정 전 | 1 통과 / 1 실패: AdmissionDenied vsOk | `/private/tmp/deppy-wide-cursor-red-runtime-20261014.log` |
| terminal input_snapshot, 수정 후 | 2 통과 | `/private/tmp/deppy-wide-cursor-green-terminal-20261014.log` |
| runtime composer 전체 | 8 통과 | `/private/tmp/deppy-wide-cursor-green-runtime-20261014.log` |
| terminal 전체 | 138 통과 / 기존 수동 검사4개 ignored | `/private/tmp/deppy-wide-cursor-terminal-session-20261014.log` |
| session 전체 | 81 통과 / doc0 | 같은 로그 |
| App composer | 97 통과 | `/private/tmp/deppy-wide-cursor-app-composer-20261014.log` |
| 4crate all-targets strict clippy | exit0, 경고 없음 | `/private/tmp/deppy-wide-cursor-clippy-20261014.log` |
| crate boundary | exit0 | `/private/tmp/deppy-wide-cursor-boundary-20261014.log` |
| cargo fmt / git diff --check | 실제 exit0 | 최종 명령 실행 결과 |

최종 관련 테스트는 중복 없이 **324 통과**, 기존 수동 검사4개 ignored다. 위 terminal input_snapshot 2개를 전체 terminal 검사와 중복 합산하지 않았다. 전체 runtime 재실행으로 주장하지 않는다. 이전 runtime 전체의 별도 macOS 메모리 비교 실패는 기존 QA의 과거 기록이며 이번 검사 범위 밖이다.

제품 변경·테스트 및 입력용 snapshot 호출부를 최종 직접 대조했다. native cursor 선택은 그대로 유지했고 반환 좌표의 wide owner 보정만 기존 vendor 렌더 동작과 맞췄다. 추가 수정이 필요한 결함은 확인하지 못했다. 소스와 문서는 미커밋 상태다.

추가한 회귀:

- 한글·CJK·이모지의 wide 뒷칸 좌표, primary/alternate screen, primary scroll, 실제 grid cursor와 viewport 불변을 검사한다.
- 별도 실제 `/bin/cat` PTY에서 맨 아래·5줄 스크롤·최상단에서도 한글 개행 입력이 전달되고 전송 직후 viewport가 유지되는 것을 검사한다.
- 같은 scroll 조건에서 실제 한글 초안, 안내문 안쪽으로 이동한 커서, 자동 전송은 계속 거절되는 것을 검사한다.
- 기존 vi cursor/alternate screen, 현재 선택창/초안, 긴 입력, stale stream 상태의 6개 runtime 회귀를 함께 재실행했다.

실행 명령:

```sh
CARGO_BUILD_JOBS=2 cargo test --locked -p terminal input_snapshot_normalizes_wide_cursor -- --nocapture
CARGO_BUILD_JOBS=2 cargo test --locked -p runtime composer_wide_ -- --nocapture
CARGO_BUILD_JOBS=2 cargo test --locked -p terminal input_snapshot -- --nocapture
CARGO_BUILD_JOBS=2 cargo test --locked -p runtime composer_ -- --nocapture
CARGO_BUILD_JOBS=2 cargo test --locked -p terminal -p session
CARGO_BUILD_JOBS=2 cargo test --locked -p deppy-sijo --bin deppy-sijo composer
CARGO_BUILD_JOBS=2 cargo clippy --locked -p terminal -p session -p runtime -p deppy-sijo --all-targets -- -D warnings
CARGO_BUILD_JOBS=2 cargo run --locked -p xtask -- check-boundary
cargo fmt --all --check
git diff --check
```

## 범위

사용자의 실행 앱·에이전트·DB·클립보드에는 시험 입력을 보내지 않았다. 실제 외부 provider 계정 또는 GUI E2E 검증으로 주장하지 않는다. 이전 QA에 명시된 실험 Ghostty 지원 범위는 이번 변경에 포함하지 않았다.

소스/격리 테스트만 변경했으며 사용자용 artifact를 빌드·배포하거나 앱을 재실행하지 않았다. source/test 버전은 `0.12.1`이다. 다음 사용자용 빌드 배포에는 별도 버전 증가와 artifact 버전 검증이 필요하다.
