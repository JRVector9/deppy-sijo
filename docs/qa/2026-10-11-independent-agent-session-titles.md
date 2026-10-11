# 독립 세션 작업 제목·대화 바인딩 검증 — 2026-10-11

Canonical source: /Users/jr/Desktop/projects/deppy-sijo-performance, base799fae89 + working tree. PWA 모바일4건 완료 후 진행. 실행 앱PID21173/0.10.1, 사용자 PTY/DB/설치된 shim은 변경하지 않았다. 다른 작업자의 history/Git/resize 변경을 보존한다.

## 확정 원인과 실제 증거

Design의 두 Codex 세션은 작업 제목 대신 Design · 1/2를 표시했다. 두 pane를 같은 대화에 연결한 결과 기존 공유 대화 보호가 작업 문구를 숨긴 것이다.

read-only metadata.sqlite3와 프로세스 조사:
- workspace f820fa9f-1170-464a-9e87-2b677e6f1c44의 pane622d9033-…/7b5da262-…에 동일 Codex ID 저장. 조사 사이 둘 다 01a128ae-…→01a128af-…로 함께 변경.
- 실제 TUI PID61978은 bare codex, PID19752는 codex resume 01a121af-61f8-7153-940a-c992a3ce66f0. 서로 다른 zsh51396/60099를 거쳐 App21173 소속이며 cwd는 동일 Design. 명시적 resume ID가 저장 ID와 다름.
- 두 TUI 모두 열린 rollout 없음. 같은 cwd의 PID32412 codex app-server는 별도 node_repl 소속이며 사용자 pane TUI가 아님.
- 실제 codex-cli0.162.1의 --help/fork --help에서 공유 서버, --no-daemon, fork 지원 확인. 실제 CLI 세션/프롬프트 실행 없음.

기존 agent_detect.rs::bind_transcript(Codex)는 열린 파일이 없으면 process cwd의 최신 rollout을 선택했다. 실제 저장 결과와 일치하는 오바인딩 경로다. Codex 이력 원문 조회에도 같은 cwd 폴백이 있었다.

## cmux 참고

- [Agent hooks](https://github.com/manaflow-ai/cmux/blob/main/docs/agent-hooks.md): native ID와 workspace/surface, 실행 정보를 함께 관리.
- [Codex wrapper](https://github.com/manaflow-ai/cmux/blob/main/Resources/bin/cmux-codex-wrapper): hook의 실제 ID를 사용하고 fresh/resume/fork ID를 추측하지 않음.
- [Live process binding](https://github.com/manaflow-ai/cmux/blob/main/CLI/CMUXCLI%2BAgentHookProcessBinding.swift): 살아 있는 PID/TTY/surface 검증.
- [Resume verification](https://github.com/manaflow-ai/cmux/blob/main/CLI/CMUXCLI%2BCodexResumeBindingVerification.swift): 실제 checkpoint/귀속 확인.
- [Workspace auto naming](https://github.com/manaflow-ai/cmux/blob/main/docs/workspace-auto-naming.md): 작업 정보에 따른 제목, 사용자 지정 제목 우선.
- [Codex0.162.1 CLI](https://github.com/openai/codex/blob/rust-v0.162.1/codex-rs/tui/src/cli.rs): 현재 local/no-daemon/fork 인자 검증.

pane별 ID·실행 소유권 원칙을 적용했다. --no-daemon은 Deppy의 pane 환경 상속 문제를 피하기 위한 추가 구현이다. cmux가 이 flag를 사용한다고 주장하지 않는다.

## 수정 파일

- crates/app/src/agent_detect.rs: Codex/Grok cwd 최신 폴백 제거, exact resume ID/writable rollout, helper 제외, Codex ID와 파일명 대조, exact 이력 조회, resume/fork와 맞지 않는 hook 제외.
- crates/app/src/app.rs: 4 provider의 pane별 작업 지시, 현재 실행/birth/hook 시간 검증, 과거 hook 보호와 활성 override 분리, binding 선적용, 미확인 pane 과거 작업 문구/context 정리. 중복 Codex 저장 ID 자동 복원 중지, 명시적 공유 resume은 fork.
- crates/app/src/agent_shim.rs: 지원 local TUI만 --no-daemon. 구버전/remote/helper/중복/원래 인자 보존. 실제 user shim 설치 없음.
- crates/storage/src/db.rs: 기존 updated_at을 HookSessionRow에 노출, 스키마/마이그레이션 변경 없음.

## 실제 실행 검사

중간 focused independent_15PASS(신규7 포함), agent_detect70PASS/1ignored, agent_shim14PASS. 로그 /private/tmp/deppy-independent-title-first-tests-20261011.log, exit0.

후속 gate: 실제 macOS lsof1PASS, App2837PASS/38ignored, storage426PASS, mcp-proxy108PASS/1ignored. /private/tmp/deppy-independent-title-verified-gate-20261011.log. 이 gate의 strict Clippy는 제거된 cwd 폴백의 Grok helper가 test에서만 사용되어 FAIL. cfg(test)로 제한했다.

최종 검토에서 Codex hook/명시적 resume ID 대조 누락 발견. independent_codex_hook_rejects_other_resume_and_fork_source는 수정 전0PASS/1FAIL을 확인했다(/private/tmp/deppy-independent-title-hook-red-20261011.log). 보완 후 최종 gate는 exit0이다.

| 검사 | 최종 결과 | 로그 |
| --- | --- | --- |
| independent_ 회귀 | 17PASS(이번 신규9 포함) | deppy-independent-title-final-review-20261011.log |
| 전체 App binary 단위 회귀 | 2838PASS/0FAIL/38ignored | 같은 로그 |
| storage 전체 lib | 426PASS/0FAIL | deppy-independent-title-verified-gate-20261011.log |
| mcp-proxy 전체 | 108PASS/0FAIL/1ignored | 같은 로그 |
| strict all-targets Clippy(App/storage/MCP) | PASS(-D warnings) | deppy-independent-title-final-review-20261011.log |
| App 통합 경계4target | 39PASS/0FAIL | deppy-independent-title-boundaries-20261011.log |
| cargo fmt / git diff --check | exit0 | 최종 gate/직접 실행 |

로그는 모두 /private/tmp/ 아래에 있다. 독립 회귀는 native ID/다른 resume/fork/helper, 정확한 파일 조회, read-only peer/복수 writer, 실제 macOS lsof, hook 시간, 4provider pane prompt, stale native ID, 활성 runtime만 정리, 중복 saved ID 자동 복원 방어, fake CLI의 8실행 호환성을 포함한다. focused17은 전체App에 포함되므로 합계에서 다시 더하지 않는다. App2838+storage426+MCP108+통합39=3411PASS, 별도 PWA Chromium9PASS. 전체 workspace의 모든 crate/ignored test를 실행했다고 주장하지 않는다.

```sh
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py --batch '[["fmt","--all"],["test","-p","deppy-sijo","independent_","--","--test-threads=1"],["test","-p","deppy-sijo","--bin","deppy-sijo","--","--test-threads=1"],["clippy","-p","deppy-sijo","-p","storage","-p","mcp-proxy","--all-targets","--","-D","warnings"]]'
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py test -p deppy-sijo --test agent_state_boundary --test dotenv_launch_boundary --test lazy_bounded_worker --test logging_policy -- --test-threads=1
git diff --check
```

실패 이력: --lib -p deppy-sijo는 binary-only라 실행 불가, --bin으로 정정. 첫 실제 lsof fixture는 macOS /var→/private/var alias 때문에 App2836PASS/1FAIL/38ignored. 제품은 logical/canonical root를 이미 처리했다. fixture를 canonicalize 후 실제 lsof/전체 App 통과. 실패를 PASS에 합산하지 않는다.

## 후속 리뷰와 수정 완료

[후속 코드 리뷰](2026-10-11-independent-agent-session-titles-review.md)에서 실제 저장·초기조회·대화전환 경로의 미수정 결함6건을 재현했다. 위 통과 결과는 해당 누락 시나리오를 커버하지 않는다. 구현을 완전히 검증된 상태로 판단하지 않는다. 리뷰 시점에는 제품 코드를 변경하지 않았다. 이후 사용자 병렬 수정 요청으로6건과 추가 경계 문제를 모두 수정하고 root 통합3443PASS/0FAIL 및 strict Clippy/fmt를 확인했다. 최신 상태는 [수정·검증 보고서](2026-10-11-independent-agent-session-titles-fixes.md)를 따른다.

## 전달 경계

native ID/hook 없는 기존 bare CLI의 실제 대화를 추측할 수 없다. 새 shim 실행/검증 가능한 hook·owned rollout이 필요하다. 이미 오염된 저장 ID로 원래 대화를 역추정해 DB를 덮어쓰지 않는다.

사용자 앱의 수정 후 화면, 설치형 iOS PWA 및 사용자 prompt E2E는 미검증. 소스/격리 회귀까지이며 재시작·릴리스·커밋·푸시 없음. 변경 artifact는 shipped0.10.1보다 높은 버전 및 binary/bundle 검증이 필요하다.
