# 팝업 동작 리뷰 3건 수정 — 2026-10-02

원인과 이전 재현 결과: [팝업 동작 코드 리뷰](2026-10-02-popup-behavior-code-review.md).

## 수정 내용

| 대상 | 이전 | 수정 후 |
| --- | --- | --- |
| MCP 연속 승인 | 이전 허용 버튼 포커스로 다음 요청도 Enter 승인 | 고정 창 Area·두 고정 버튼 ID 묶음, 대상 변경 시 포커스 해제 |
| 휴지통 실패 확인 | 첫 표시 전에 배경 단축키 통과 | FileTree 대기 확인 상태를 App의 전역 입력 차단에 포함 |
| 숨긴 파일 목록의 삭제 확인 | 접힌 패널·메모 탭에서 확인창이 보이지 않음 | 목록 가시성과 분리해 panel 진입 시 한 번 표시, Esc 취소 가능 |
| 이름 변경 확정 | egui 이벤트만 제거하고 native 붙여넣기가 터미널로 전달 | 소유한 native batch도 소비, 확정 pass 끝까지 기존 입력 차단 유지 |

워크스페이스와 세션 이름 편집은 같은 도우미를 사용하므로 입력 소유권 수정이 함께 적용된다. 다음 pass에는 입력 차단이 해제되며 일반 터미널 입력이 다시 전달된다.

## 테스트와 코드 리뷰

최초 3건은 정식 App 회귀 테스트로 옮긴 뒤 현재 코드에서 실제 **3 failed**를 확인했다. 수정 후 **3 passed**였고, 추가 검증을 포함한 집중 테스트는 **4 passed**였다.

첫 독립 소스 리뷰는 두 가지를 지적했다. 삭제 확인을 일찍 차단하면서 표시까지 파일 목록 상태에 의존하면 입력이 막힌 채 남을 수 있었고, 새 테스트가 process-wide native 큐에 넣은 신호를 다른 병렬 테스트가 가져갈 수 있었다. 각각 다음과 같이 처리했다.

- 파일 목록 숨김 회귀 테스트를 추가하고 실제 **1 failed**를 확인한 뒤, 기존 확인창 렌더를 panel의 표시 분기 앞으로 옮겼다. 패널 접힘과 메모 탭 두 상태에서 표시·취소를 검증한다.
- 테스트 스레드 간 큐 격리 회귀 테스트에서 실제 **1 failed**를 확인했다. `cfg(test)`에서는 스레드별 큐를 사용하고, 실제 앱은 기존 전역 Mutex·용량 제한·poison fallback을 유지한다. 저장소 접근 도우미를 통해 record/drain/peek가 같은 batch 처리 로직을 사용한다.

추가 독립 리뷰에서 egui 0.36.1의 포커스 ID 캐시가 방향키 탐색 전에 자동 정리되지 않는다는 지적을 확인했다. 20개 요청의 4개 버튼을 실제로 포커스한 회귀 테스트는 80개 ID로 실패했다. 마지막 요청 정보 하나와 두 개의 고정 버튼 묶음만 유지하도록 수정했으며, 최종 전체 테스트에서 같은 시나리오의 ID 수가 최대 8개임을 검증했다. 기존 요청 변경·정상 승인 검증도 통과했다.

| 최종 검증 | 결과 |
| --- | --- |
| App 전체, 8 스레드 병렬 | 2520 passed / 0 failed / 28 ignored, 8.74s |
| Connector UI 전체 | 17 passed / 0 failed, 0.86s |
| App·Connector 전체 target Clippy, `-D warnings` | exit 0, 34.17s |
| fmt / diff 검사 | exit 0 |
| 시안 HTML script 구문 | passed |
| workspace metadata와 Cargo.lock 버전 | 27개 패키지 모두 0.5.4 |
| 최종 독립 소스 리뷰 | exit 0, 남은 actionable finding 없음 |
| 릴리스 빌드·패키지 검증 | exit 0, 19.92s; 서명·버전·ZIP 검증 passed |

정식 회귀 테스트 6개는 MCP 요청 교체, 영구 삭제 첫 pass, 파일 목록 숨김, 인라인 이름 편집과 실제 WorkspaceUi의 연결, 테스트 스레드 큐 격리와 연속 요청의 포커스 ID 상한을 다룬다. 한 테스트에서 검사하는 두 레이아웃을 테스트 두 개로 집계하지 않는다. MCP 새 버튼 선택 후 정상 승인, native paste 다음 pass 재생 방지, 이후 일반 Text의 터미널 전달도 같은 harness에서 검증한다.

```sh
cargo test --offline --locked -q -p deppy-sijo --bin deppy-sijo -- --test-threads=8
cargo test --offline --locked -q -p connector-ui -- --test-threads=1
cargo clippy --offline --locked -p deppy-sijo -p connector-ui --all-targets -- -D warnings
cargo fmt --all --check
git diff --check
```

로그: `/tmp/deppy-popup-fix-{red,green,focused,focused-final,hidden-red,queue-red,app-tests-final,connector-tests,clippy,cli-final,release-build}-20261002.log`. 최종 실행 로그는 `/tmp/deppy-popup-fix-{app-tests-release-final,connector-tests-release-final,clippy-release-final,cli-bounded,release-build-final,package-final,package-verify-final}-20261002.log`에 있다. 최종 소스 diff는 `/tmp/deppy-popup-fix-task-diff-20261002.patch`, 변경 전 파일은 `/tmp/deppy-popup-fix-before-20261002`에 보존했다.

## 코드·문서와 배포 상태

이번 변경 파일:

- `crates/app/src/app.rs`: FileTree 대기 확인 입력 차단, 공개 Connector UI 회귀 harness.
- `crates/app/src/ui/file_tree.rs`: 대기 확인 predicate와 독립 표시, 공통 이름 편집 batch 소비·pass 차단, 회귀 테스트.
- `crates/app/src/ui/workspace.rs`: 실제 FileTree→WorkspaceUi 입력 소유권 회귀 harness.
- `crates/app/src/native_key_monitor.rs`: 테스트용 native 큐 격리와 접근 도우미, 회귀 테스트.
- `crates/connector-ui/src/lib.rs`: operation별 승인 버튼과 포커스 전환.
- `Cargo.toml`, `Cargo.lock`: **0.5.3 → 0.5.4**.
- `docs/design/popup-components.md`, 해당 HTML 시안, 이 보고서와 handoff.

기존 dirty 작업을 보존했다. source base `166f8daeb1054cf09a07194fa83bf0a4a19d93ce`, 0.5.4 제품 diff hash `c85122b390332d35a30b9c069b03bf2174aa663b267fa8f69d34b8c8981881d4`. 제품 버전과 현재 source/artifact 기록은 `/tmp/deppy-popup-fix-release-source-20261002.json`에 유지한다.

실제 OS 클립보드를 읽거나 사용자 PTY에 입력하지 않았다. native 앱을 켜서 파일을 삭제하거나 프로세스를 종료하는 E2E도 실행하지 않았다. 재실행·앱 종료·커밋·푸시는 하지 않았다. 실행 중인 0.5.3 PID21631을 유지한다.

## 최종 산출물 확인

- 0.5.4 App/proxy 릴리스 빌드와 별도 Developer ID 서명 번들·ZIP 완료: `target/bundle-0.5.4/Deppy Sijo.app`, `target/bundle-0.5.4/Deppy Sijo.zip`.
- 컴파일된 reported-version marker `Deppy0.5.4`, `CFBundleShortVersionString`, `CFBundleVersion` 모두 0.5.4. 27개 workspace metadata/lock 버전도 동일하다. 앱을 실행해 버전을 확인한 것은 아니다.
- strict nested signature, architecture, 추출 ZIP과 bundle 일관성 검증 통과. 로컬 개발 검증이며 공증·Gatekeeper 배포 승인까지 수행했다는 의미는 아니다.
- 마지막 소스 hash와 빌드 전 hash 일치. 기존 0.5.3 bundle/ZIP hash도 그대로이며 PID21631이 기존 bundle에서 실행 중임을 확인했다.
- 첫 서명 패키지에는 최종 리뷰 전 구현이 들어 있어 사용자에게 배포하지 않고 `/tmp/deppy-popup-fix-pre-final-bundle-20261002`로 보존했다. 현재 `target/bundle-0.5.4`는 모든 수정과 최종 테스트·리뷰 후 다시 빌드한 산출물이다.
- 요청한 3건의 구현·회귀 테스트·최종 리뷰·재빌드 완료. 확인된 미해결 수정 사항은 없다. 재실행은 하지 않았다.
