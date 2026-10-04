# 환경 설정·안내·포트 공용 팝업 적용 — 2026-10-03

## 완료한 표시 변경

| 번호 | 화면 | 공용 구성 | 동작 |
| --- | --- | --- | --- |
| 09 | 환경 및 API 프로젝트 목록 닫기 | 400pt 셸·Info notice·Primary 닫기·Ghost 취소 | 원래 프로젝트 대상으로 목록만 닫고 실행 세션 유지 |
| 15 | 환경변수 정의 삭제 | 480pt 셸·경고·36pt 원본 선택·34pt 삭제/취소 | 원본 선택 보존, 명시적 삭제, 기본 Enter로 삭제하지 않음 |
| 33 | 런타임 이벤트 적체 | 공용 `information` | 기존 동기화 설명·닫기 |
| 34 | 실행 중 워크스페이스 한도 | 공용 `information` | 기존 대상·한도 안내·닫기 |
| 35 | 다른 워크스페이스 셀 열기 실패 | 공용 `information` | 기존 실패 이유·닫기 |
| 37 포트 | 상태바 포트 목록 | 공용 앵커 팝오버·목록 행·본문 스크롤·고정 푸터 | 갱신·소켓 복사·소유한 프로세스 종료 확인 유지 |

37은 상태바에 붙는 팝오버로 유지했다. 목록의 종료 버튼은 기존12번 확인창을 사용하고 새 종료 경로를 만들지 않았다. 외부·보호·소유권 불명 프로세스는 읽기 전용이고 IPv6 주소에도 임의로 HTTP 스킴을 붙이지 않는다. 같은37번에 묶인 승인·리소스 팝오버는 이번 지정 범위인 포트 목록과 구분했다.

## 컴포넌트와 호출자

- `ui/environment_dialogs.rs`:09·15의 지역화된 표시와 선택 결과. App/EnvProfilesUi가 대상·초안·기존 명령을 소유한다.
- `ui/popup/information.rs`:420pt 정보 안내와 닫기. 세 안내는 같은 pass에 하나만 표시하고 대기 상태를 App의 배경 키 fence에 포함한다.
- `ui/popup/shell.rs`:모달/팝오버가 같은 머리글·스타일·표면을 사용한다. 팝오버는 모달 fence를 만들지 않고 egui 메뉴의 입력 소유권을 유지한다. 본문 높이는 viewport와 선택 상한을 함께 적용한다.
- `ui/popup/list.rs`:목록 표면과34pt 높이의 줄바꿈 동작 행. 스크롤 영역의 남은 높이를 행 버튼의 높이로 사용하지 않는다.
- `ui/popup/fields.rs`:36pt `choice_input`. 열린 하위 선택 메뉴가 먼저 Esc를 처리하고 그 키를 소비하여 부모 확인창에 전달하지 않는다.
- `prepare_target`:최신 대상과 두 개 재사용 동작 scope만 보존한다. 새 환경 확인창은 반환된 scope를 사용하고 대상 변경 시 이전 선택 버튼의 포커스를 해제한다.
- `app.rs`,`ui/env_profiles.rs`,`ui/ports.rs`,`ui/agent_terminal.rs`:표시 연결과 원래 작업 접수. 새 디스크/네트워크 작업이나 백그라운드 폴링을 추가하지 않았다.
- `env.delete_source`:다섯 언어의 레이블 추가. 다른 표시 문구는 기존 번역을 사용한다.

## 실제 검증 결과

| 검증 | 실제 결과 | 기록 |
| --- | --- | --- |
| 변경 전 실제 렌더러 RED | 4실패/0통과 · 0.11s | `/tmp/deppy-env-ports-popup-red-20261003.log` |
| 리뷰 지적의 하위 선택 메뉴 Esc RED | 1실패/0통과 · 0.04s | `/tmp/deppy-env-ports-popup-nested-escape-red-20261003.log` |
| 최종 집중 테스트 | 9통과/0실패/2ignored · 0.26s | `/tmp/deppy-env-ports-popup-green-release-final-20261003.log` |
| 최종 App 전체 · 8threads | 2529통과/0실패/30ignored · 9.35s | `/tmp/deppy-env-ports-popup-app-tests-release-final-20261003.log` |
| i18n | 8통과/0실패 · 0.02s | `/tmp/deppy-env-ports-popup-i18n-tests-20261003.log` |
| App all-target strict Clippy | exit0 · 8.46s | `/tmp/deppy-env-ports-popup-clippy-release-final-20261003.log` |
| 오프스크린 실제 UI 렌더링 | 2테스트 통과 · 6개 PNG 생성·모두 육안 확인 | `/tmp/deppy-env-ports-popup-render-20261003.log` |
| HTML JS 문법·fmt·diff | 실행하여 통과 | 최종 작업 도구 결과 |
| 최종 App/proxy release build | exit0 · 21.46s | `/tmp/deppy-env-ports-popup-release-build-final-20261003.log` |

실행 명령:

```sh
cargo test --offline --locked -q -p deppy-sijo --bin deppy-sijo env_ports_popup -- --test-threads=1
cargo test --offline --locked -q -p deppy-sijo --bin deppy-sijo -- --test-threads=8
cargo test --offline --locked -q -p i18n
cargo test --offline --locked -q -p deppy-sijo --bin deppy-sijo env_ports_popup_render -- --ignored --test-threads=1
cargo clippy --offline --locked -p deppy-sijo --all-targets -- -D warnings
cargo fmt --all -- --check
git diff --check
cargo build --offline --locked --release -p deppy-sijo -p mcp-proxy
```

검증한 동작은 실제 egui 렌더러의 버튼/선택 입력 크기, 좁은 viewport 안 배치, 취소·기본Enter·명시적 삭제, 원래 변수/선택 파일을 담은 `DotenvWrite`, 새 대상의 포커스 초기화, 하위 선택 메뉴의 Esc 소유권, 원래 소켓 복사 의도, 팝오버 새로고침/닫기와 비모달 상태다. 기존 전체 테스트도 재열기 시 갱신·종료 대상·확인창 유지·외부 프로세스 보호 경로를 검사했다.

실제 사용자 파일 삭제·프로세스 종료·클립보드 쓰기·네이티브 앱 실행을 테스트에서 수행하지 않았다. 오프스크린 이미지는 실제 egui 컴포넌트를 렌더링한 fixture이며, 실행 중인 Deppy에 대한 GUI E2E 검증이라고 주장하지 않는다.

## 소스 코드 리뷰와 수정

1. 실제 작업 시작 스냅샷과의 Rust diff1548줄을 독립 Codex CLI로 리뷰했다. 문서·계획서는 리뷰 입력에서 제외했고 리뷰어는 읽기만 수행했다.
2. Medium1건: 원본 선택 메뉴 Esc가 부모 삭제 확인창도 취소하는 문제. 실제 기존 UI 테스트로1 RED를 확인했고 잠긴 egui0.36.1의 Popup/ComboBox/Modal 소스를 읽었다.
3. 선택 메뉴가 렌더링된 pass에만 egui의 메뉴 닫기가 끝난 뒤 Esc를 소비하도록 `choice_input`을 수정했다. 첫 Esc는 선택 메뉴만 닫고 `.env.local`과 확인창을 보존하며, 다음 Esc는 부모를 취소한다. 전역 `take_modal_escape`는 바꾸지 않아 뒤에 원래 포트 목록이 있어도12번 확인을 취소하는 동작을 유지한다.
4. 최종 독립 재리뷰는 **남은 actionable introduced defect 없음**을 보고했다. `/tmp/deppy-env-ports-popup-cli-final-result-20261003.txt`. 리뷰어가 테스트를 실행했다는 주장은 하지 않는다.

중간 실패도 기록했다. ComboBox ID salt에 Debug 제약이 필요해 첫 GREEN 빌드가 실패했고 제약을 바로잡았다. 접근성의 선택 값은 label이 아니어서 ComboBox role로 찾도록 fixture를 수정했다. 목록의 직접 RTL 레이아웃이 스크롤 여백을 행 높이로 사용해 복사 버튼을 멀리 배치했고, 공용 `list_actions`의 명시적34pt 행으로 고쳤다. 주소 복사 검증은 실제 스크롤로 대상 버튼을 보이게 한 뒤 클릭한다. 최종 테스트 결과는 위 표이며 이전 중간 통과 결과로 대체하지 않는다.

## 시안과 이미지

[갱신한42개 시안](../mockups/shared-popup-components-2026-09-30.html)은 요청대로 OS 기본 브라우저에 열었다. `#terminal_status`로 상세 포트 목록을 바로 보여주며 09·15·33·34·35도 왼쪽 번호로 선택할 수 있다. 포트 행의 종료는 선택한 소켓과 프로젝트를 넣은12번을 보여주고 취소하면 목록으로 돌아간다. CUA의 iab/chrome 표면은 사용할 수 없어 OS `open`으로 열었고, 브라우저 DOM 자동화/스크린샷 검증은 수행했다고 주장하지 않는다.

실제 UI 이미지: `target/popup-parity/20261003/{09,15,33,34,35}.png`, `37-ports.png`.

## 버전과 산출물

- **0.5.4→0.5.5 patch**. 존재하는 worktree 소스/번들의 최대 버전0.5.4를 확인한 뒤 올렸다.27개 workspace package의 metadata와 Cargo.lock 버전은0.5.5로 일치한다.
- 소스 기준 commit:`166f8daeb1054cf09a07194fa83bf0a4a19d93ce` + 누적 미커밋 코드. 최종 제품 diff SHA256:`891a6a2312db9c90172fbce2bd3e664c140b48157f0a156043de9f6fd84ca847`.
- 최종 소스/버전/산출물 기록:`/tmp/deppy-env-ports-popup-release-source-20261003.json`.
- 별도 번들/ZIP:`target/bundle-0.5.5/Deppy Sijo.app`, `target/bundle-0.5.5/Deppy Sijo.zip`.
- 컴파일된 `Deppy0.5.5` 버전 문구와 `CFBundleShortVersionString`·`CFBundleVersion` 모두0.5.5로 일치한다. 별도 Developer ID 서명 번들/ZIP의 strict signature·architecture·압축 해제 후 바이너리 일치 검증을 실제로 통과했다. `/tmp/deppy-env-ports-popup-package-verify-20261003.log`. 명시적 로컬 개발 검증이며 Apple 공증을 받았다는 주장은 하지 않는다.
- 빌드 후 제품 소스 hash가 위 최종 값과 같고 이전0.5.4 앱 바이너리/ZIP hash가 변경되지 않았음을 확인했다. 기존 번들을 덮어쓰지 않았다.

Deppy 재실행·종료·커밋·푸시는 하지 않았다. HTML 시안 브라우저 표시만 현재 사용자가 명시적으로 요청한 실행이다.

## 후속 적용 현황

일반 공용 모달 적용은13개에서18개로 늘었고37번의 포트 목록도 공용 팝오버가 됐다. 아직 일반 팝업으로 남은12개는 **05·06·14·16·18·19·20·21·22·23·24·25**다. 이번에 요청되지 않은 팝업·전용 도구창·메뉴·런처·OS 창은 이 작업의 개발 범위에 추가하지 않았다.
