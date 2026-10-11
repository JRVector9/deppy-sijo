# 이력 화면 승인 시안 대조 · 2026-10-10

## 목표와 원인

승인 원본은 `docs/mockups/work-history-proposal-2026-10-08.html`이다. 기존 browser는 행 높이만 68pt로 고정하고 본문·메타정보·버튼에 앱 기본 크기를 적용했다. 작은 글자와 행의 빈 공간, 기본 사각 선택 버튼, 다른 상단/상세 여백 때문에 원본과 차이가 컸다.

canonical source `/Users/jr/Desktop/projects/deppy-sijo-performance`, 시작 HEAD `799fae89`, app version `0.9.0`. 이번은 소스 수정/테스트이며 앱을 재시작하거나 릴리스 artifact를 전달하지 않았다.

## 변경

- history 안에만 Body/Button14pt, Small12pt, heading21/detail20pt와 시안의 색/5pt 모서리/버튼 padding을 적용한다. 외부 pane의 폰트와 버튼 spacing이 변경되지 않는 것을 각 harness frame에서 검사한다.
- 일반 버튼·입력 37pt, 정렬·복사26pt, 상세 탭39pt. 원본 일반 버튼은 21px line + 14px padding + 2px border = 37px이다. 작은 보조 컨트롤은 26pt로 통일한다.
- 목록은 title21pt line + 5pt gap + meta18pt line를 68pt 행 안에 배치하고 상하 빈 공간을 각각12pt로 맞춘다. 제목과 메타정보는 공통 좌측에 정렬하고 시간은 우측에 분리한다. 선택 배경·좌측3pt stripe·구분선을 적용한다.
- header24/list20/detail26pt 가로 여백, 기본1:1.1 split. 명시적으로 저장한 사용자 분할 폭은 존중한다. 좁은 화면에는 목록↔상세 전환과 toolbar wrap을 유지한다.
- 상세는 상태 badge, 지시/결과와 원문 tabs의 underline2pt, section heading 오른쪽 copy26pt, 결과 좌측선, 기본37pt 세션/Git action을 사용한다. action ownership, transcript once-per-identity, bounded clipboard와 copy feedback을 유지한다.

## 실제 검증

Cargo는 root gate(`/private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py`)로 직렬 실행했다. cwd는 canonical source이다.

```sh
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py test -q -p deppy-sijo ui::work_history:: -- --test-threads=1
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py test -q -p deppy-sijo approved_history_render_evidence -- --ignored --test-threads=1
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py clippy -q -p deppy-sijo --all-targets -- -D warnings
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py fmt --all -- --check
git diff --check
```

- 최종 work_history: **62PASS / 0FAIL / 1ignored**, `/private/tmp/deppy-history-parity-tests-final-20261010.log`. 포함: 실제 control bounds, row의 왼쪽 정렬/21+18pt typography/아래12pt 여백,320/390/760/900/1100pt containment, 선택·이동·Git·검색·scope·cards·transcript·long-summary fixed bounds 및 기존 copy/card 회귀.
- 최종 opt-in offscreen render: **1PASS / 0FAIL**, `/private/tmp/deppy-history-parity-render-final-20261010.log`. 실제 제품 렌더러/production CJK font와 격리 예시 데이터,1100×720 및390×720. 앱 프로세스를 시작하지 않는다.
- `work-history-parity-2026-10-10-assets/measurements.json`에는 실제 AX rect와1pixel/point를 기록했다. ordinary37, compact26, tabs39를 실제 bounds로 확인했다.
- 비교 HTML: inline Node syntax, 중복ID 없음, tab 연결, 모든 로컬 asset 존재, 실제 JSON 높이 검증 PASS. native Chrome 전용 새 창에서 loopback8767 페이지를 열고 실제 AX의 이미지/크기 표/대조 탭을 확인했다. 이어서 tab interaction은 native CUA의 stale element / cgWindowNotFound로 미검증이다.
- strict App all-targets Clippy **PASS(exit0)**, `/private/tmp/deppy-history-parity-clippy-20261010.log`. 최종 `cargo fmt --all -- --check` **PASS(exit0)**, `git diff --check` **PASS(exit0)**.

## 실패한 접근

- 최초 `Ui::place`는 Label::halign(Min)에도 row labels를 가운데 배치했다. 실제 이미지에서 발견해 명시적 top-down child layout으로 변경하고 좌표 회귀로 고정했다.
- 최초320pt containment는 provider 뒤의 Frame filtergroup을 자동 줄바꿈할 것으로 가정해 실패했다. 실제 group 폭을 측정하고 wrapping layout의 다음 행으로 옮겨 수정했다.
- 추가 containment oracle의760pt는 parent8pt margin을 제외한 실제744pt browser 폭을 고려하지 않았고, desktop의 filter/detail 둘 다 ‘완료’ label이 있는 점을 고려하지 않았다. actual detail mode와 복수 label bounds를 사용하도록 oracle를 수정했다. 이 실패를 product 기능 오류로 취급하지 않는다.

## 증거와 범위

비교 `docs/mockups/work-history-parity-2026-10-10.html`, 원본 unchanged. before/after desktop은 같은2rows/blocked1 fixture이며 normal은5rows/blocked0이다. 예시 session label/응답은 사용자 DB와 무관하다. PNG는 재작성된 HTML 그림이 아니라 실제 egui 렌더링이다. 실행 중인 앱은 이전 빌드이므로 이번 변경은 아직 그 프로세스에 반영되지 않았다. 실제 사용자 이력/맥의 클릭 전체 흐름과 설치형PWA는 이 격리 렌더링과 구분한다.
