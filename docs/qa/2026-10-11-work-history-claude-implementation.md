# Claude 구조를 적용한 작업 이력 — 2026-10-11

## 요청과 구현 기준

사용자가 지정한 [Claude 이력 시안](https://claude.ai/artifact/1NHCPjiQ2nHnQgWDv5tvEX#page-dcb81aee8d83)의 정보 구조를 적용한다. 앞선 비교 문서 `2026-10-11-work-history-claude-comparison.md`에 실제 native Chrome AX/스크린샷 확인 범위를 기록했다. 60% 캔버스 이미지로 원본 CSS 치수를 단정하지 않는다. 기존 승인한 글자·버튼·행 크기는 유지한다.

소스는 `/Users/jr/Desktop/projects/deppy-sijo-performance`, base HEAD `799fae89`, version `0.9.0`. nominal checkout, API 보고서와 이전 시안 자료는 보존했다. 이번 요청은 소스 변경이다. 앱 중지·실행, 변경 artifact 출시, 커밋·푸시를 수행하지 않았다. 추가 스킬·서브에이전트를 사용하지 않았다.

## 적용 내용

- 기본 스레드 묶음, 시간순 선택, 기존 카드 보조 보기. 검색·상태별 개수·프로젝트 필터와 펼침형 추가 필터(세션·기간·공급자·정렬).
- 스레드별 대표 제목, 프로젝트/브랜치, 에이전트·작업 수·시간 범위. 68pt 개별 행은 지시/저장 결과 한 줄/시간/검증된 상태를 표시한다.
- 상세 상단 프로젝트·경로·브랜치·세션·모델, 과거 선택 시점의 대표 제목, 이번 지시와 앞선 지시3개, 저장 결과/원문 대화. 넓은 상세에서는 지시와 앞선 흐름을 나란히, 좁으면 위아래로 표시한다.
- 이동·Git·지시/요약 복사를 상세 스크롤 밖 하단에 고정한다. 좁은 상세는 버튼 두 줄이다. 기본 목록32%/상세68%, 사용자 저장 split 폭 우선.
- `display_pane_title`을 사용해 자동 생성한 `workspace.spawn.agent 11` 같은 이름을 사람이 읽는 세션 이름으로 표시한다. 저장 데이터/identity는 바꾸지 않는다.
- 일반 입력·버튼37pt, 복사·정렬·문맥 지시26pt, 상세 탭39pt, 본문14pt/메타12pt. 스타일은 이력 UI scope에 한정하며 다른 화면의 글자·크기를 바꾸지 않는다.

## 데이터·동작 경계

`browser/thread.rs`는 workspace/pane/provider/native agent_session_id가 모두 같은 기록만 묶는다. 대표 제목은 anchor 이전의 긴 지시, 앞선 흐름은 strict earlier 최대3개다. 이후 턴과 다른 세션의 내용을 과거 상세에 사용하지 않는다.

짧은 지시의 sparse metadata는 같은 owner의 과거 저장 기록에서 보완한다. path 변경 시 branch, model 변경 시 effort를 초기화해 서로 다른 프로젝트·모델의 metadata를 상속하지 않는다. turn identity, live proof, 원문, summary, git_count는 수정하지 않는다. 표시하는 branch/model은 해당 시점의 마지막 저장 값이며 live 조회로 만들지 않는다.

결과 한 줄은 저장 응답의 첫 일반 문장에서 최대160Unicode 문자로 추출한다. heading과 fenced code, 링크의 URL/일부 inline markdown은 제외한다. 한국어 번역·새 요약을 생성하지 않으며 PR·파일·검사 수를 추정하지 않는다. 전체 원문은 상세에서 유지한다.

문맥 지시 클릭은 로컬 검색/상태/기간/프로젝트 필터를 해제하고 그 정확한 saved turn을 보여준다. 외부 보조 검색이 다른 턴을 제외하는 경우 해당 문맥 버튼을 비활성화해 다음 프레임에 다른 작업으로 되돌아가는 클릭을 막는다. 이동/Git/원문은 기존 App presentation과 exact durable identity를 사용한다.

## 실제 실행한 검사

| 검사 | 결과 | 로그 |
|---|---|---|
| 전체 `ui::work_history::` | 67PASS / 0FAIL / 1ignored | `/private/tmp/deppy-history-claude-final-20261011.log` |
| opt-in 실제 offscreen 렌더 | 1PASS / 0FAIL | 같은 final log |
| App 기본 이력 split | 1PASS / 0FAIL | `/private/tmp/deppy-history-claude-green2-20261011.log` |
| i18n locale 순서·placeholder 등 | 8PASS / 0FAIL | 같은 green2 log |
| App + i18n all-targets Clippy `-D warnings` | PASS / exit0 | final log |
| `cargo fmt --all -- --check` | PASS / exit0 | final log |
| `git diff --check` | PASS / exit0 | root 실행 |
| HTML script·IDs·탭 target·local asset·크기 JSON | PASS | root Node 실행 |

회귀는 group ownership/future 경계/metadata path 변경, 짧은 지시의 project filter, 앞선 지시 클릭→정확한 activation, thread/time 전환 선택 유지, localized title, unknown active 상태, 카드 cache scope 변경, 원문 요청1회, 검색·필터 초기화, copy feedback/identity/clip, parent style 보존을 포함한다. 긴 결과300줄 및30repaint 후 행/고정 footer bounds,320×568/320×720/390/760/900/1100pt의 프로젝트 필터와 하단 버튼 bounds도 검사했다.

## 실제 렌더와 미리보기

`approved_history_render_evidence`는 제품의 `show_browser`를 격리 fixture3threads/8turns로 호출한다. 사용자 DB·세션·PTY를 쓰거나 앱을 시작하지 않는다. Apple SD Gothic Neo,1pixel/logical point. `docs/mockups/work-history-claude-2026-10-11-assets`의 desktop1100×720,normal1440×900,timeline,narrow-list,narrow-detail PNG와 `measurements.json`을 생성했다. 추가필터의 숨겨진 컨트롤은 펼친 상태에서 측정하고 나머지는 기본 상태에서 측정했다.

`docs/mockups/work-history-claude-applied-2026-10-11.html`은 실제 PNG, 정보 구조·측정표, 이전 구현과 비교를 제공한다. 원본10월8일 HTML은 수정하지 않았다. localhost8767 HTTP200을 확인했고 native Chrome 전용 새 창에서 문서와 실제 이미지/표를 확인했다. 좁은 화면 탭 클릭→AX selected 및 두 이미지와 screenshot, 스레드 탭 복귀→AX selected를 확인했다. HTML의 시간순/크기/맞춤 버튼은 script 구문과 target 검사이며 이번 native UI 클릭 검증에 포함하지 않았다. 제품의 thread/time 전환은 Rust UI 회귀에서 별도로 실행했다.

현재 실행 중인 앱에 새 코드를 주입하거나 재실행한 live E2E는 수행하지 않았다. 제품 기능과 렌더 검증은 이번 소스의 격리된 화면 함수/회귀 범위다.

## 실패한 접근과 수정

- 최초 compile E0106: borrowed field lifetime을 명시해 수정했다.
- 최초 history60PASS/4FAIL: scope Combo를 메뉴 안에 중첩해 값 선택 때 닫힘, 메뉴 기본style 크기,320pt copy footer overflow. CloseOnClickOutside만으로 scope2건이 남았다(64PASS/2FAIL). 추가필터를 inline 펼침 구조로 바꾸고 scoped style과 narrow 두 줄 footer로 수정했다.
- inline 변경 뒤64PASS/3FAIL: narrow Combo가 수평 영역을 넘었고 project test가 같은 경로의 label과 Button을 함께 찾았다. narrow 추가필터는 세로 배치, test는 정확한 Button role을 사용해 수정했다.
- 실제390pt PNG에서 main project Combo도 child layout 때문에 `…`로 잘렸다. 남은 행 공간을 확인하고 명시적으로 다음 행에 배치한 뒤 재검사·PNG·Clippy가 모두 통과했다.
- sparse metadata가 다른 project의 branch를 상속할 수 있는 경로를 수동 리뷰에서 발견했다. path/model 변경 경계 fold와 회귀로 수정했다.

## 재현 명령

cwd `/Users/jr/Desktop/projects/deppy-sijo-performance`; Cargo는 기존 gate로 직렬 실행한다.

```sh
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py test -q -p deppy-sijo ui::work_history:: -- --test-threads=1
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py test -q -p deppy-sijo approved_history_render_evidence -- --ignored --test-threads=1
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py test -q -p deppy-sijo 이력_본문은_목록 -- --test-threads=1
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py test -q -p i18n
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py clippy -q -p deppy-sijo -p i18n --all-targets -- -D warnings
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py fmt --all -- --check
git diff --check
```
