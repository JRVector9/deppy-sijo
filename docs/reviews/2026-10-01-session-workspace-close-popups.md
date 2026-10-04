# 세션·워크스페이스 종료 공용 팝업 적용 — 2026-10-01

## 완료 내용

- 사례07 세션 종료와08 워크스페이스 전체 세션 종료의 표시 사양을 `crates/app/src/ui/session_close_dialogs.rs`에 모았다. 실제 WorkspaceUi/App이 같은 공용 함수를 호출한다. 제목·설명·경고·400pt 셸·34pt 버튼·하단 여백은 기존 공용 popup 컴포넌트를 재사용한다.
- 대상이 다른데 표시 이름이 같은 경우도 pane/workspace ID로 구분한다. 이전 확인 버튼의 포커스로 Enter가 새 대상을 종료하는 오류를 재현하고 수정했다. 기존 문서 팝업의 포커스 초기화를 `popup::prepare_target`으로 옮겨 문서와 종료 팝업이 함께 사용한다. action ID는 대상별로 나누고 Modal Area/하단 높이 캐시는 케이스별로 유지한다.
- 취소·X·Esc·바깥 클릭은 기존 취소 동작이다. 명시적으로 확인한 대상만 기존 종료 명령 경로로 보낸다. 입력/IME/전역 단축키 차단, 종료 대상 캡처, workspace confirmation ON/OFF 설정을 유지한다. 워크스페이스 자체를 삭제하지 않는다.
- 실제07/08 함수의 PNG를 화면 밖에서 렌더링해 폰트·버튼·본문·하단 위치와 잘림 여부를 확인했다. App 프로세스를 실행하거나 종료하지 않았다.

**표시 정책:** 실행 중인 세션을 닫으면 확인창이 뜬다. 이미 종료된 세션 정리는 기존 즉시 닫기를 유지한다. 워크스페이스 전체 종료는 기존 설정의 `워크스페이스 종료 확인`을 켠 경우 확인창이 뜬다. 설정의 환경 및 API 프로젝트 목록 닫기09는 세션 종료와 다른 숨김 동작이므로 이번 변경 대상에 넣지 않았다.

## 변경 파일과 책임

| 파일 | 책임 |
| --- | --- |
| `ui/session_close_dialogs.rs` | 07/08 지역화·고정 케이스 ID·대상 ID·공용 확인 사양 |
| `ui/workspace.rs`, `app.rs` | 표시 함수 호출, 기존 대상 생존 확인과 종료 접수 |
| `ui/popup/confirmation.rs` | `confirmation_for_target`, 대상별 action ID·공용 Danger/Ghost 버튼 |
| `ui/popup/shell.rs`, `ui/popup/mod.rs` | 문서와 종료창이 공유하는 `prepare_target`, 케이스/viewport당 마지막 ID 하나 |
| `ui/document_dialogs.rs` | 기존 문서 동작을 같은 포커스 보호로 재사용 |
| `ui/mod.rs` | 표시 모듈 등록 |
| `docs/design/popup-components.md`, 번호 HTML | 07/08 코드 위치·역할·입력 보호·하네스 갱신 |
| root Cargo.toml/lock | 0.5.1→0.5.2, 27개 inherited workspace 버전 |

기존 다른 작업의 미커밋 코드와 원래 worktree를 보존했다. 이번 작업에서 새로운 sub-agent·커밋·푸시를 만들지 않았다.

## 실제 RED/GREEN과 최종 검증

| 검증 | 결과 | 로그/산출물 |
| --- | --- | --- |
| 다른 세션 대상에 이전 Close focus 전파 | RED: 새 pane도 종료됨 → GREEN: 새 명시 확인 전까지 보존 | `/tmp/deppy-close-popups-{red,green}-20261001.log` |
| 같은 이름의 다른 workspace에 이전 focus 전파 | RED: Enter가 Confirm 반환 → GREEN: 확인 없이는 None | 위 로그 |
| App bin 전체 | **2514 passed / 0 failed / 28 ignored**, 5.52s | `/tmp/deppy-close-popups-app-final-20261001.log` |
| i18n 전체 | **8 passed / 0 failed**, 0.02s | `/tmp/deppy-close-popups-i18n-20261001.log` |
| 실제07 offscreen renderer | 1 passed / 0 failed, 2.84s; PNG 직접 확인 | `target/popup-parity/07-session.png` |
| 실제08 App 표시 helper renderer | 1 passed / 0 failed, 1.82s; PNG 직접 확인 | `target/popup-parity/08-workspace.png` |
| App all-target Clippy | exit0, 39.93s | `/tmp/deppy-close-popups-clippy-20261001.log` |
| fmt / diff / UI boundary | 모두 exit0 | `/tmp/deppy-close-popups-boundary-20261001.log` |
| 독립 Codex 소스 리뷰 | exit0, 확인된 미해결 결함 없음 | `/tmp/deppy-close-popups-cli-review-20261001.log` |
| 0.5.2 App/proxy 릴리스 빌드 | exit0, 26.09s | `/tmp/deppy-close-popups-release-20261001.log` |
| 별도 bundle + 추출 ZIP 검사 | exit0; 바이너리/서명/architecture/버전 일치 | `/tmp/deppy-close-popups-package-20261001.log` |

포커스 회귀2개는 App 전체 테스트 수에 포함된다. PNG renderer2개는 ignored 테스트를 별도 명시적으로 실행한 결과다. 08은 App이 사용하는 실제 표시 함수를 검증하며 전체 App 화면 클릭 E2E가 아니다. 사용자 세션을 실제로 종료하거나 heap/RSS를 실측한 결과로 해석하지 않는다.

첫 workspace 테스트는 이후 렌더가 캡처한 결정을 덮어쓰는 fixture 오류가 있었다. 실제 소비자처럼 한 번 정한 결정을 유지하도록 수정한 뒤 위 제품 결함 RED를 확인했다. 제품 코드 수정 뒤 두 회귀 모두0.04s에 통과했다. 다른 실패한 구현 접근은 없었다.

```sh
cargo test --offline --locked -q -p deppy-sijo --bin deppy-sijo close_popup
cargo test --offline --locked -q -p deppy-sijo --bin deppy-sijo
cargo clippy --offline --locked -p deppy-sijo --all-targets -- -D warnings
cargo fmt --all -- --check
git diff --check
cargo run --offline --locked -q -p xtask -- check-boundary
cargo test --offline --locked -q -p deppy-sijo --bin deppy-sijo popup_parity_render_session_close -- --ignored --test-threads=1
cargo test --offline --locked -q -p deppy-sijo --bin deppy-sijo popup_parity_render_workspace_close -- --ignored --test-threads=1
cargo test --offline --locked -q -p i18n
```

리뷰는 이전0.5.1 source snapshot `/tmp/deppy-close-popups-before-20261001`과 이번353줄 source diff `/tmp/deppy-close-popups-task-diff-20261001.patch` 및 해당 실제 파일을 대상으로 했다. 다른 누적 변경을 이번 리뷰 결과로 재인증하지 않는다. 대상·모달 입력·하단 배치·문서 동작·종료 접수·설정 정책을 확인했다.

## 릴리스 기록

- **0.5.1 → 0.5.2** patch 증가. 존재하는 worktree의 이전 로컬 artifact 최대0.5.1을 확인했고 27개 inherited metadata/lock 버전을0.5.2로 검증했다.
- 새 별도 artifact: `target/bundle-0.5.2/Deppy Sijo.app`, `target/bundle-0.5.2/Deppy Sijo.zip`. App/proxy는 새 릴리스 바이너리, helper/notice·리소스는 이전 검증 bundle에서 재사용했다.
- 실행 파일 및 bundle 실행 파일의 `Deppy0.5.2` compiled reported-version marker, plist `CFBundleShortVersionString`과 `CFBundleVersion`의0.5.2를 앱 실행 없이 확인했다. Developer ID 서명과 추출 ZIP 검증 완료. 로컬 개발 bundle이며 Apple 공증 완료를 주장하지 않는다.
- Base commit `166f8daeb1054cf09a07194fa83bf0a4a19d93ce`, branch `fix/cloud-agent-ended-sessions`. 미커밋 결과를 포함하며 product diff SHA256은 **`cce017973b12438dccc452eec746129cba52b20242be1f8707a27d505a41f353`** 이다. Cargo.toml/lock + app/storage/i18n/connector-ui/runtime/session의 binary diff와 정렬된 미추적 제품 path+NUL+bytes를 결합했다. 빌드 뒤 불변을 재확인했다.
- 소스·버전·artifact hash 기록: `/tmp/deppy-close-popups-release-source-20261001.json`.
- 실행 중인 이전 앱 **0.4.5 / PID45213**은 유지했다. **재실행·앱 종료는 하지 않았다.**

```sh
cargo metadata --offline --format-version 1
cargo build --offline --locked --release -p deppy-sijo -p mcp-proxy
DEPPY_REQUIRE_TRUSTED_SIGNING=0 DEPPY_ALLOW_UNTRUSTED_SIGNING=1 sh scripts/verify-macos-package.sh 'target/bundle-0.5.2/Deppy Sijo.app' 'target/bundle-0.5.2/Deppy Sijo.zip'
```
