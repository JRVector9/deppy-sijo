# 독립 세션 작업 제목·대화 바인딩 수정 — 2026-10-11

PWA 모바일 4건 구현/Chromium9PASS 완료 후 진행. 소스 수정과 격리 검사만 수행한다. 실행 중인 사용자 앱·PTY·DB·설치된 shim을 변경하지 않으며 하단 컴포저는 조사/PR 계획만 유지한다.

## 확인한 원인

Codex 탐지기는 pane의 열린 rollout을 찾지 못하면 같은 cwd의 최신 rollout을 선택했다. 실제 Design의 두 CLI는 bare 실행과 명시적 resume 실행이지만, 두 pane의 저장 native ID는 같은 최신 ID로 함께 바뀌었다. Codex0.162.1의 공유 app server에서는 TUI에 열린 rollout이 없을 수 있다.

잘못된 동일 native ID를 발견한 기존 공유 대화 보호가 작업 문구를 숨겨 Design · 1/2로 표시했다. 제목 formatter만 바꿔서는 잘못된 대화 연결과 재시작 resume을 해결할 수 없다. cmux의 surface/native ID·실행 소유권 기준을 참고했다. 상세는 [QA](../../qa/2026-10-11-independent-agent-session-titles.md).

## PR1 — 확인한 대화만 연결

- [x] Codex/Grok의 cwd 최신 파일 live binding 제거.
- [x] Codex 정확한 resume UUID 해석, fork 원본 ID 배제, app-server 등 helper 제외.
- [x] lsof -Ffan의 유일한 writable rollout만 허용. read-only peer/복수 writer 제외, native ID와 파일명 ID 대조.
- [x] Codex 이력 원문도 exact native ID만 조회.
- [x] Codex hook과 명시적 resume/fork ID 대조.

## PR2 — pane별 작업 문구

- [x] Claude/Codex/Grok/Kimi 모두 native ID가 일치하는 pane의 작업 지시 적용.
- [x] 기존 hook updated_at 조회. 현재 종류·살아 있는 실행·시작 이후 이벤트만 활성 연결 후보로 사용(스키마 변경 없음).
- [x] 활성 hook과 과거 hook을 분리해 닫힌 peer의 공유 대화 보호 유지.
- [x] binding을 표시 전에 적용, 미확인 pane의 과거 작업 문구/context 정리.
- [x] provider별 독립 prompt, 다른 native ID, 활성 runtime만 정리하는 회귀.

## PR3 — 새 실행/복원 독립성

- [x] 기능을 지원하는 local Codex TUI만 --no-daemon 사용. 실제 help로 capability 확인.
- [x] 구버전·remote·helper·중복 flag·사용자 인자 보존. fake CLI로 생성 shim을 실행해 검증.
- [x] 잘못 저장된 중복 Codex ID는 자동 resume 중지. 원래 대화를 임의 선택하지 않음.
- [x] 명시적 공유 resume은 Codex fork, Claude --fork-session으로 독립화.

## 검증/전달

실제 macOS lsof, App 전체, storage/MCP 회귀 및 strict Clippy 결과는 QA/handoff에 기록한다. 처음 lsof fixture의 /var와 /private/var alias 문제를 canonical path로 수정했다. 최종 검토의 Codex hook 검증 누락은 실패 테스트로 확인 후 보완했다.

native ID/hook이 불명확한 기존 bare CLI는 작업을 추측하지 않고 기본 제목을 유지한다. 현재 실행0.10.1 앱에 적용하거나 실제 세션 화면에서 수정 결과를 검증했다고 주장하지 않는다. 변경 artifact 전달은 버전 증가/binary·bundle 검증, 재시작은 해당 작업의 명시 요청이 필요하다.


## 후속 코드 리뷰 R1~R6 수정 — 병렬 작업

[후속 리뷰](../../qa/2026-10-11-independent-agent-session-titles-review.md)에서 드러난 실제 경로 누락을 별도 수정한다. 이전 체크는 최초 구현 기록이며 후속 결함의 해결을 뜻하지 않는다.

- [x] 저장 PR(R1/R4): exact provider/native ID UPDATE, UTF-8 256B preview, transport를 preview 전에 정규화, 실제 proxy→DB→Hooks 검사.
- [x] 실행 소유권 PR(R2/R6): runtime token + bounded kernel ancestor identity stamp, raw Linux birth ticks equality, 검증된 같은 프로세스의 thread 전환과 캐시 교체. prompt stamp는 binding stamp와 분리.
- [x] 초기 projection PR(R3): 실행을 검증한 첫 snapshot부터 지시 반영. stale prompt/다른 runtime/workspace/동일 본문 새 실행 방어.
- [x] Codex shim PR(R5): 명령 위치와 prompt 위치 구분, 정확한 원래 argv 및 local/remote/구버전/helper 호환성.

담당은 title_storage_fix / title_execution_fix / title_projection_fix이며 모두 gpt-6.1-sol xhigh. root가 각 변경을 리뷰하고 공유 Cargo gate로 통합 검증한다. 실제 GitHub PR 생성·커밋·푸시·릴리스·재시작은 이 작업에 포함되지 않는다.

후속 완료 결과: [수정·통합 QA](../../qa/2026-10-11-independent-agent-session-titles-fixes.md). root gate3443PASS/0FAIL 및 strict Clippy/fmt/diff 확인. PR 단위는 논리적 변경 묶음이며 GitHub PR·커밋·푸시는 생성하지 않았다.
