> 후속 구현: 사용자의 시안 적용 요청에 따라 실제 이력 UI에 날짜별 목록/선택 상세, 필터, 시작 시각 정렬, 검증된 현재 상태 표시를 연결했다. 고유한 정확한 pane 지시로 증명되는 공유 턴만 수집하고, 불명확한 기록은 계속 차단한다. 아래 수치는 구현 전 실행 중 0.8.7의 조사 기록이다. 적용 화면은 [실제 렌더러 결과](mockups/work-history-applied-2026-10-08.html)를 참조한다. 소스/테스트 결과이며 실행 중 앱 재시작이나 누락 기록 전체 복구를 의미하지 않는다.

# Git·이력 점검 — 2026-10-08

| Priority | Location | Finding | Impact | Next step |
|---|---|---|---|---|
| high | app.rs:stage_detected_work_history / shared_agent_transcript_sessions | 공유 대화 보호 조건으로 현재 nomorevibe 이력 저장이 건너뛰어짐 | 10월 8일 작업이 이력에 없음 | pane별 지시·턴 출처를 확인한 기록만 저장하고 중단 사유 표시 |
| medium | app.rs:stage_attention_work_history / ui/work_history.rs | 과거 Working 상태를 현재 실행 상태처럼 표시 | 5~14일 전 작업도 실행 중으로 보임 | 저장 상태와 현재 확인 상태 분리, 종료·재연결 시 재조정 |
| low | ui/git_panel.rs / agent_work_git.rs | 파일별/폴더별 변경 수와 현재/저장 시점 의미가 다름 | 같은 저장소의 68개와 10개가 상충해 보임 | 집계 기준 통일 또는 기준·시점 명시 |

## 확인 범위

실행 중인 **0.8.7**의 nomorevibe Git/이력 화면을 CUA로 열고, 같은 저장소의 Git CLI 및 SQLite를 읽기 전용으로 대조했다. 수정된 소스로 앱을 재실행한 검증은 하지 않았다. 사용자 세션에 시험 메시지를 보내거나 DB 기록을 수정하지 않았다.

### Git 조회

- 실제 Git 탭: `main → origin/main`, 변경 **68개**, upstream 대비 커밋 변경 **0개**, 워크트리 **32개**와 ‘목록이 잘렸습니다’ 표시.
- 같은 수집 기준 `git status --porcelain=v1 -z --untracked-files=all`: 미추적 파일 **67개** + 수정 **1개** = **68개**. 변경 목록 조회 누락은 이 샘플에서 재현되지 않았다.
- `git rev-list --left-right --count HEAD...@{upstream}`: **0 / 0**. ‘브랜치에 COMMIT 됨’은 모든 커밋의 로그가 아니라 upstream 대비 파일 변경 영역이다.
- `git worktree list --porcelain`: **81개**. 화면의 32개는 의도된 상한이고 절단 안내가 나온다.
- 이력의 ‘작업 트리 변경 10’과 Git의 68은 `agent_work_git`의 `--untracked-files=normal`과 패널의 `-uall` 차이다. `normal` 결과는 미추적 디렉터리 등 **9항목** + 수정 **1개**다. 작업 변경량 또는 커밋 수로 해석하면 안 된다. 이력의 최신 cwd 행은 Git 조회 결과로 갱신돼, 엄밀한 작업 시점 스냅샷도 아니다.
- 빈 커밋 저장소는 Git 패널이 HEAD 조회에서 전체 CollectionFailed가 되는 기존 제한이 있다. 이번 실행 중 저장소에서는 재현되지 않았다.

### 이력 조회

- 처음 화면은 ‘실행 중 + Claude’ 필터로 **66개 표시 / 전체 256개**였다. ‘전체’로 바꾸면 **256 / 256**이 표시된다. 256개는 저장소의 workspace별 보존 상한이며 무제한 전체 이력이 아니다.
- DB도 **Working 66 / Completed 190**으로 동일하다. 화면 필터가 없던 기록을 만든 문제는 아니다.
- nomorevibe의 마지막 저장 `updated_at`: **2026-10-07 08:35:53 KST**. 현재 native 대화 파일은 10월 8일까지 갱신돼 있다. 최근 이력이 저장되지 않는 문제가 확인됐다.
- 현재 persisted pane `722c86d1…` / session `bed83c0f…`도 native 대화 `113b810f…`에 연결된다. 같은 Claude 대화 id `113b810f…`에 nomorevibe hook 키 `:2`, `:3`, `:4`, `:6` 네 개가 저장돼 있다. `shared_agent_transcript_sessions`는 현재 바인딩과 과거 hook 바인딩을 함께 비교하며, 하나의 대화를 여러 pane이 공유한 것으로 판단한다. `stage_detected_work_history`는 그 세션의 턴을 모두 건너뛴다. 이는 다른 세션의 지시를 잘못 귀속하는 버그를 막기 위해 도입한 보호 동작이다. 보호를 제거하면 이전 제목/이력 혼동을 되살릴 수 있으므로, 정확한 pane 출처가 있는 턴만 수집하는 후속 수정이 필요하다.
- `stage_attention_work_history`도 같은 공유 보호를 사용하고, 현재 바인딩의 최신 턴 상태만 갱신한다. 이미 사라진 세션과 과거 턴의 Working 값은 그대로 남을 수 있다. 화면에서 5일/14일 전 기록이 ‘실행 중’으로 표시되는 것을 확인했다. 66개 모두 실제 미완료라고 판단할 근거는 없다.
- ‘최신순’은 작업 시작 시각(`occurred_at`) 대신 `updated_at` 기준이다. 상태 변경으로 과거 작업이 상단으로 올라갈 수 있다. 제안에서는 시작 시각과 마지막 상태 확인 시각을 구분한다.

## 이번에 완료한 원래 수정

- 같은 세션 생성 기능의 워크스페이스·세션 메뉴·단축키·빈 화면 버튼/안내를 **세션 추가**로 통일했다. 번역 5개 카탈로그를 함께 정리했다. 기존 워크트리 추가 기능은 별도 동작임을 유지한다.
- Git·이력·문서 보조 UI를 workspace/runtime/pane/session별로 보관한다. 새 세션에 이전 보조 탭이 따라가지 않고, 돌아오면 원래 선택과 탭을 복원한다.
- 같은 문서를 다른 세션에서 열어도 버퍼 id와 편집기 undo/caret id가 다르다. 전역 문서 수/메모리 상한과 id 기반 IO는 유지한다.
- 늦은 Git/원문 결과는 요청한 세션에 적용한다. Git 목록과 파일 diff의 요청 세대를 분리해 diff 클릭이 진행 중인 목록 조회를 취소하지 않게 했다. 파일 diff 요청은 Git 목록의 loading 상태를 켜지 않는다. 다른 세션의 대기 조회를 덮어쓰던 단일 retry는 표면별 최신 요청을 남기는 전역 유계 FIFO로 바꿨다.
- **위 이력 수집/상태 문제는 점검·보고 범위다. 현재 코드에서 해결했다고 주장하지 않는다.** UI 시안 역시 제품 기능으로 적용한 것이 아니다.

## HTML 제안

[이력 화면 시안](mockups/work-history-proposal-2026-10-08.html)

권장: 날짜순의 작은 목록 + 우측 지시/결과/원문 상세. 큰 에이전트 그룹을 펼치지 않아도 최근 지시를 볼 수 있게 한다. 워크스페이스 전체/현재 세션, 기간, 상태, 검색 조건과 표시 수를 한 영역에 배치한다. 카드형 대안으로 전환할 수 있다. 수집 중단, 로딩, 오류, 기록 없음은 각각 다르게 표현한다. 마지막 저장 상태와 현재 상태 미확인을 분리하고, Git 버튼에는 ‘현재 Git 보기’라고 표시한다.

HTML은 명시된 예시 데이터만 쓰며 실제 세션·DB·Git에 연결하지 않는다. Chrome 전용 새 탭에서 정상 화면 렌더를 확인했다. 사용자가 다른 Chrome 창으로 이동해 검색·시나리오 전환의 실제 클릭 검증은 이어서 수행하지 않았다. JS 문법 검사 `node --check /private/tmp/deppy-history-proposal-20261008.js`는 exit0이다. 모바일에는 목록→상세→목록 구조를 제안했으며 실제 모바일 브라우저 검증은 하지 않았다.

## 소스 검증

- 마지막 전체 App 재검증: **2792 PASS / 36 ignored**, integration **4+5+15+15 PASS**, exit0. 로그 `/private/tmp/deppy-session-isolation-final-full-20261008.log`. i18n **8 PASS**, `/private/tmp/deppy-session-isolation-full3-20261008.log`. 초기 Clippy 경고는 수정 후 strict 재검증을 통과했다.
- 후속 최종 점검: session_aux **4 PASS**, Git **36 PASS**, native menu captures **5 PASS (17화면)**, strict all-target Clippy·fmt·boundary **PASS**. 로그 `/private/tmp/deppy-session-isolation-final2-20261008.log`.
- 이 테스트는 수정 소스와 격리 fixture를 검증한다. 실행 중 0.8.7에 수정이 로드됐다는 의미는 아니다. release/bundle/version 갱신이나 재시작은 이번 작업에서 수행하지 않았다.
