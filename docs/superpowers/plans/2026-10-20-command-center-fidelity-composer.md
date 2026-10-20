# Command Center fidelity and composer implementation plan

**Goal:** 승인 시안의 실제 화면 규격과 정보 구조를 복원하고, 미연결 상태에서도 입력·에이전트 설정이 가능한 작성 영역을 구현한다.

**Architecture:** render-only UI와 host 권한/저장 계약을 유지한다. 입력 영역을 별도 모듈로 분리해 네이티브 화면 담당과 작성/host 담당의 파일 충돌을 방지한다. PWA는 별도 파일 소유권으로 병렬 구현한다. root가 중앙 체크리스트·통합·리뷰·최종 증거를 관리한다.

**Tech stack:** Rust/egui, 기존 Command Center host/SQLite worker, 공용 JavaScript/CSS renderer, 실제 Chrome/egui fixture.

## 파일 소유권과 순서

1. root: 현재0.14.0 전달/리뷰 문서와 버전 변경을 checkpoint commit으로 보존. 아래 체크리스트 작성. `main.rs`의 `composer_id/submit/composer`를 `composer.rs`로 기계적으로 이동하고 `main.rs`에 재수출해 기존 호출/테스트 유지.
2. 네이티브 담당: `ui/command_center/{mod,main,workers,layout,settings,devices}.rs`와 필요한 UI fixture만. `composer.rs`와 host 파일 수정 금지. 작성 담당에게 새 상태 필드가 필요하면 root가 소유권 조정한다.
3. PWA 담당: `web/shared/command-center.{js,css}`, 관련 browser fixture. 네이티브와 같은 미연결 작성/설정 흐름을 기존 SaveProject/CreateMain protocol로 제공. 새 wire 필드는 작성/host 담당과 먼저 합의.
4. 작성/host 담당: `ui/command_center/composer.rs`, `command_center_host.rs`, 필요한 host/route/draft tests. `mod.rs`/`main.rs`는 직접 수정하지 않고 root에게 필요한 변경 요청. snapshot 기존 필드/Intent를 우선 재사용.
5. 각 담당은 독립 worktree/commit, 모델 gpt-6.1-sol/xhigh, root 통합 전 파일 변경/회귀 명령과 결과를 보고. 원본 시안 hash 변경 금지. 사용 중인 앱/DB/유료 provider 작업 금지.

## 단계별 실행·검증

- [x] 실제 현재 앱을 CUA AX/스크린샷으로 직접 확인하고, 승인 HTML 최종 스타일과1200px 논리 너비로 대조해 C01–C26을 작성했다. 수정 전6case 기하 측정을 `docs/qa/assets/command-center-v2-exact-20261020/before/`에 저장했다.
- [x] root가 composer 모듈 분리 후9da3417a 기준 기존 Command Center 회귀92PASS/0FAIL/1ignored를 직접 실행했다. 새 동작 테스트와 최종 통합 검증은 별도 진행한다.
- [x] 새 입력 동작은 unbound 작성, Enter 선택 진입, 연결 후 초안 유지, 프로젝트 격리, 자동 전송 없음, IME의 실제 회귀를 먼저 RED로 확인한다.
- [x] 네이티브/PWA/host 각 담당이 최소 변경으로 GREEN을 만든다.
- [x] root가 각 commit과 소유권·실제 동작을 리뷰하고 통합한다.
- [x] 같은 내용·viewport에 대해 네이티브/HTML/PWA 렌더 대조, 실제 기하 측정과 화면별 체크를 기록한다.
- [x] fmt/clippy/필요 회귀와 통합 테스트를 실행한다. 기존 통과 결과를 현재 결과로 재사용하지 않는다.
- [x] 체크 완료마다 증거·파일·명령을 중앙 체크리스트에 기록하고 handoff 갱신한다.

## 중앙 체크리스트 — root만 갱신

완료 표시에는 실제 코드·테스트·렌더 근거가 필요하다. 데이터가 없는 기본 화면은 실제 빈 상태로 검증한다.

| ID | 영역 | 확인·수정 기준 | 담당 | 상태 |
| --- | --- | --- | --- | --- |
| C01 | 공통 | 색상·다크/라이트 token을 승인 최종 CSS와 대조 | native/PWA | 검증 · N/P: 양 테마 token·fade 제거 |
| C02 | 헤더 | 48/46px 높이, 탭 간격·활성 선·글자 크기 | native/PWA | 검증 · N/P: 실제 헤더 48/46px |
| C03 | 모바일 | 글자 강제 축소 제거, 필요한 가로 스크롤 | native/PWA | 검증 · N/P: 390px·모바일 drawer |
| C04 | 프로젝트 행 | glyph/좌측 이름/우측 개수 34px 독립 정렬 | native | 검증 · N: 34px glyph·이름·개수 |
| C05 | 좌측 세션 | 프로젝트별 작업/세션 표시·상태·58px 밀도·선택 표시 | native/PWA | 검증 · N/P/H: exact route 중복 제거 |
| C06 | 좌측 하단 | 설정·메인 진입·설명 정렬과 ghost 버튼 | native/PWA | 검증 · N/P: 하단 정렬·ghost |
| C07 | 프로젝트 헤더 | 84/78/72px, 이름·강조 dot·경로·선택 label | native/PWA | 검증 · N/P: 84/78/72px |
| C08 | 헤더 액션 | 설정 텍스트·접기 아이콘·전체 기록 위치, 모바일 선택기 숨김 | native/PWA | 검증 · N/P: 액션·모바일 선택기 |
| C09 | 본문 | padding·max reading width·MAIN 열 gutter·구분선 | native/PWA | 검증 · N/P: reading 폭·gutter·선 |
| C10 | 글자 계층 | 목표18/16, 섹션14/13, 설명12 muted | native/PWA | 검증 · N/P: 실제600 font face |
| C11 | 단계 표시 | 선 연결·현재 실행 강조·작은 label | native/PWA | 검증 · N/P: 현재 단계·연결선 |
| C12 | 배정 행 | compact 세 열, 담당자 시작 정렬·세로 선·역할별 scope | native/PWA | 검증 · N/P: compact 3열·owner 선 |
| C13 | 작업자 패널 | 필터·badge·scope·본문·관측 정보·규칙 밀도 | native/PWA | 검증 · N/P: 필터·관측·규칙 |
| C14 | 작업자 액션 | 28px 버튼과 원문 아이콘, 고정 하단 기기 영역 | native/PWA | 검증 · N/P: 실제28px 액션 |
| C15 | 상세 | 작업과 결과 통합·원문·Git·기록 순서/정보 구조 | native/PWA | 검증 · N/P: 4탭·좌측 설명·29px 정보 행 |
| C16 | 설정 | 최종 cascade max960px form·36px field·inline 행·접힌 실행 규칙·저장/취소 | native/PWA | 검증 · N/P/H: max960·36px·저장 CAS |
| C17 | 보조 화면 | 작업 목록·기기·기록의 행/구분/빈 상태·반응형 | native/PWA | 검증 · N/P/H: 실제 기록·종류 저장·정확한 작업 이동 |
| C18 | 작성 외형 | 62→100/60→88px 포커스 높이·한 테두리·화살표 전송 | native/PWA/compose | 검증 · N/P: 실제 입력62→100/60→88 |
| C19 | 작성 대상 | 프로젝트/메인 또는 역할/기기를 식별, 대상에서 설정 진입 | compose/PWA | 검증 · N/P: 대상 선택·설정 진입 |
| C20 | 미연결 입력 | unbound에서도 실제 타이핑·IME·초안 저장 가능 | compose/PWA | 검증 · N/P/H: 미연결 실제 편집 |
| C21 | 입력 내 설정 | 실제 세션/에이전트/모델/강도 선택·취소·실패 복구 | compose/PWA | 검증 · N/P/H: 실제 catalog·취소·실패 |
| C22 | 연결 초안 | main 연결 전후 보존, 프로젝트/작업 간 격리 | compose/PWA | 검증 · N/P/H: 바인딩·late load·용량 |
| C23 | 전송 권한 | 연결 전 Enter는 선택 진입, 연결만으로 자동 전송 없음 | compose/PWA | 검증 · N/P/H: 연결 후 명시 재전송 |
| C24 | 기본 안내 | unbound/JSON/내부 준비절차 대신 사용자가 이해할 안내 | native/PWA/compose | 검증 · N/P: 사용자 안내 |
| C25 | 화면 보존 | 포커스 확대/설정 열기로 터미널·본문/원격 scroll 소실 없음 | all | 검증 · N/P/H: focus·scroll·동일버전 ACK |
| C26 | 권한 회귀 | exact target/readonly/blocked/offline/IME/중복 접수 guard 유지 | all | 검증 · N/P/H: readonly·exact·IME·receipt |

## 증거 식별자와 해석

- N: 실제 production font를 설치한 egui 렌더/접근성/paint 회귀. 데스크톱1440×900·1200×800, 모바일390×844와 다크/라이트52PNG. 최종 source hash와 상세 결과는 QA 보고서와 evidence manifest에 기록한다.
- P: 실제 Chrome의 승인 HTML/공용 renderer 동일 viewport 비교20case(10쌍), 작성 focus 전후13개 기하·색상 속성 대조, 전체 shared browser 회귀. 사용자 브라우저/물리 휴대폰을 대신 검증했다고 주장하지 않는다.
- H: root 독립 App/host 회귀. unbound Enter·초안 충돌·256개 용량·late load/ACK·A→B→A 편집·자신의 저장 revision 갱신·readonly/exact target을 실제 코드로 검사한다.
- 기본 capability/기록이 없는 상태는 빈 상태로 표시한다. 원본 시안 예시의 허용되지 않은 버튼이나 가상의 결과를 실제 제품에 추가하지 않는다. 실데이터 부재에 따른 상태 차이는 레이아웃 일치와 구분한다.
- 최종 결과와 source별 실행 명령은 [QA 보고서](../../qa/2026-10-20-command-center-fidelity-composer.md)에 보존한다. 전체 초기6095PASS와 후속 최종 검증은 서로 합산하지 않는다.
