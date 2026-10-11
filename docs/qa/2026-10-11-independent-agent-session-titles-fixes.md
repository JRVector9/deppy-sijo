# 독립 세션 제목 후속 결함 수정·검증 — 2026-10-11

Canonical source: `/Users/jr/Desktop/projects/deppy-sijo-performance`, base `799fae89` + 기존 working tree. 사용자 요청에 따라 3명(gpt-6.1-sol/xhigh)이 파일 소유권을 나눠 병렬 수정했고 root가 인터페이스와 변경 코드를 리뷰하고 최종 통합 검증을 완료했다. 후속 리뷰 R1~R6 및 같은 데이터 경로에서 추가로 재현한 경계 문제를 처리한다. 다른 작성자의 history/Git/resize 수정은 보존한다.

## 수정 내역

| 리뷰 | 수정 | 담당 |
| --- | --- | --- |
| R1 | pane/provider/native ID가 정확히 일치하는 지시 저장; 4 provider의 실제 수신 경로 연결 | title_storage_fix |
| R2 | 실행 소유권을 유지한 hook으로 Codex 대화 전환 수용; fast/full scan 캐시 교체와 PID 재사용 방어 | title_execution_fix |
| R3 | 검증된 첫 snapshot부터 제목 반영; 캐시 정리 뒤 동일 지시도 복구 | title_projection_fix |
| R4 | human transport 정규화 후 UTF-8 경계에서 256B 미리보기; 긴 원문은 그대로 유지 | title_storage_fix |
| R5 | 첫 positional 명령만 helper로 분류; resume/fork prompt와 옵션 값 구분, 정확한 argv 보존 | title_projection_fix |
| R6 | Linux raw start ticks와 Unix epoch 비교 제거; kernel PID/birth equality 검증 | title_execution_fix |

root/담당 리뷰에서 확인한 추가 경계: Grok camelCase/snake_case 이벤트, 경로 없는 exact-provider 지시, 24h 조회 TTL, 보존된 과거 prompt stamp, stamp 읽기/메모리 상한. 각각 실패 회귀로 확인 후 보완했다. Claude/Codex/Grok 전용 observe 스크립트는 provider를 명시하며 실제 settings/command 등록에도 연결했다. Kimi 및 generic legacy/cursor 경로는 유지한다.

## 실행 소유권 계약

공유 core `HookExecutionStamp`는 runtime token과 최대32개의 kernel PID/birth ancestor identity만 담는다. receiver는 ps/argv 추측 없이 현재 ancestry를 기록한다. macOS의 초·마이크로초와 Linux의 raw boot ticks는 각각 opaque process identity로 비교한다. Linux tick0도 유효한 identity이며 epoch로 변환하지 않는다.

binding과 prompt에 별도 stamp를 저장한다. SessionStart 등 일반 binding 갱신은 보존된 지시의 stamp를 갱신하지 않는다. app은 현재 runtime/kind/live execution과 일치하는 binding 및 prompt만 projection에 넣고, worker에도 VerifiedHookBinding으로 실행 정보를 유지한다. 명시적인 현재 지시가 path 없이 와도 exact pane/provider/native binding만 갱신한다. legacy unstamped row는 live hook으로 사용하지 않는다. durable pane의 기존 정상 복원 메타데이터 계약은 별도로 유지한다.

새 DB migration은 nullable stamp2개를 추가한다. JSON 각각4096B, duplicate/invalid identity 및 잘못된 runtime을 검사한다. SQL preflight row byte 계산과 snapshot retained heap 계산에 prompt·stamp를 포함한다. 실제 사용자 DB migration이나 설치된 shim 변경은 이 작업에서 실행하지 않았다.

## 담당별 검증 기록

- 저장: storage432 PASS/0FAIL, MCPunit108 PASS/0FAIL/1ignored. 실제 proxy→격리 DB→AgentStateJob 통합8 PASS. strict Clippy(storage/MCP alltargets -Dwarnings), fmt, diff check PASS.
- 실행: detector74 PASS/1ignored, worker18 PASS, proc_info4 PASS, core11 PASS. 실제 macOS kernel ancestry, Linux `/proc/stat` fixture, 격리된 valid rollout2개·실제 PID/birth·synthetic ps로 Codex resume/fork/new, fast/full scan, 다른 pane/runtime/PID reuse를 검사.
- projection/shim: independent_24 PASS 및 강화한 legacy stale DB 회귀1 PASS. agent_shim16 PASS(18개 exact argv 시나리오 및 실제 생성 스크립트의4 provider·경로 없는 JSON 전달 포함).
- RED: provider 저장/257B, 긴 wrapped 한글, 첫 DB snapshot, cache prune 뒤 동일 지시, Codex 대화 전환, Linux 시계, Grok 이벤트, path-free provenance, TTL 및 retained heap가 각각 수정 전에 실패했다. 성공 결과로 실패를 바꾸어 기록하지 않는다.

주요 로그는 `/private/tmp/deppy-title-storage-proxy-final-fixed-20261011.log`, `deppy-title-proxy-provider-pathfree-final-20261011.log`, `deppy-title-execution-fix-20261011.log`이다. root 최종 통합 gate 결과는 아래와 같다.

준비/중간 실패: 잘못된 package명, storage cfg(test) private open API, type collection annotation, migration pragma usize가 compile 준비를 막아 수정했다. SQL source-law 테스트는 wrapper가 아니라 실제 guarded implementation을 검사하도록 갱신했다. 이 과정의 실패는 행동 RED와 구분했다.

## 범위와 한계

소스 및 격리 테스트 검증이다. 현재 실행 앱0.10.1에 적용한 검증으로 주장하지 않는다. 실제 사용자 CLI의 대화 전환, Linux 호스트 E2E, 설치된 iOS PWA는 이 작업에서 실행하지 않았다. 하단 composer는 앞선 사용자 요청대로 조사·PR 계획만 유지한다. 별도 재시작/릴리스/커밋/푸시 요청은 없다.


## root 최종 통합 결과

실행 gate exit0. 검토한 제품 코드13파일의 SHA256은 fmt 직후부터 gate 완료까지 바뀌지 않았다. 새 wrapper/타입/API, provider producer→proxy→DB→Hooks, stamp 보존과 TTL, 캐시와 화면 projection의 경계를 root가 직접 리뷰했다. 발견했던6건과 추가로 재현한 경계 결함은 모두 소스에서 수정했다.

| 검사 | 결과 |
| --- | --- |
| 전체 App binary 단위 회귀 | 2845 PASS / 0 FAIL / 38 ignored |
| storage 전체 lib | 432 PASS / 0 FAIL |
| MCP 전체 단위 | 108 PASS / 0 FAIL / 1 ignored |
| 실제 proxy→DB→Hooks 통합 | 8 PASS / 0 FAIL |
| core 전체 | 11 PASS / 0 FAIL; doc tests0 |
| App 통합 경계4target | 39 PASS / 0 FAIL (4+5+15+15) |
| strict all-targets Clippy | PASS (App/storage/MCP/core, -D warnings) |
| cargo fmt all check / git diff --check | PASS |

중복을 빼면 **3443 PASS / 0 FAIL / 39 ignored**다. focused24/detector74/shim16 등은 전체App에 포함되므로 더하지 않는다. 기존 PWA Chromium9PASS는 이전 작업 기록이며 이번에 반복하지 않았다. 전체 workspace/ignored 검사나 live 사용자 CLI 검증으로 주장하지 않는다.

로그: `/private/tmp/deppy-title-orchestrator-final-20261011.log`. 테스트 source manifest: `/private/tmp/deppy-title-orchestrator-test-source-20261011.json`. 최종 source manifest: `/private/tmp/deppy-title-orchestrator-final-source-20261011.json`.

```sh
cd /Users/jr/Desktop/projects/deppy-sijo-performance
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py --batch '[["fmt","--all"],["test","-p","deppy-sijo","--bin","deppy-sijo","--","--test-threads=1"],["test","-p","storage","--lib","--","--test-threads=1"],["test","-p","mcp-proxy","--","--test-threads=1"],["test","-p","deppy-core"],["test","-p","deppy-sijo","--test","agent_state_boundary","--test","dotenv_launch_boundary","--test","lazy_bounded_worker","--test","logging_policy","--","--test-threads=1"],["clippy","-p","deppy-sijo","-p","storage","-p","mcp-proxy","-p","deppy-core","--all-targets","--","-D","warnings"],["fmt","--all","--","--check"]]'
git diff --check
```

제품 파일: `crates/app/src/{agent_detect.rs,agent_detect_worker.rs,proc_info.rs,app.rs,agent_shim.rs}`, `crates/core/{Cargo.toml,src/lib.rs,src/process_identity.rs}`, `crates/storage/src/db.rs`, `crates/mcp-proxy/src/{main.rs,hook_payload.rs}` 및 새 `tests/hook_task_prompt.rs`. Cargo.lock에는 공유 core의 libc 의존성만 추가했고 이전 버전 변경은 보존했다. source version0.10.1 유지; changed app artifact 전달은 수행하지 않았다. 다음 출시 요청 시 최신 shipped 버전보다 증가시키고 binary/bundle 확인이 필요하다.
