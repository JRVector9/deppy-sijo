# 팝업 리뷰 5건 수정·재검증 — 2026-10-01

## 결과

이전 [전체 팝업 리뷰](2026-09-30-full-popup-code-review.md)의 확인된5건을 모두 수정했다. 독립 재검토에서 추가로 확인한 입력 게시 시점, 명시적 경로 재연결과 FIFO 대기도 수정했다. 검토 범위에 남은 확정 결함은 없다. 앱은 실행·재실행하지 않았다.

| 기존 우선순위 | 위치 | 수정 | 검증 |
| --- | --- | --- | --- |
| high | `ui/document_dialogs.rs:21` | 고정 Modal Area와 문서 대상 ID 분리·대상 전환 focus 해제 | 같은 파일명의 다음 문서에 Discard/Reload Enter 승계 없음 |
| high | `app.rs:22450`, `popup/input.rs` | 비동기 결과·예정 모달을 전역 키보다 먼저 게시, UI 도중 상태 재게시 | 첫 Text+Enter batch PTY 유출 없음·종료 대기 전역 키 차단 |
| high | `ui/workspace.rs`, `file_tree.rs`, `shortcuts.rs` | 실제 모달/열린 메뉴/Middle 창만 배경 차단 | 검색 Enter/Shift+Enter/Esc·닫힌 메뉴 후 인라인 편집 정상 |
| medium | `folder_identity.rs`, `storage/workspace_identity.rs`, `storage/db.rs` | UUID+inode 검증과 원자적 증명 갱신 | 재마운트 허용·다른/미확인 볼륨 거절·명시적 재연결·rollback |
| medium | `popup/actions.rs:24`, `popup/shell.rs` | 버튼 전체 줄바꿈·실측 footer 높이 예약 | 실제 CJK 폰트5개 언어280×360pt, 짧은 Cancel34pt |

## 동작과 구현

- 문서 확인창은 케이스별 Area 하나를 유지한다. 이전 대상과 footer 높이, 입력 pass 번호는 고정 키에 값을 교체한다. 문서/프레임별 상태를 누적하지 않는다.
- 같은 pass에서 확인창을 닫아도 그 batch의 입력을 배경으로 넘기지 않는다. 주 터미널·부착 터미널·IME·전역 키가 공용 guard를 사용한다. 일반 Foreground 검색 영역·툴팁·닫힌 메뉴는 모달로 판정하지 않는다.
- App의 상단/사이드바가 모달을 연 뒤 검색·컴포저·터미널보다 먼저 상태를 다시 게시한다. 주/부착 워크스페이스의 살아 있는 세션 종료 대기도 논리 단계에서 차단하며, 확인 UI는 입력 소유권 활성 여부와 별도로 표시한다. 헤더에서 종료/런처를 요청한 pass의 raw 입력도 차단한다. 자격증명 전용 Modal에도 등록을 적용했다.
- 저장소 증명은 workspace당1행이고 저장된 path/dev/inode에 결속된다. 같은 UUID+inode일 때만 장치 번호 변경을 허용한다. 증명이 없는 기존 항목의 장치 변경은 자동 연결하지 않으며 설정의 명시적 프로젝트 경로 재연결을 안내한다. 명시적 재연결은 경로와 숫자 anchor가 같아도 이전 UUID를 트랜잭션 안에서 교체/삭제한다.
- native probe는 settings worker에서만 디렉터리 descriptor를 열어 metadata와 fgetattrlist UUID를 읽는다. O_DIRECTORY|O_NONBLOCK으로 FIFO/device를 open 단계에서 거절하여 worker와 shutdown join 대기를 막는다. 반환 전 경로와 descriptor의 anchor도 비교하며 File은 RAII로 닫힌다.
- 버튼은 전체 행 폭으로 측정한 galley를 부모 wrapping 레이아웃에 직접 배치한다. 남은 폭에 짧은 버튼을 세로로 늘리지 않는다. 버튼보다 긴 단일 문구만 줄바꿈한다. footer 높이/Modal 크기 변화 때만 다시 그린다.
- 공용 [디자인 문서](../design/popup-components.md)와 번호01/26/27 및 footer HTML을 업데이트했다.

## 실제 실행한 최종 검증

| 명령/범위 | 결과 |
| --- | --- |
| App 전체 `cargo test --offline --locked -q -p deppy-sijo --bin deppy-sijo -- --test-threads=1` | 2500 passed /0 failed /28 ignored,46.05s |
| Storage 전체 `cargo test --offline --locked -q -p storage --lib -- --test-threads=1` | 381 passed /0 failed,10.45s |
| Connector UI /i18n | 17 /8 passed,0 failed |
| App `popup_review` | 11 passed /0 failed,0.38s |
| Storage `popup_review` | 6 passed /0 failed |
| 실제 문서 팝업 PNG ignored renderer | 1 passed,2.77s; 정상26–29와 좁은 영어27 이미지 생성 |
| App/Storage/Connector 전체 target 엄격 Clippy | exit0, -D warnings |
| fmt /diff /xtask check-boundary /HTML Node 문법 | 모두 exit0 |
| release App +MCP proxy 빌드 | exit0,27.53s |
| root +27 workspace metadata/lock | 모두0.4.11 |
| 컴파일된 reported-version marker·bundle 두 plist 값 | 모두0.4.11, 앱 실행 없이 확인 |
| 별도 Developer ID 서명 app/ZIP 로컬 패키지 검증 | exit0, 압축 해제·서명·바이너리 동일성 포함 |

정상26과 좁은 영어27 PNG를 직접 시각 확인했다. 좁은 화면에서 주 동작과 취소가 완전히 보이고 본문은 스크롤한다. 자동 테스트 합계는 중복 실행과 별도 PNG renderer를 제외해2906건이다.

## 독립 리뷰·실패 접근

- `codex-reviewer` 흐름으로 보안·성능·품질·논리 리뷰와 독립 `codex review --uncommitted`를 수행했다. 제공된 환경에 Sonnet이 없어 지원되는 Codex 리뷰어를 사용했다. 제한된 동시 슬롯 때문에 네 번째 검토는 슬롯이 비워진 뒤 실행했다.
- 논리 리뷰에서 동일 경로 UUID 갱신과 UI 도중/대기 종료의 입력 차단 누락을 확인·수정했다. CLI가 재현한 FIFO hang은 실제 회귀 RED→GREEN으로 수정했다. 마지막 보안 후속 검토와 논리 검토에 확정 미해결 결함은 없다.
- 최초 App 회귀6건·볼륨 device1건은 실제 assertion RED를 관찰했다. footer의 첫 wrapping 접근은 부모 전체 높이를 상속해 과도하게 늘어났고, 실제 CJK 검사에서는 짧은 Cancel이92pt가 되어 추가 RED를 관찰했다. 고정 초기 row34pt와 직접 widget 배치로 수정했다.
- 첫 전체 App은2495 passed /3 failed /28 ignored였다. 소스 문자열 게이트가 새 모달 조건을 기대하지 않았고, 두 raw Modal 시험 fixture가 새 production 등록 경로를 사용하지 않았다. 실제 공용 Modal fixture로 교체하고 기대 조건을 갱신한 뒤 전체 suite를 통과했다.
- setter 위임 후 Storage source-law gate가 wrapper에서 transaction을 찾으며1건 실패했다. 실제 with_volume delegate의 admission-before-mutation 확인을 유지하고 wrapper 위임도 확인하도록 수정했다. 이후381건 통과했다.
- 테스트 fixture의 import/생성자/mux 인자 컴파일 오류, fgetattrlist의 void pointer cast 및 test Some 인자 편집 오류는 수정 후 재실행했다. 초기 Clippy boolean/중첩 if도 수정했다. 이 실패들은 통과 수에 포함하지 않았다.
- `cargo metadata --no-deps`는 lock 버전을 갱신하지 않아 첫 version assertion/locked build가 실패했다. 기존 dependency resolution을 유지하고27 workspace package 버전만 갱신한 뒤 metadata/lock 확인과 locked release build를 통과했다.

주요 로그: `/tmp/deppy-popup-app-final-gate-20261001.log`, `/tmp/deppy-popup-storage-full-final2-20261001.log`, `/tmp/deppy-popup-final-focused5-20261001.log`, `/tmp/deppy-popup-fixes-codex-review-20261001.log`, `/tmp/deppy-popup-package-verify-20261001.log`.

## 릴리스·한계

0.4.10→**0.4.11 patch**. 수정된 제품 동작과 이전 미커밋 작업을 포함한 별도 로컬 빌드다. 기존 worktree/실행 앱은 바꾸지 않았다.

- App: `target/bundle-0.4.11/Deppy Sijo.app`
- ZIP: `target/bundle-0.4.11/Deppy Sijo.zip`
- 소스 기준 commit: `166f8daeb1054cf09a07194fa83bf0a4a19d93ce` +미커밋 변경
- product diff/new source SHA256: `fe425e226999e978813dcd8647525adb7e67fbd98ae13ffafde459822f4e2300`. Cargo.toml/lock와 App/i18n/Storage/Connector의 binary diff 뒤에 정렬된 미추적 파일 경로·NUL·내용을 붙여 계산했다.
- 실행 중 앱: PID45213, PPID1,0.4.5 bundle의 기존 프로세스 유지. 재실행·커밋·푸시 없음.

하네스·임시 DB/폴더와 native UUID 조회를 검증했다. 실제 외장 볼륨 재마운트, live GUI/Finder 선택, 사용자 파일 저장/삭제·프로세스 종료와 실행 앱의 RSS 실측은 수행하지 않았다. 패키지는 로컬 개발용 서명 검증이며 공증/Gatekeeper 통과를 주장하지 않는다.
