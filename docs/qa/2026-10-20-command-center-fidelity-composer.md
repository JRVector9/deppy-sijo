# Command Center 시안 대조·입력 독립성 수정

## 요청과 범위

승인 HTML과 현재 실행 중인0.14.0을 직접 대조했다. 사용자의 지적대로 기존 구현은 헤더·좌측 목록·본문 글자 계층·버튼·상세 탭·설정·작성 영역에서 차이가 있었다. `feat/agent-command-center-v2-20261020`에서26개 영역을 목록화하고 GPT-6.1-Sol/xhigh 담당3명이 독립 worktree로 병렬 수정했다. root가 diff와 실제 렌더를 검토해 통합했다. 기존 nominal dirty worktree와 main은 수정하지 않았다.

중앙 항목별 기록: [C01–C26 체크리스트](../superpowers/plans/2026-10-20-command-center-fidelity-composer.md). 시안은 [승인 HTML](../mockups/multi-agent-command-center-v2-2026-10-20.html)의 **최종 CSS cascade와 실제 렌더**를 기준으로 삼았다. 원본 시안의 내용을 구현에 맞춰 변경하지 않았다.

## 차이가 생긴 이유와 수정

| 영역 | 수정 전 원인 | 적용 내용 |
| --- | --- | --- |
| 화면 규격 | 초기 CSS 값·egui 기본 버튼/행 크기 사용 | 최종 cascade 기준 헤더·프로젝트·본문·패널·버튼 크기와 여백 반영 |
| 글자 계층 | egui strong은 색만 바꾸고 실제 굵기를 바꾸지 않음 | 캐시된 시스템 TTC의 실제600굵기 face를 별도 family로 등록, 큰 버퍼 공유 |
| 좌측 목록 | 원시 세션과 작업 세션을 중복 표시 | exact workspace/session/task 기준 중복 제거, 독립 세션은 유지, stable widget ID |
| 상세·설정 | 작업/결과 분리, 고급 규칙 항상 노출, 폼 폭 오해 | 작업과 결과 통합4탭, 규칙 접힘·저장/취소, 최종 max960/36px field |
| 작성 제한 | 타이핑 가능 여부를 전송 capability와 연결 | 미연결/불러오기 중에도 작성, 실제 전송은 기존 host 권한 유지 |
| 초안 수명 | 바인딩 전후 다른 key, 늦은 load/ACK의 덮어쓰기 | 프로젝트 메인 초안 이동·충돌 보존, load 원자 검증, edit revision+NFC 확인 |
| 설정 선택 | 입력 불가능한 raw App Server가 세션 후보에 포함 | 실제 입력 가능한 task-free local 세션과 실제 catalog tuple만 선택 |
| 저장 충돌 | 저장 중 새 설정을 편집하면 예전 CAS revision 유지 | 자신의 저장 결과를 확인한 경우에만 revision 갱신, 새 편집 내용 보존 |
| 본문·표 열 | egui가 예약한 영역보다 작은 widget을 중앙 배치하고 작업 행이 자연 폭으로 축소 | 짧은 설명도 좌측 정렬, 상세 정보의 행 밀도와 작업 표 전체 열 폭 복원 |
| 이력 종류·행 | 저장 DTO가 실제 이벤트 종류를 누락하고 행이 자연 폭으로 축소 | 실제 종류를 기본값 호환 필드로 보존, 전체/작업/승인/설정 필터·정확한 소유자·작업 이동·전체 폭 복원 |
| 스크롤 표시 | egui 기본 fade가 라이트 화면에서 검은 띠를 그림 | Command Center scope에서 fade 제거, 모니터 경계선 복원 |

## 사용 흐름

1. 프로젝트 메인이 연결되지 않아도 입력창에 내용을 작성한다.
2. 입력 하단의 대상 문구를 누르거나 미연결 상태에서 Enter/전송하면 에이전트 선택이 열린다.
3. 실제 기존 세션을 선택하거나 에이전트·모델·추론 강도와 작업자·범위를 직접 선택해 저장한다. 새 메인 생성은 별도 명시 동작이다.
4. 연결 후에도 초안이 남는다. 사용자가 다시 Enter/전송할 때 전달한다. 연결/저장만으로 자동 전송하지 않는다.

다른 프로젝트/작업의 초안과 담당은 독립적이다. 바인딩 충돌 시 다른 초안을 전환해 확인할 수 있다. 똑같은 NFC 내용으로 확인된 초안에는 중복 전환을 만들지 않는다. 입력 승인 응답이 늦는 동안 A→B→A로 새로 편집한 경우도 새 글을 지우지 않는다. 네이티브는 저장 worker 복구와 용량 충돌도 검사했고, 저장할 수 없는 경우 로컬 초안임을 표시한다. 공용 웹 초안은 해당 renderer가 유지되는 동안 보존하며 페이지 종료 후 영구 복원을 약속하는 기능은 이번 변경에 포함하지 않았다.

## 실제 검증

최종 제품 소스는 `d58505a89cc76011118f60044810617c7bb0d236`이다. 이후 문서·증거 변경은 제품 소스와 분리한다. 이전 단계와 최종 통과 수를 합산하지 않는다.

| 실행 범위 | 실제 결과 | 증거 |
| --- | --- | --- |
| 최종 App 전체 bin | 3,505 PASS / 0 FAIL / 40 ignored | [app.log](assets/command-center-v2-exact-20261020/gates-final/app.log) |
| 최종 storage 전체 | 단위512 + 통합8 PASS / 0 FAIL | [storage.log](assets/command-center-v2-exact-20261020/gates-final/storage.log) |
| 최종 공용 브라우저 | 237 checks PASS | [shared-chrome.log](assets/command-center-v2-exact-20261020/gates-final/shared-chrome.log) |
| 최종 production Chrome / Relay Node | 1 / 17 PASS | [production](assets/command-center-v2-exact-20261020/gates-final/production-chrome.log), [Relay](assets/command-center-v2-exact-20261020/gates-final/relay-node.log) |
| 최종 fmt / diff / boundary / deps / i18n / workspace all-target Clippy | 모두 exit0 | [11개 gate 명령·source·시간](assets/command-center-v2-exact-20261020/gates-final/gates.json) |
| 실제 native focused / GPU render | 담당45 PASS, render1 PASS·52PNG | [native 검증·SHA](assets/command-center-v2-exact-20261020/after/native-validation.json) |
| 이력 추가 수정 전 전체 workspace | f65bd5af에서6,095 PASS / 0 FAIL / 67 ignored·79targets | [전체 source별 결과](assets/command-center-v2-exact-20261020/gates/workspace-summary.json) |

최종 추가 수정은 네이티브 main/tests/DTO, 이력 저장·투영, 공용 JS/CSS/fixture에 한정됐다. 따라서 최종 App·storage 전체와 브라우저·strict workspace Clippy를 다시 실행했다. 다른 workspace target의 테스트를 최종 source에서 다시 전부 실행했다고 표현하지 않는다. nested subprocess의 단독1PASS는 App 전체3,505에 다시 더하지 않았다. 실행한 정확한 argv와 source는 gate JSON에 있다.

## 대조 증거와 경계

[시안·웹·네이티브 비교 HTML](2026-10-20-command-center-fidelity-comparison.html)에 메인1440/1200/390, 라이트, 설정, 상세, 작업 목록, 기기, 기록을 나란히 배치했다. 입력창의 실제 에이전트 선택도 포함했다.

- [주요 치수 비교](assets/command-center-v2-exact-20261020/after/geometry-check.json): 실제 Chrome20case·10쌍, 포커스 전후13개 속성에서0차이(허용0.02CSSpx). 전체 화면의 모든 픽셀이 같다는 뜻은 아니다.
- [이력 치수 비교](assets/command-center-v2-exact-20261020/after/history-geometry-check.json): 1440/390·양 테마4쌍의 intro·분류·행·담당·버튼 규격 대조. 시안 전용 예시 안내 문구는 제품에 옮기지 않았다.
- [소스 일치](assets/command-center-v2-exact-20261020/after/source-match.json): 네이티브11파일 + 공용3파일을 통합 source에서 SHA 재검증했다. 승인 원본6개 hash도 그대로다.
- [캡처·snapshot 출처](assets/command-center-v2-exact-20261020/after/evidence-manifest.json)와 [fixture 보정 내역](assets/command-center-v2-exact-20261020/after/shared-auxiliary-measurements.json)을 보존했다. 빠져 있던 exact-main Session.input은 제공된 Project.main_input으로만 채웠고, 종류는 승인 fixture eventID로만 명시했다. 실제 제품이나 사용자 snapshot을 수정하지 않았다.
- 비교 HTML 자체를 격리 Chrome1440/390에서 열었다. [34개 이미지·26개 체크 항목·가로 넘침 검증](assets/command-center-v2-exact-20261020/after/comparison-report-validation.json)이 통과했다.

실제 Task DTO에 없는 의존 관계 목록·제출 파일별 요약·구조화된 리뷰 댓글은 시안 예시에서 가져와 만들어 넣지 않았다. 사용할 수 있는 실제 결과·검증·권한 정보만 표시한다. 이 데이터 경계는 [native fixture 메타데이터](assets/command-center-v2-exact-20261020/after/native-fixture-metadata.json)에 명시했다.

현재 실행 앱은 이전0.14.0이다. 이번 요청에는 새 재시작 지시가 없어 앱을 종료·교체하지 않았다. 검증은 실제 egui offscreen·격리 Chrome fixture로 수행하며, 사용자 데이터나 유료 에이전트 작업을 실행하지 않는다. 검사 빌드를 제품 릴리스로 배포하지 않으므로 버전은 올리지 않았다. 향후 전달 빌드에는0.14.0보다 높은 버전과 binary/두 plist 필드 확인이 필요하다.

물리 휴대폰, 유료 provider의 전체 작업 실행, 사용자 DB의 실제 재시작은 이번 증거 범위에 포함하지 않는다. 플랫폼 글꼴 rasterization과 macOS 창 버튼 여백, 실제 권한·작업 상태에 따른 문구/활성 여부는 시안 예시와 다를 수 있다. 이를 픽셀100% 일치라고 표현하지 않는다.

기록 종류가 없는 예전 항목과 알 수 없는 새 종류는 전체 목록에서 계속 보인다. 분류는 저장된 종류만 사용한다. 현재 작업 이벤트에 설정 변경 종류가 없으므로 실제 설정 이력을 만들어 넣지 않는다.

## 실패 접근 기록

- 초기 폼620px 추정은 앞부분 CSS만 본 오류였다. 최종 cascade와 실제 승인 화면은 max960이므로 그 기준으로 수정했다.
- production Chrome fixture가 접힌 규칙 내부 checkbox를 바로 focus하고 config={} 및 삭제된 헤더 버튼을 사용해 실패했다. 실제 summary click·정확한 bound DTO·현재 terminal nav로 fixture를 갱신했고 기존 NFC/ACK/정확한 watch/복귀 검증은 유지했다.
- 최종 렌더 측정이 입력이 없는 화면에서 get_all_by_role로 실패했다. query_all_by_role로 바꿔 빈 화면도 실제 캡처한다.
- 전체 검사 중 소스가 추가 수정되어 이전 coordinator는 명시 중단했다. 최종 검증은 source hash를 고정하고 gate마다 확인한다. 중단된 run을 완료로 계산하지 않는다.
