# Command Center v2 — PR 계획 검토

2026-10-20. 이 문서는 계획 단계의 확인 기록이다. 제품 구현·실행 검증 기록이 아니다.

## 기준과 격리

- 작업 폴더: `/Users/jr/Desktop/projects/deppy-sijo-command-center-20261020`.
- 통합 브랜치: `feat/agent-command-center-v2-20261020`.
- `git fetch origin` 후 기준 HEAD와 origin/main: `f78ebb69c94ec7cb72ae9a1159d2137c0d6a5679`.
- 기존 settings/cloud 작업 폴더의 미커밋 제품 변경은 복사하지 않았다.
- PR 계획: `docs/superpowers/plans/2026-10-20-command-center-v2-implementation.md`.

## 시안과 계획 대응

| 채택한 요구 | 담당 PR | 확인 기준 |
| --- | --- | --- |
| 프로젝트·독립 세션 탐색 | 1,2,3 | 안정된 ID, 초안·선택 대상 격리 |
| 메인 목표·계획·분담·취합 | 4,5 | 실제 provider와 정확한 배정 연결 |
| 우측 현재 프로젝트 작업자 관측 | 1,3,6 | 세션/attempt별 상태와 정확한 진입 |
| 승인·결과·Git·기록 | 6 | 승인 owner, 결과 revision, source 일치 |
| 프로젝트 실행 규칙·기기 선택 | 2,4,7 | 설정 snapshot, 경로·권한·capacity 확인 |
| 컴퓨터·휴대폰 연결 | 7,8 | 실행 worker와 열람/입력 연결 구분 |
| 반응형·테마·입력 | 3,8,9 | 시안 치수, IME, viewport, 실제 화면 비교 |
| 복구·오프라인·불확실 상태 | 1,2,4,7,9 | 중복 실행 방지, 초안·과거 결과 보존 |

## 이번에 실제 확인한 항목

- Python/hashlib로 `reference.json`의 HTML2개·설계1개·이미지3개, 총6개 SHA-256 일치를 확인했다.
- 선택 HTML의 로컬 상대 링크1개가 실제 파일을 가리키는 것을 확인했다.
- PR1~9 본문9개와 placeholder0개를 검사했다. `rg` placeholder 검색의 exit1은 일치 항목 없음이다.
- CSS 최종 override와 계획의 기본/반응형 치수를 대조했다. HTML이 제품 화면을 렌더한 증거라는 의미는 아니다.
- 기존 runtime command/event, Fleet/AgentSurface, draft, DB schema/migration, history/Git, remote/Relay, PWA 코드를 읽고 재사용 경계와 새 계약을 계획에 기록했다.
- `git diff --name-only f78ebb69c94ec7cb72ae9a1159d2137c0d6a5679 -- crates Cargo.toml Cargo.lock`은 출력 없이 exit0이었다. 제품 코드와 버전 변경이 없다.
- 새 작업 폴더의 `git diff --check`와 기존 작업 폴더의 handoff 대상 검사는 모두 exit0이었다. 별도로 새 계획/QA Markdown2개의 줄 끝 공백·마지막 개행을 Python으로 검사해 통과했다.

Cargo 테스트·Clippy·제품 빌드·네이티브/PWA 실제 동작 검증은 실행하지 않았다. 기존 시안 단계에서 실행한 검사를 이번에 다시 실행한 것으로 합산하지 않는다. 앱 종료·실행·재시작, push, GitHub PR 생성·병합도 하지 않았다.

## 자체 검토 결과

채택한 구조의 각 요구를 담당 PR과 완료 기준에 연결했다. UI 적용 이정표(PR3), 로컬 작업 흐름(PR6), 기기/PWA 포함 전체 인수(PR9)를 구분한다. 중간 PR은 통합 기능 브랜치로만 모으고 main에 직접 반영하지 않는다. 이후 각 PR에서 실제로 실행한 검증 결과를 별도로 기록한다.
