# 독립 세션 제목·PWA 후속 코드 리뷰 — 2026-10-11

> 후속 수정 완료: 사용자 수정 요청에 따라 R1~R6 및 추가 경계 회귀를 모두 보완했다. root 통합3443PASS/0FAIL, strict Clippy/fmt 통과. [수정·검증 보고서](2026-10-11-independent-agent-session-titles-fixes.md). 아래 내용은 수정 전 리뷰 기록이다.

| Priority | Location | Finding | Impact | Next step |
| --- | --- | --- | --- | --- |
| high | crates/storage/src/db.rs:7679 | 작업 지시 UPDATE가 Claude로만 제한됨 | Codex/Grok/Kimi의 pane별 지시 저장 누락 | provider와 native ID를 함께 검증해 저장 |
| high | crates/app/src/agent_detect.rs:980 | 현재 hook ID를 최초 resume argv에 고정하고 이전 캐시 재사용 | TUI 내부 대화 전환 후 제목·복원 대상이 옛 대화에 남음 | 현재 실행 소유 hook의 전환을 검증하고 캐시 교체 |
| medium | crates/app/src/app.rs:10848 | 첫 hook 조회의 유효 지시도 fresh에서 제외 | 빠르게 보낸 첫 지시/작업 제목이 계속 미반영될 수 있음 | 이미 실행 검증한 최초 snapshot은 반영 |
| medium | crates/storage/src/db.rs:7674 | 256바이트 초과 지시를 자르지 않고 저장 거절 | 긴 새 지시 후 이전 작업 제목이 남음 | UTF-8 안전한 유계 미리보기 저장 |
| medium | crates/app/src/agent_shim.rs:147 | 모든 positional 인자를 helper 명령으로 검사 | resume/fork의 prompt 값에 따라 local 격리 해제 | subcommand 위치와 prompt 위치 구분 |
| medium | crates/app/src/agent_detect.rs:132 | Linux process birth tick을 Unix epoch 초와 비교 | 재사용된 pane의 과거 hook이 새 실행으로 인정될 수 있음 | runtime/실행 identity 및 같은 시간 단위 사용 |

## 범위와 변경 상태

Canonical source /Users/jr/Desktop/projects/deppy-sijo-performance, base799fae89 + working tree. 이번 리뷰는 직전 제목/native binding 4파일과 PWA assets 변경을 검토했다. 다른 작업자의 history/Git/resize 변경은 리뷰 결론에 합치지 않았다. 제품 코드·repo test·user DB·설치된 shim·실제 prompt·앱 실행 상태는 변경하지 않았다. 보고서/handoff만 추가한다. 앱PID21173/0.10.1 유지.

앞선 App2838/storage426/MCP108/통합39/PWA9 검사 통과 사실은 유효하지만 이 결함들을 커버하지 않는다. 직전 「구현 완료」 판단은 현재 실제 데이터 경로까지 검증된 완료로 볼 수 없다. 아래 6건은 미수정이다.

## Details

### R1 — provider별 prompt 저장이 끝까지 연결되지 않음

새 app projection은 Codex/Grok/Kimi에도 pane별 prompt를 사용한다. 그러나 Db::record_hook_task_prompt의 UPDATE WHERE는 여전히 kind='claude'다. 호출 결과가 Ok여도 UPDATE0행이어서 조용히 실패한다. 실제 compiled storage Db로 만든 격리 DB에서 동일 native ID·짧은 prompt를 넣었을 때 Claude만 task_prompt가 저장되었다. Codex/Grok/Kimi는 모두 None.

표시 회귀 independent_provider_titles_use_each_panes_own_native_id_and_prompt는 이미 채워진 HashMap을 입력한다. 실제 proxy→DB→Hooks→projection이 연결되는지 검증하지 않았다. 저장 API에 provider를 전달해 exact(session_key,kind,native ID)를 검사하고 4provider 실제DB/관찰기 통합 회귀를 추가해야 한다.

### R2 — TUI의 실제 대화 전환을 최초 argv로 거절

hook_binding_matches_command의 Codex resume 분기는 처음 실행한 ID와 다르면 false다. Codex는 같은 TUI에서 /resume 및 새 대화 전환을 지원한다. 현재 버전 primary [session lifecycle](https://github.com/openai/codex/blob/rust-v0.162.1/codex-rs/tui/src/app/session_lifecycle.rs)는 같은 App에서 thread를 교체한다. 최초 argv는 계속 원래 ID다.

새 hook을 거절한 뒤 detect_cached의 deterministic cache 분기는 owner의 종류가 같으면 옛 binding을 반환한다. 실제 agent_detect.rs 전체를 변경 없이 임시 module에 복사하고 실제 agent_transcript/proc_info 및 기존 deps와 링크했다. 격리 home의 유효한2rollout, 실제 probe PID/birth, synthetic ps rows(codex resume A), 이전A 캐시, 현재B hook을 넣었다. 전체탐색을 두 번 강제해도 둘 다 A 반환. 가짜 행을 사용한 상태 전이 재현이며 사용자 Codex 세션에서 /resume을 실행한 E2E는 아니다.

stale startup hook과 현재 실행에서 온 thread 전환을 구별할 provenance가 필요하다. 현재 pane/runtime/실행 및 native ID의 검증된 전환을 수용하고 기존 캐시를 교체해야 한다. argv만으로 고정하거나 검증 없이 hook을 무조건 신뢰하는 수정은 피한다.

### R3 — 최초 projection의 검증된 지시가 계속 버려짐

apply_agent_state_storage_projection은 이제 현재 kind/PID/birth와 hook 시각을 검증한 후 new_prompts를 만든다. 그러나 refresh_fresh_hook_prompts는 had_projection=false면 fresh를 비우고 observed만 next로 기록한다. 다음 poll에서 같은 prompt면 observed와 같아서 fresh로 승격되지 않는다.

실제 생산 함수를 그대로 임시 Rust driver에 추출해 첫 유효 prompt snapshot→동일 snapshot3회로 실행했다. 첫 조회와 세 번째 poll 모두 fresh에 지시가 없었다. 처음 hook 조회보다 입력이 빠르고 persisted prompt가 없는 새 pane, 특히 shared transcript 보호로 원문 지시를 쓰지 못하는 경우 제목이 기본값으로 남는다. 첫 snapshot부터 실행 provenance로 검증된 지시를 반영하는 회귀가 필요하다. 현재 hook 검증보다 앞선 stale row는 별도 테스트로 계속 제외해야 한다.

### R4 — 긴 지시 이후 이전 제목 유지

record_hook_task_prompt는 task_prompt_text로 human transport를 풀어낸 다음 전체 본문에 256바이트 한도를 검사한다. 초과하면 Err, proxy의 run_hooks는 그 결과를 무시한다. upsert_hook_session은 같은 kind/native ID의 이전 prompt를 유지하므로 새 지시 대신 이전 작업 문구가 남는다.

실제 compiled storage DB로 짧은 이전 작업→동일 native ID의 새480바이트 한글 지시를 저장했다. 새 저장은 Err이고 task_prompt는 이전 작업 그대로였다. 긴 지시를 preview 한도에 맞춰 UTF-8 경계에서 저장하되 원문 prompt를 변경하지 않는 방식과 256바이트 경계/한글 회귀가 필요하다. 이는 사용자 prompt 전송 실패 자체의 원인이 아니라 세션 제목 저장 문제다. desktop composer 수정 금지 범위는 유지한다.

### R5 — resume/fork의 prompt가 명령으로 오인됨

새 Codex shim은 모든 인자에서 helper 명칭을 검사한다. 실제 codex resume --help는 SESSION_ID 뒤에 optional PROMPT를 허용한다. resume ID review의 review는 prompt지만 helper로 인식해 --no-daemon을 생략한다. fork ID app-server도 동일하다.

실제 shim_header/isolation shell 본문을 그대로 추출해 fake CLI와 실행했다. 지원하는 CLI에서 resume ID normal은 --no-daemon1회, resume ID review와 fork ID app-server는0회. 실제 hooks 인자 조립은 이 probe에서 제외했으며 flag 판정 shell은 원문 그대로다. subcommand 위치를 판정한 뒤 resume/fork의 native ID 및 prompt를 따로 처리해야 한다. 원문 인자/구버전/remote/helper 호환 회귀도 유지한다.

### R6 — Linux 시계 단위 불일치

proc_info.rs::read_linux_process_birth는 /proc/pid/stat field22의 boot-relative raw tick을 ProcessBirth.seconds에 넣는다. accepts_hook_timestamp는 이를 DB strftime Unix epoch seconds와 비교한다. 단위와 기준점이 다르므로 생존한 새 PID를 확인해도 이전 실행의 hook 시각을 거르지 못한다.

실제 생산 method를 포함한 module probe에서 Linux식 birth fixture3600과 Unix epoch의 한 시간 전 hook을 넣자 true였다. Linux host E2E가 아니라 source 계약과 fixture로 확인했다. macOS의 같은 초 hook은 false로 제외되는 것도 확인했지만 이는 함수가 명시적으로 선택한 보수적 정책이므로 별도 결함으로 합산하지 않았다. 시간만으로 실행 귀속을 판단하기보다 runtime/execution provenance를 사용해야 한다.

## 재현과 검사 기록

임시 evidence: /private/tmp/deppy-independent-title-review-xzs1dw3g

- storage_probe.rs + storage_probe.log: 실제 compiled Db, provider4건과 긴 prompt 재현, compile/run exit0.
- cold_prompt_probe.rs + cold_prompt_probe.log: 실제 refresh_fresh_hook_prompts 그대로 추출, compile/run exit0.
- agent_detect.rs + cache_probe.rs + cache-probe.log: 생산 detector 전체 copy, 실제 transcript/proc_info, isolated home + synthetic process table, compile/run exit0. cache2passes와 시각 fixture 재현.
- codex + fake-cli + shim-probe.json: 생산 shim shell 판정 본문, fake CLI3cases, 모두 실행 exit0.
- provenance.json: 읽은 source SHA256과 링크한 rlib 경로. 기존 제품 test를 새로운 회귀 PASS로 재분류하지 않았다.
- 첫 probe 준비는 함수 generic signature를 잘못 찾는 substring 실패. 제품 코드를 고치지 않고 추출 marker를 정정한 뒤 모든 probe를 실행했다.

PWA의 가로 clip/검색 제거/width fit/header 변경에서 추가로 확정할 결함은 찾지 못했다. 기존 실제 Chromium9PASS를 참고했고 동일 suite를 다시 실행하지 않았다. 설치형 iOS 실기기 검증을 이번 리뷰로 증명하지 않는다. git diff --check는 exit0. 전체 suite 재실행, codex exec/review 또는 subagent 실행은 하지 않았다.

## 수정 순서

1. R1 provider별 exact 저장과 R4 bounded preview를 같은 저장 PR로 수정하고 실제 proxy/DB→UI 회귀 추가.
2. R2/R6 실행 provenance·thread 전환과 cache 정책 PR. macOS/Linux의 단위 및 stale PID/runtime 검증.
3. R3 최초 검증 projection 수용 PR. 시작 직후/워크스페이스 전환/동일본문 새 turn을 검증.
4. R5 shell 인자 parser PR. resume/fork optional prompt와 모든 기존 호환성을 검사.

제품 변경은 다음 사용자 수정 요청의 범위에서 진행한다. 이번 리뷰에서는 수정·릴리스·재시작·커밋·푸시 없음.
