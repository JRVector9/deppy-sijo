# Claude 이력 시안과 현재 구현 대조 — 2026-10-11

## 비교 대상 / 실제 확인 범위

- 사용자 지정 시안: https://claude.ai/artifact/1NHCPjiQ2nHnQgWDv5tvEX#page-dcb81aee8d83
- native Chrome에서 지정 URL을 열고 `작업 이력 개선 시안`의 실제 AX와 screenshot을 확인했다. `제안 — 스레드 제목 · 결과 한 줄 · 프로젝트` artboard와 `지금 화면` artboard를 대조했다. artifact viewer가 관찰 당시 `#artboard-f456eb10460f`로 표시되었으며 canvas zoom은60%였다. 시안의 정확한 CSS px 값을 이 screenshot만으로 단정하지 않는다.
- 현재 소스: `/Users/jr/Desktop/projects/deppy-sijo-performance`, source HEAD799fae89 + 지난 turn의 미커밋 history 변경. 실제 제품 렌더 `docs/mockups/work-history-parity-2026-10-10-assets/after-normal.png`와 관련 소스를 읽어 대조했다. 현재 앱을 재시작하지 않았다.
- 앞선 수정 기준은 `docs/mockups/work-history-proposal-2026-10-08.html`이다. 이번 링크는 같은 화면의 간격 보정만이 아니라 정보 구조를 바꾼 별도 개선 시안이며, 현재 구현이 이 링크와 같다는 판단은 잘못이다.

## 확인된 차이

| 영역 | 링크 시안 | 현재 구현 / 근거 | 필요한 변경 |
|---|---|---|---|
| 목록 구조·대표 제목 | 스레드 묶음/시간순 전환, 문맥을 설명하는 대표 제목과 그 아래의 개별 턴 | 기본 browser는 날짜별 flat row; 각 제목은 현재 instruction 첫 줄. browser.rs:631,1040. legacy card의 그룹 helpers는 work_history.rs:225 이하에 있으나 시안의 thread heading/preview 규칙은 아니다. | 스레드 view와 대표 제목 계산; 짧은 이어서 지시와 실제 작업 주제를 구분 |
| 목록 결과·프로젝트 | 각 턴은 지시 + 결과 한 줄, group에 프로젝트/브랜치/에이전트/턴수/시간범위 | compact_row는 instruction/session/provider/state/time만 표시한다. browser.rs:986–1095. | bounded 결과 한 줄, project+branch header, group count/time |
| 상세 맥락 | 프로젝트/경로/브랜치/에이전트 상단 strip, 대표 제목, 이번 지시와 직전3개 지시를 나란히 보여줌 | 선택 instruction 제목과 지시·결과/원문 tabs. 직전 지시 흐름 없음, cwd/branch는 접힌 info 안에 있음. browser.rs:665–820. | 상단 metadata strip 및 앞선 흐름과 작업 의미를 별도 표시 |
| 결과 표시 | 짧게 정리한 결과와 보조 정보, 원문 보기 | agent_summary 원문을 그대로 표시하며 번역/요약을 만들지 않는다. browser.rs:778 및935. | 이미 저장된 한 줄을 추출하는 것과 새로운 한국어 요약 생성은 별도 요구사항으로 구분 |
| 세션 표시 이름 | 사람에게 보이는 에이전트 이름 | App app.rs:20022에서 raw pane.title을 넘기고 browser.rs:1170에서 그대로 출력. workspace.spawn.agent 11가 표시될 경로를 확인했다. 번역 helper workspace.rs:9929는 이미 존재. | display_pane_title 재사용 |
| 도구막대·행/액션 구성 | search/status+counts/project filter, 더 넓은 detail 비중, 하단 action bar | session/period/provider/status filters, 목록/카드/정렬 header, default1:1.1 split, action은 summary scroll content 안. browser.rs:298–894. 일반37/보조26/행68pt. | 구성/위치/너비와 action 고정 영역을 링크 시안 기준으로 변경, 원본 치수는 따로 대조 |

## 구현 시 고려할 경계

시안 메모의 대표 제목은 ‘같은 agent_session_id에서 가장 최근의 긴 지시’라는 휴리스틱이다. 실제 구현은 workspace/provider/pane의 소유권도 유지하고 선택한 과거 턴 이후의 지시가 과거 제목을 바꾸지 않게 정해야 한다. 각 turn의 exact identity로 이동·원문·copy를 유지한다. 기존 raw 영어 응답의 첫 문장 추출만으로 한국어 문장이 생성되지는 않는다. PR/파일/테스트 개수도 source에서 확인 가능한 정보만 표시해야 한다.

이번 요청은 비교이며 제품 코드 추가 수정/테스트 재실행/release/commit/push/app restart를 하지 않았다. 지난 turn의62PASS+offscreen1PASS+strict Clippy는 10월8일 로컬 시안 적용 코드의 근거이며 이번 링크 시안의 구현 완료를 증명하지 않는다.
