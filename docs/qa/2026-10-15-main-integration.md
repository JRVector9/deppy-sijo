# main 통합 검토 — 2026-10-15

## 목표와 보존

main에 없는 변경을 각각 대조하고 승인된 기능과 실제 결함 수정만 통합한다. 원본 dirty, 제외 브랜치, 조사용 worktree는 삭제하지 않는다. 앱 재실행은 승인되지 않았다.

- 시작 main: d5f88b353de077920afca316dc5b903ed104d8f0, 버전0.11.0.
- 통합 기반: d8826f0bdb8d89255f3448b5b61b6eee83faa87a, 버전0.12.1. main의 모든 커밋을 포함하며 이후23개 커밋이 있다.
- 원본 tracked45/untracked361개를 별도 integration worktree로 복사하고 SHA를 대조했다.
- local branch120개, 기존 프로젝트 worktree12개, 열린 PR11개를 목록화했다. 증거: evidence/2026-10-15-main-integration/.
- 모든 local/remote branch와 기존 macOS bundle의 최고 버전을 확인했다. 이번 버전은0.12.2다. 원래 shipped bundle 최고0.11.0, source 최고0.12.1.
- ship/review 스킬은 필수 checklist.md 누락으로 자동 workflow 중단. 직접 코드 검토와 저장소 검증으로 계속한다. 스킬 완료로 세지 않는다.

## 열린 PR별 결정

| PR | 내용 | 결정 | 근거 |
|---|---|---|---|
| #191 | SSH 화면 첫 입력까지 고정 | 제외, 원본 보존 | 현재 Session::finish_ansi_replay가 alt 화면을 primary/history로 옮기고 새 출력을 표시한다. 옛 held viewport는 보이는 화면과 실제 입력 화면을 다시 분리하고 최신 input_snapshot/복원 gate와 다른 정책이다. |
| #192 | 파일 트리 접두어 키보드 탐색 | 미채택 실험 제외 | 별도 keyboard-focus 계약이 필요한 새 기능이다. 현재 popup/terminal 입력 소유권 안정화에 예전 탐색 경로를 추가하지 않는다. |
| #193 | 재귀 프로젝트 파일 검색 | 미채택 실험 제외 | 현재 파일 패널 시안에 없는 검색 UI/재귀 worker를 추가한다. 이번 승인 기능 안정화 범위에서 제외한다. |
| #194 | 새 파일/폴더 생성 모달 | 대체 구현으로 충족, 중복 제외 | 현재 file_tree_create 공통 popup 및 같은 너비/Enter/Escape/외부 입력 차단 회귀가 있다. |
| #195 | 빈 탭에 shell tab 추가 | 대체 UX로 충족, 중복 제외 | 현재 승인된 빈 영역 세션 추가/우측 pane 분할과 controller intent/retry 경로를 유지한다. 예전 별도 shell-tab queue를 중첩하지 않는다. |
| #197 | 세션 binding/자동 이어가기 격리 | 대체 구현으로 충족, 중복 제외 | 프로세스·파일 소유권 기반 binding, trusted pane prompt, shared_agent_transcript_sessions, ambiguous_saved_codex_binding, Claude/Codex fork가 더 최신이다. |
| #198 | Finder 복사·이동 | 대체 구현으로 충족, 중복 제외 | 현재 handle_clipboard_shortcuts와 native move sequence, root/볼륨/선택/팝업 fence를 유지한다. |
| #190 | base64 0.23.1 | 이미 포함, 중복 제외 | 현재 lock에0.23.1이 있다. 다른 의존성의0.22.1은 호환성을 위해 유지한다. |
| #206 | upload-artifact v4→v7 | 이번 통합 제외 | 제품 수정과 무관한 major CI runtime 변경이며 기존 artifact 계약을 유지한다. |
| #207 | setup-node v4→v7 | 이번 통합 제외 | 현재 action도 지정 Node24로 WebCrypto 검사를 실행한다. major action 변경은 필요하지 않다. |
| #217 | Rust dependency14개 업데이트 | 이번 통합 제외 | egui/kittest/선택 Ghostty 및 정확히 고정된 ureq unversioned connector까지 변경한다. 현재 GUI/IME/transport 버전과 lock을 유지한다. 과거 PR GREEN은 현재 소스의 업그레이드 검증이 아니다. |

제외는 기능이 전혀 없다는 뜻이나 해당 PR의 모든 코드가 잘못됐다는 뜻이 아니다. 현재 승인된 동작과 중복되거나 다른 UX/의존성 계약을 추가하므로 이 통합에 넣지 않는 결정이다.

## 기존 작업 폴더별 결정

| 작업 폴더 | 잔여 내용 | 결정 |
|---|---|---|
| deppy-sijo | AGENTS/handoff, 과거 HTML/설계 | 현재 규칙/승인 시안과 중복. 원본 보존 |
| deppy-sijo-agent-wait-audit | 모델 label, 정렬/Finder, drop fallback, 원문자 렌더 | 실제 Claude 버전 label 누락만 선별. 정렬/Finder는 추가 UX, drop fallback은 실패한 문서 열기를 PTY 입력으로 바꾸며 최신 owner/backpressure와 다름, 원문자 확대는 셀폭 밖 그리기 실험이라 제외 |
| deppy-sijo-cloud-agent-mcp | handoff/과거 시안 | 기능 HEAD는 main 조상. 문서 실험 보존 |
| deppy-sijo-env-api-preview | 초기 API 시안 | 최신 프로젝트 키/연결 관리에 대체됨 |
| deppy-sijo-layout-20260906 | 과거 layout | main 이관과 이후 resize/IME/입력 개선 유지 |
| deppy-sijo-perf-baseline | 변경 없음 | 조사용 baseline 제외 |
| deppy-sijo-perf-model | packed cell/grapheme 호출부 | 현재 wide_spacer()/attrs()/push_cell_text/graphemes가 적용됨. 중복 제외 |
| deppy-sijo-perf-render | 과거 render/IME | 현행 렌더/IME 경로 유지, 예전 역사 재머지하지 않음 |
| deppy-sijo-perf-storage | handoff만 dirty | 추가 제품 변경 없음 |
| deppy-sijo-performance | 23개 commit + 최근 수정 | 통합 대상, 최종 검사 재실행 |
| deppy-sijo-project-keys | 변경 없음 | #223으로 main 포함 |
| deppy-sijo-pwa-reading-20261008 | 과거 viewer 읽기/검색 | 최신 읽기/보존/반응형 구현과 중복, 제거 요청한 검색 API를 다시 넣으므로 실험 patch 제외 |

## 오래된 local branch 처리

120개 중 main 조상인 가지는 중복이다. 예전 audit-pr1~21 worker snapshot은 #216의 통합·후속 수정으로 대체됐다. Grok/Cursor/환경키/Git/이력/Relay/layout/IME/perf 가지는 main의 후속 이관 구현을 기준으로 본다. main ancestry가 다르다는 이유로 오래된 전체 소스를 merge하지 않는다. 정확한 branch별 HEAD/분기 수/고유 commit 제목은 local-branches-before.json에 보존했다. 브랜치 삭제/force push는 하지 않는다.

## 소스 검토와 회귀 경계

- 클라우드: principal/session generation/lease epoch와 clock 연속성, 종료 신규 접수 봉인, bootstrap bounded retry, durable continuation. cloud_agent/agent-mcp/web-remote 회귀.
- 입력: 표시 scrollback과 live editor snapshot 분리, query fence가 이전 agent 응답만 폐기하고 사용자/새 셸 입력은 유지. App composer/pty/runtime/session 회귀.
- 파일/팝업: 동일 handle regular bounded read, symlink target/revision 재확인, dead-owner 명시적 복구, 취소 tombstone/queue retry. App document/file_tree/session_aux/connector-ui 회귀.
- DB/프로세스: IMMEDIATE 아래 WAL 포함 online backup, backup 최종 symlink 비추적, birth+PGID+SID/WNOWAIT 소유권 확인 정리. storage-core/runtime/PTY 회귀.
- 표시/성능: corpus revision+원본 Arc로 safe projection 무효화, opaque routing identity 보존, snapshot당 process adjacency1회. secret/App/resource_monitor 회귀.
- 추가 모델 표시: launch value는 그대로 유지하고 resolvedModel로 label만 보완. parser 기반 회귀를 구현 전에 실행해 실제0PASS/1FAIL(exit101) 확인. 실패는 버전 없는 label이며 compile 오류가 아니다.
- 실제 외부 계정/공개 tunnel/물리 모바일/Windows runtime은 로컬 검증 범위가 아니다.

## 이번 실행 결과

- 현재 source0.12.2 전체 workspace 실제 exit0, **5733PASS/0FAIL/65ignored**, Cargo harness78개. 각 harness의 마지막 summary만 집계해 nested subprocess3개 중복을 제외했다.
- `cargo fmt --all -- --check` 실제 exit0.
- Node relay16PASS/0FAIL; Chromium readability114/history89/reading182/cloud-results77 assertions, 각각 exit0. 실제 Deppy 실행 없음.
- `cargo clippy --offline --locked --workspace --all-targets -- -D warnings`, xtask check-boundary/check-deps, diff 검사 모두 실제 exit0.
- 제품 commit057048ac. merge/build는 진행 중. 과거 QA 결과를 이번 실행으로 대체하지 않는다.


## main 이후 커밋23개 각각 검토

| Commit | 변경 | 처리 |
|---|---|---|
| e32a1353 | chore(release): 0.12.0 | 승인 기능/후속 수정/검증 기록 유지 |
| 5cdafd3f | fix(ui): 우클릭 메뉴 폭을 항목에 맞추고 행 간격과 모서리를 넓힌다 | 승인 기능/후속 수정/검증 기록 유지 |
| 6bf56f53 | docs: 우클릭 메뉴 감사 캡처를 새 폭·간격으로 갱신한다 | 승인 기능/후속 수정/검증 기록 유지 |
| 3ac1f61f | chore(release): 0.12.1 | 승인 기능/후속 수정/검증 기록 유지 |
| 19fa8df3 | docs(cloud): 연결 관리 개선안과 병렬 구현 계약을 기록한다 | 승인 기능/후속 수정/검증 기록 유지 |
| 35e8a264 | fix(cloud): 서버 준비 상태와 연결 안내를 정확히 표시한다 | 승인 기능/후속 수정/검증 기록 유지 |
| 1878a449 | feat(pwa): 클라우드 작업 결과와 권한별 관리 화면을 연결한다 | 승인 기능/후속 수정/검증 기록 유지 |
| 2fdf4a4d | fix(secret): 클라우드 출력의 줄바꿈과 부분 비밀을 안전하게 가린다 | 승인 기능/후속 수정/검증 기록 유지 |
| 1dd6d16d | fix(runtime): 연결별 내부 작업 ID를 입력 수신증에 허용한다 | 승인 기능/후속 수정/검증 기록 유지 |
| 9d0ed76b | feat(cloud): 연결별 권한과 작업 관리·이력·복구를 통합한다 | 승인 기능/후속 수정/검증 기록 유지 |
| fcf237f7 | docs(cloud): PR 1–6 수용 검증과 완료 기록을 남긴다 | 승인 기능/후속 수정/검증 기록 유지 |
| cd614286 | fix(cloud): redact obfuscated run data before storage and display | 승인 기능/후속 수정/검증 기록 유지 |
| 18491c2e | fix(mcp): retain approved OAuth connection names across refresh | 승인 기능/후속 수정/검증 기록 유지 |
| a3843d99 | fix(cloud): preserve input continuity and reliable result navigation | 승인 기능/후속 수정/검증 기록 유지 |
| bf13eb48 | docs(cloud): record seven review fixes and executed verification | 승인 기능/후속 수정/검증 기록 유지 |
| 5fff2264 | fix(cloud): retire input consent when continuity clocks are unavailable | 승인 기능/후속 수정/검증 기록 유지 |
| 8c5e0db4 | fix(cloud): refresh cached display masking after credential changes | 승인 기능/후속 수정/검증 기록 유지 |
| 7e02a46d | fix(pwa): report observed cloud navigation completion and failures | 승인 기능/후속 수정/검증 기록 유지 |
| 70380aea | docs(qa): record residual cloud fixes and regression evidence | 승인 기능/후속 수정/검증 기록 유지 |
| 51b23890 | feat(cloud): align agent settings with approved tabbed design | 승인 기능/후속 수정/검증 기록 유지 |
| 2fd73a5b | fix(cloud): match permission column sizing and checkbox hit areas | 승인 기능/후속 수정/검증 기록 유지 |
| 6a06d319 | docs(cloud): record design parity review and native UI evidence | 승인 기능/후속 수정/검증 기록 유지 |
| d8826f0b | fix(input): inspect live editor while terminal is scrolled | 승인 기능/후속 수정/검증 기록 유지 |
