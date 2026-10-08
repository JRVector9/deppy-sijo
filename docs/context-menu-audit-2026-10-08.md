# 우클릭 메뉴 시안·소스·실제 렌더링 대조

2026-10-08. Worktree: `/Users/jr/Desktop/projects/deppy-sijo-performance`, HEAD `5d68285e` + 미커밋 변경.

[비교 HTML](mockups/context-menus-compare-2026-10-08.html)은 최초 시안의 기록된 규격과 지난 수정 HTML을 선택해 실제 코드 렌더링과 나란히 보여준다. Chrome의 별도 탭에서 열고 9개 위치, 하위 메뉴 및 비활성 세션을 포함한 17개 화면 전환을 확인했다.

## 결과

요청한 상단 제목 제거, 가로 폭 축소, 여백 축소는 소스와 실제 렌더링에서 확인했다. 독립적으로 기록한 시안의 항목·순서·하위 메뉴 화살표·구분선을 실제 접근성 버튼 데이터와 대조한 결과 17/17 화면이 일치했다.

| 위치 | 실제 가로 × 세로(pt) | 행 수 | 최초 284px 대비 폭 축소 | 수정 256px 대비 폭 축소 |
| --- | --- | --- | --- | --- |
| 터미널·탭바 | 202 × 214 | 9 | 28.9% | 21.1% |
| 세션 목록 | 202 × 130 | 5 | 28.9% | 21.1% |
| 워크스페이스 | 202 × 79 | 3 | 28.9% | 21.1% |
| 파일 | 202 × 151 | 6 | 28.9% | 21.1% |
| 폴더 | 202 × 130 | 5 | 28.9% | 21.1% |
| 작업 카드 | 202 × 58 | 2 | 28.9% | 21.1% |
| 메모 | 202 × 121 | 5 | 28.9% | 21.1% |
| 환경·API 폴더 | 220.281 × 79 | 3 | 22.4% | 14.0% |
| 파일 목록 헤더 | 202 × 28 | 1 | 28.9% | 21.1% |

실제 한 행은 18pt로 최초 32px보다 43.8%, 수정 28px보다 35.7% 작다. 바깥 여백은 5→4pt, 행 사이 간격은 공통 `ROW_GAP=3.0`으로 1→3pt(+2px) 늘렸다. 각 행 자체의 높이는 그대로이며 행 간격만 모든 메뉴에 공통 적용한다. 공통 최소 **내용** 너비 192pt에 여백 8pt와 테두리 2pt가 더해져 대부분 **외곽** 너비는 202pt가 된다. 환경·API의 긴 항목은 줄바꿈 없이 필요한 너비로 늘어나 220.28125pt이며, 두 시안보다 여전히 작다.

## 발견한 결함과 수정

파일 목록 헤더는 드래그 영역과 기본 탭 버튼의 입력 소유권을 보존하기 위해 `Sense::hover` 응답을 사용한다. egui의 기본 `Popup::context_menu`는 이 응답에서 `secondary_clicked()`를 받을 수 없어 ‘새 폴더’ 메뉴가 열리지 않았다. 실제 헤더 우클릭 렌더링에서 두 번 재현했다.

`crates/app/src/ui/context_menu.rs::show`에서 클릭을 감지하지 않는 응답 위의 보조 버튼 클릭만 공개 `Popup::open_memory` API로 전달하도록 수정했다. 클릭 레이어나 기본 클릭 감지를 추가하지 않았다. 회귀 테스트 `context_menu_header_right_click_keeps_primary_tabs`는 실제 헤더의 빈 영역을 우클릭해 ‘새 폴더’를 확인한 뒤 Escape로 닫고 ‘메모’ 탭의 기본 클릭도 확인한다.

## 남아 있는 외형 차이

메뉴 구성과 축소는 적용됐지만, HTML의 모든 시각 토큰을 앱에 그대로 적용한 상태는 아니다. 앱의 기존 공통 테마를 상속한다.

| 속성 | HTML 시안 | 실제 구현 |
| --- | --- | --- |
| 배경 | `#21252d` | `#181b20` |
| 테두리 | `#414956` | `#32363e` |
| 모서리 | 5px | 1pt |
| 글자 | 12px | 기본 Button 13pt |
| 아이콘 | SVG 15px | egui 벡터 14pt |

이 차이는 비교 HTML에 숨기지 않고 표시했다. 이번 대조에서는 전역 테마나 사용자 글꼴 설정을 바꾸지 않았다.

## 측정 방법과 범위

- 실제 `pane_context_menu`, 세션 메뉴 action renderer, FileTree 패널, Fleet 카드, Notes TextEdit, 환경·API 프로젝트 헤더 콜백을 egui_kittest에서 우클릭/hover로 열었다. 앱 프로세스나 실제 에이전트는 시작하지 않았다.
- 실제 앱 시작 경로와 동일하게 다크 팔레트와 CJK fallback을 설치했다. 한국어, 기본 UI 글꼴 13pt를 사용했다. 각 Popup Area 경계와 그 안에 온전히 들어간 버튼 경계를 기록했다.
- PNG는 실제 egui 렌더링이며 HTML에서 다시 그린 이미지가 아니다. 원본 PNG를 Popup Area만큼 CSS로 표시하고 `pixels_per_point`를 나누어 논리 1배로 맞췄다. PNG 자체를 편집하지 않았다. 합성 마우스 커서는 렌더링 전에 harness의 `PointerGone`으로 제거했다.
- 터미널 fixture는 텍스트 선택 있음/에이전트 없음, 활성 세션은 cwd 있음/이어가기 없음/워크트리 아님, 작업 카드는 유효 PTY 대상/예약 있음, Notes는 선택/내용 있음, env는 경로 있음이다. 해당 상태에서만 구성을 대조했다. 다른 상태의 동작은 기존 전체 앱 테스트와 소스 검토 범위이며 모든 분기를 별도 PNG로 검증한 것은 아니다.
- 최초 HTML 원본은 이전 수정 때 덮어써졌다. 최초 프리뷰는 기록된 284px/32px/여백 5px로 복원했고 제목 높이는 정확히 복구할 수 없다. 따라서 최초 메뉴의 총 높이 감소율을 주장하지 않는다. 지난 수정 프리뷰는 기존 HTML의 256px/28px/여백 4px CSS를 사용한다.
- 조사 당시 실행 중인 PID71458은0.8.7 번들이며 이번 변경을 로드하지 않았다. 제품 빌드 배포나 앱 재실행은 하지 않았다. 임의 글꼴·테마·배율 및 실제 실행 중 앱의 화면 일치까지 확인한 결과는 아니다.

## 실행한 검증

후속 행간 +2px 변경: `/private/tmp/deppy-context-menu-row-gap-20261008.log`, cargo gate exit0. 기존 메뉴 집중 회귀 3개 PASS, 실제 렌더링 진단 5개 PASS(17개 화면 재측정), fmt PASS. 비교 HTML과 위 크기 표는 증가한 행간 기준으로 갱신했다. 아래 전체 검사 결과는 행간 변경 전의 감사 단계 결과이며 이번에 재실행한 것은 아니다.

`/private/tmp/deppy-menu-audit-final-20261008.log`, cargo gate exit0:

- 실제 렌더링 진단 5개 PASS: 17개 PNG/JSON 생성.
- App 단위 2,788개 PASS / 36개 ignored. 헤더 우클릭 회귀 포함.
- 통합 4+5+15+15개 PASS. 플랫폼/리소스 관련 통합 3개 ignored.
- i18n 8개 PASS.
- App+i18n 모든 target의 Clippy `-D warnings`, fmt, UI boundary PASS.

이후 진단의 부모/자식 버튼 추출 필터만 보완하고 `/private/tmp/deppy-menu-audit-evidence-20261008.log`에서 렌더링 5개 및 엄격 App all-target Clippy/fmt를 다시 실행해 exit0으로 확인했다. 부모 메뉴의 휴지통 행 중심이 자식 영역 뒤에 겹쳐 진단 JSON에 잘못 포함됐던 측정 문제를, 전체 버튼 경계의 포함 여부로 바로잡았다. 제품 메뉴에 추가 행이 있던 문제는 아니다.

비교 HTML의 독립 시안 데이터 대조 스크립트는 17/17 항목·순서·화살표·구분선 일치, `node --check` PASS. CUA로 Chrome에서 9개 루트와 8개 추가 상태, 최초/수정 시안 전환을 확인했다. 최종 화면에서 최초 메뉴의 마지막 ‘세션 닫기’까지 잘리지 않고 보이는지 확인했다. `git diff --check`도 exit0으로 확인했다. 내장 증거와 17개 JSON/PNG 크기, 10개 소스 SHA-256 일치 및 재생성 Python 스크립트 문법도 확인했다.

초기 진단 실패는 handoff에 기록했다: 잘못된 Context/TreeNode API, 처음 잘못된 헤더 좌표, hover 응답으로 인한 실제 메뉴 미개방, private Popup 메모리 API, 앱 팔레트 대신 기본 egui 팔레트를 설치한 fixture. 최종 증거는 모두 수정된 앱 팔레트의 렌더링으로 덮어썼다.

## 재생성

먼저 현재 환경의 cargo gate로 진단을 실행한다. 아래 명령은 앱을 실행하지 않는다.

```sh
RUST_TEST_THREADS=1 python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py test --offline --locked -q -p deppy-sijo context_menu_audit -- --ignored --nocapture
python3 scripts/refresh-context-menu-comparison.py --test-summary '실제로 실행한 검사 결과를 여기에 기록'
```

생성 JSON/PNG는 `docs/mockups/context-menu-audit-assets/`, HTML은 `docs/mockups/context-menus-compare-2026-10-08.html`이다. HTML에는 대응 소스 SHA-256과 HEAD도 포함한다. 재생성 도구 자체는 테스트를 실행하거나 통과를 주장하지 않는다.
