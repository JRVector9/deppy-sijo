# 팝업·우클릭·폴더·파일트리 결함 수정

기준 HEAD: `d8826f0bdb8d89255f3448b5b61b6eee83faa87a`. Canonical: `/Users/jr/Desktop/projects/deppy-sijo-performance`. 사용자 요청으로 [직전 리뷰](../reviews/2026-10-14-popup-context-folder-file-tree-review.md)의 R1–R10을 GPT-6.1-Sol/xhigh 서브에이전트 3명이 별도 작업 트리에서 병렬 수정했다. Root가 watcher/picker, 소스 통합, Cargo 실행, 최종 검증과 문서를 담당했다.

현재 상태: **R1–R10 소스 수정·통합·교차 리뷰·실행 검증 완료**. 최종 고유 테스트 **3354 PASS / 0 FAIL / 기존43 ignored**. 미커밋 소스이며 사용자 실행 앱은 변경하지 않았다.

## 원인별 수정

| 항목 | 변경 | 검증 |
| --- | --- | --- |
| R1 영구삭제 대상 교체 | 실패한 실제 Trash worker가 원래 root/parent/target FD·dev/inode/type 증명을 전달. 삭제 직전 경로와 객체를 검증하고 atomic quarantine move 후 다시 확인. 바뀐 객체는 덮어쓰지 않고 복원, 충돌 시 양쪽 보존 | 확인 후/worker 대기/atomic move 시 교체, 외부 symlink, root 교체, restore·cleanup 실패, 정상 파일/폴더/symlink, 취소·깊이 제한·합성 proof 거부 |
| R2 Rename 종료키 전달 | 공통 inline editor 소유권·IME 검사와 raw/native 전체 batch 소비, 닫힌 pass 펜스 사용 | 실제 FileTree+Workspace+terminal renderer Enter/Escape·IME |
| R3 OAuth 이전 키 undo 복원 | 공용 masked secret field, draft 수명 종료 시 shared undo clear·위젯 제거·focus 반환 | 실제 ConnectorUi 취소/제출/작업·서버·revision 변경/숨김/상태 제거·복제 undo9개 |
| R4 트리 focus 회수 | 트리 keyboard owner 동안 터미널 입력·자동 refocus 차단. 명시적 pane 이동과 터미널 클릭은 기존 재획득 유지 | 다음 frame 삭제 단축키, automatic vs explicit focus, 터미널 클릭 후 첫 키/IME/paste |
| R5 Rename 행 ID 변경 | full path+edit generation ID로 고정 | watcher listing에서 앞 sibling 추가 후 focus·target·입력 유지 |
| R6 Secondary drag | 시작/진행/자동스크롤/해제 모두 Primary 기준 | Secondary edge 이동과 기존 정상 Primary 드래그 |
| R7 권한 복구 retry | manual Refresh/denied 행 펼치기에서만 재조회. 실제 denied node 상태 변경 시에만 flat rebuild | 실제 mode000→0700 복구, 명시 펼치기, 동일 목록 row buffer/watch plan 재사용 |
| R8 symlink watch 누락 | watch worker가 physical root를 등록·반환, exact completion에서 적용. callback을 logical namespace로 옮긴 뒤 기존 필터 적용 | 실제 macOS FSEvents와 삭제된 .env/ignore/hidden/outside 경로·stale generation |
| R9 사라진 picker 경로 | nonempty directory identity probe 실패 시 DB setter 전에 오류. 이전 path/anchor/volume 보존 | 격리 SQLite 사라진 폴더/일반파일 거부, 정상 경로·빈 detach 호환 |
| R10 부분 복사 목록 | 정확히 일치하는 IO completion은 성공/실패 모두 영향 디렉터리 갱신, 원래 오류 유지 | watcher 없는 실제 부분 복사 실패 후 생성된 파일 표시 |

삭제 proof는 host helper `crates/app/src/file_delete.rs`에 있으며 UI는 opaque proof를 운반한다. leaf에 filesystem I/O·스레드·watcher를 추가하지 않았다. unsupported OS의 permanent deletion은 증명이 없으면 거부하고 normal Trash는 기존 경로를 사용한다. macOS 실행만 검증했으며 Linux/Windows 통과를 주장하지 않는다.

## 실제 검증

| 검사 | 결과 | 로그 |
| --- | --- | --- |
| 수정 전 delete + mutation boundary | control1 PASS / 실제10 FAIL | [delete-red](evidence/2026-10-14-popup-context-folder-fixes/delete-red.log), [boundary-red](evidence/2026-10-14-popup-context-folder-fixes/delete-boundary-red.log) |
| 수정 전 input composite | 실제8 FAIL | [input-red](evidence/2026-10-14-popup-context-folder-fixes/input-red.log) |
| 수정 전 OAuth | 실제9 FAIL | [oauth-red](evidence/2026-10-14-popup-context-folder-fixes/oauth-red.log) |
| 수정 전 watcher/picker | 실제4 FAIL | [watch-picker-red](evidence/2026-10-14-popup-context-folder-fixes/watch-picker-red.log) |
| 수정 후 connector-ui 전체 | 35 PASS/doc0/exit0 | [connector-green](evidence/2026-10-14-popup-context-folder-fixes/connector-green.log) |
| 수정 후 input composite | 8 PASS/exit0 | [app-input-green](evidence/2026-10-14-popup-context-folder-fixes/app-input-green.log) |
| App 전체 | 3181 PASS / 기존39 ignored / exit0 | [app-full-final](evidence/2026-10-14-popup-context-folder-fixes/app-full-final.log) |
| terminal 전체 | 138 PASS / 기존4 ignored / doc0 / exit0 | [terminal-green](evidence/2026-10-14-popup-context-folder-fixes/terminal-green.log) |
| App/connector-ui/terminal all-targets strict Clippy | -Dwarnings exit0 | [clippy-final](evidence/2026-10-14-popup-context-folder-fixes/clippy-final.log) |
| UI boundary | exit0 | [boundary-final](evidence/2026-10-14-popup-context-folder-fixes/boundary-final.log) |
| lint 정리 후 App popup·입력·audit 재검사 | 87 PASS / 기존10 ignored / exit0 | [app-popup-final](evidence/2026-10-14-popup-context-folder-fixes/app-popup-final.log) |
| lint 정리 후 기존 동일 목록 회귀 | 1 PASS / exit0 | [app-refresh-final](evidence/2026-10-14-popup-context-folder-fixes/app-refresh-final.log) |
| fmt / HTML JS syntax / diff | exit0 · SHA/기존 source 보존 확인 | [static-final](evidence/2026-10-14-popup-context-folder-fixes/static-final.log) |

전체 suite 합계에 narrow/child-process 결과를 중복 합산하지 않았다. App 전체 통과 후 enum 이름·동일 의미의 if-let 조건·0o0 fixture 표기만 strict lint에 맞춰 정리했으며, 영향 App popup87개 및 동일 목록 회귀1개를 재실행했다.

중간 실패를 통과로 세지 않았다. 첫 App 새21개 gate에서 깊이 fixture PATH_MAX 실패1개는 segment만 줄이고130단계/보존 assertion은 유지했다. 첫 whole App3180PASS/1FAIL/39ignored에서 unconditional Refresh rebuild 회귀를 확인해 denied node 변화가 있을 때만 rebuild하도록 고쳤다. 초기 OAuth fixture의 undo 안정 시간과 private API compile 오류는 fixture를 교정한 뒤 corrected9 actual RED를 실행했다. 공유 Cargo target의 다른 worktree binary0 selected는 검증으로 세지 않고 source touch 후 실제4 RED를 재실행했다. [근거·SHA manifest](evidence/2026-10-14-popup-context-folder-fixes/README.md)에 보존한다.

## 검토·실행 범위

Root가 worker 패치를 읽고 3-way 통합했다. FileTree의 인접 helper 충돌은 keyboard focus 메서드와 delete-confirmation cleanup 메서드를 모두 보존하여 해결했다. 별도 read-only 교차 리뷰에서 input 담당이 삭제/watcher/picker를, delete 담당이 input/OAuth를 대조했고 추가 확정 결함은 발견하지 못했다. generic review skill의 필수 gstack checklist가 없어 해당 파이프라인 완료로 주장하지 않으며 직접 소스 검토·회귀 검증 결과를 기록한다.

기존 terminal/runtime wide-cursor 미커밋 수정은 worker 시작 시 복사한 baseline과 `cmp`로 동일함을 확인했다. 이전 문서와 unrelated active-work HTML, 원래 deppy-sijo checkout도 보존했다. popup 디자인 계약 및 HTML case13·25에 상태·보호 규칙을 반영했으며 시각적 치수는 변경하지 않았다.

실제 사용자 앱·DB·credential·clipboard에는 테스트를 수행하지 않았다. 임시 파일/SQLite, fake key, native watcher 및 화면 밖 egui 검증이며 전체 사용자 GUI E2E/실제 제공자 인증을 의미하지 않는다. 소스/테스트 버전0.12.1, release artifact 생성·배포·재실행·커밋·푸시는 이 작업에서 하지 않았다.

재실행 명령(canonical cwd):

```sh
CARGO_BUILD_JOBS=2 cargo test --offline --locked -p deppy-sijo --bin deppy-sijo -- --test-threads=1
CARGO_BUILD_JOBS=2 cargo test --offline --locked -p connector-ui
CARGO_BUILD_JOBS=2 cargo test --offline --locked -p terminal
CARGO_BUILD_JOBS=2 cargo clippy --offline --locked -p deppy-sijo -p connector-ui -p terminal --all-targets -- -D warnings
CARGO_BUILD_JOBS=2 cargo run --offline --locked -p xtask -- check-boundary
cargo fmt --all --check
git diff --check
```

다른 worktree에서 동일 target으로 실행한 직후에는 canonical crate entry source를 touch하여 과거 binary의0 selected를 통과로 잘못 집계하지 않는다. fixture나 생산 코드를 변경했다면 영향 검사를 다시 실행한다.
