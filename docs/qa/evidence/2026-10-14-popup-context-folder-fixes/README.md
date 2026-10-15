# 팝업·파일트리 수정 근거

작업 경로는 canonical `deppy-sijo-performance`, 기준 HEAD d8826f0b다. 실제 격리 임시 FS/SQLite·macOS native FSEvents·production egui/renderer/OAuth UI 회귀를 실행했다. fake credential만 사용한다. raw log 저장 시 행 끝 공백만 제거했으며 원본/저장 SHA를 manifest에 기록한다.

- `delete-red.log`: 정상 삭제 control1 PASS, 실제 결함8 FAIL.
- `delete-boundary-red.log`: atomic-move/restore 경계 실제2 FAIL.
- `input-red.log`: FileTree+Workspace+terminal renderer 실제8 FAIL.
- `oauth-red.log`: 실제 ConnectorUi OAuth9 FAIL.
- `watch-picker-red.log`: production native watcher/격리 settings worker 실제4 FAIL.
- `connector-green.log`: 수정 후 connector-ui 전체35 PASS/doc0.
- `app-input-green.log`: 수정 후 composite8 PASS.
- `app-audit-first-failed.log`: 수정 후20 PASS/1 fixture failure. 깊이 검사 경로가 macOS PATH_MAX를 넘은 사전 setup 오류를 짧은 segment로 고쳤다.
- `app-full-first-failed.log`:3180 PASS/1 실제 회귀 FAIL/39기존ignored. 일반 Refresh가 동일 목록 버퍼·watch plan을 다시 만든 회귀를 조건부 rebuild로 고쳤다.
- `fixture-oauth-first-failed.log`: shared undo debounce 전제 실패를 포함한 초기 fixture. 이를 제품 결함 RED로 세지 않았다.
- `fixture-oauth-private-api-failed.log`: private undo API compile 오류. public API로 교정했다.
- `stale-target-zero-tests.log`: 공유 Cargo target의 다른 worktree binary 재사용으로0 selected. 검증 통과로 세지 않고 source touch 후 다시 실행했다.

- `app-full-final.log`: corrected 전체 App3181 PASS/0 FAIL/39기존ignored.
- `terminal-green.log`: terminal138 PASS/4기존ignored/doc0.
- `clippy-first-failed.log`: 신규 enum naming/nested if/fixture permission 표기의 strict lint 실패. allow 없이 동작을 유지해 정리했다.
- `clippy-final.log`:3crate all-targets -Dwarnings exit0.
- `app-popup-final.log`: lint 정리 후 production popup/input/audit87 PASS/10기존ignored.
- `app-refresh-final.log`: lint 정리 후 동일 목록 재사용1 PASS.
- `boundary-final.log`:UI/native/runtime 경계 exit0.

최종 고유3354 PASS/43기존ignored이며 narrow와 child-process 결과를 전체 suite에 중복 합산하지 않았다. 전체 App 통과 후 lint 이름/조건 표기만 정리하고 모든 새 App regression과 기존 popup 및 동일 목록 회귀를 다시 실행했다. 중단된 미수정 재검사는 집계하지 않는다. 사용자 앱·DB·실키·clipboard를 변경하거나 앱을 재실행하지 않았다.
