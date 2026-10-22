# 에이전트 터미널 표 — 구현 및 외부 자료 재검토

작업 브랜치는 `feat/agent-command-center-v2-20261020`, 기준은 `7584ed75`이다. PR 단위로 resize 전달 검사, application wheel 지원, 세 공급자 재현 harness를 분리했다. **검증된 입력 경로 개선은 구현했지만, 모든 터미널 과거 표가 재배치되는 문제 전체를 해결했다고 판정하지 않는다.** Claude classic 과거 표 고정은 남아 있고 사용자가 보고한 Codex의 정확한 사례는 재현되지 않았다.

## PR별 결과

| 그룹 | 변경 | 통합 커밋 / 상태 |
| --- | --- | --- |
| PR1 | 실제 egui pane → tail dispatcher → runtime → Session → PTY resize 검사, rapid drag/stale ACK/hidden/split/remote geometry 회귀 | `2e442a51`; 기존 resize 전달 결함은 재현되지 않아 제품 resize 알고리즘은 유지 |
| PR2 조사 | Claude2.1.305·2.1.307의 새 manual-mode fixture에서 classic/fullscreen 과거 표 및 한글 초안 비교 | `1364e722`; classic 고정 폭과 fullscreen 원문 재배치 구분 |
| PR2a | negotiated alternate-screen SGR wheel, 현재 mode/grid 재확인, 기존 PTY admission, prompt와 분리된 Navigation, wire35→36 | `3a0acd5a`, test-only `2001a888`, evidence `5f49065f`; native UI `3957e2b1` |
| PR2b | Claude renderer 자동 변경 | 적용하지 않음. preview·명시적 classic·실패 후 fallback을 존중할 신뢰할 capability/preference 판정이 없음 |
| PR3 | 격리된 Claude/Codex/Grok CLI harness, semantic/border analyzer, child 실패 전달, timeout/자식 프로세스 정리 | `8ca63036`; 12개 실제 사례 중5개 VALID/PASS,7개 INVALID를 구분 |

PR는 로컬 검토 가능한 커밋 그룹이다. 다른 Command Center 작업이 포함된 브랜치를 원격 PR로 게시하거나 main에 합치지 않았다.

## 제품 변경과 불변 조건

focused writable alternate pane에서 terminal 위의 수정키 없는 휠을 bounded typed `Wheel`로 보낸다. backend는 alternate screen + SGR1006 + 실제 mouse reporting이 모두 켜지고 표시 grid가 현재 grid와 일치할 때만 버튼64/65를 인코딩한다. 1-based 좌표와 최대32회 제한을 검증하고 오래된 grid, 미협상 mode, output backlog, 권한 회수 및 queue 포화를 기존 admission 경계에서 거부한다.

휠은 기존 PTY queue와 순서를 공유하되 내부 `Navigation`으로 기록한다. draft, 사용자 입력 sequence, accepted/submitted prompt 및 작업 완료 evidence를 변경하지 않는다. 기존 raw SGR를 일반 입력으로 전달하면 끝의 `M`이 초안으로 인식되는 문제를 실제 detector에서 확인했기 때문이다. native `Scroll`은 계속 화면 보기 전용이다. wire36은 예전35 peer를 명시적으로 거절하며 기존 Text/Key 직렬화는 유지한다.

선택·드래그·수정키는 native 경로, disabled/read-only/modal/비포커스 pane은 CLI 입력 차단을 유지한다. Shift 때문에 egui가 Y를 X로 바꾼 경우 native 세로 스크롤을 복원한다. trackpad 제스처 도중 수정키를 놓아도 종료까지 override를 보존한다. 임의 ANSI 행을 표로 추측해 재작성하지 않고 자동 Ctrl+L·프롬프트·renderer 재실행을 넣지 않는다.

## 완료 후 리서치와 잘못된 구현 교정

세 명의 독립 reviewer가 최종 integration source를 읽었고 root가 통합 상태를 확인했다. 추가로 확정된 제품 결함은 없었다. 휠 지원을 CLI 클릭·드래그 전체 mouse 지원으로 확대 해석하지 않는다.

1. **공식 renderer 계약:** [Claude fullscreen 문서](https://code.claude.com/docs/en/fullscreen)는 preview이며 환경변수, 설정, screen-reader 및 실패 후 fallback의 우선순위를 정의한다. 강제 환경변수/`--settings`를 일반 해결책으로 적용하는 제안은 채택하지 않았다. launcher는 변경하지 않았다. source renderer가 보유한 과거 표는 fullscreen에서 재배치되지만 classic의 VT 행을 의미 있는 표로 복구할 수 있다는 근거는 없다.
2. **공식 mouse 계약:** [xterm Patch411 명세](https://invisible-island.net/xterm/ctlseqs/ctlseqs.pdf)의 SGR1006, 버튼64/65, release packet 없음,1-based 좌표와 대조했다. 협상 없이 cursor-key/legacy-mouse로 추측하지 않는다. pixel mode1016을 지원한다고 주장하지 않는다.
3. **실제 egui 계약:** Cargo.lock의 egui0.36.1 `WheelState`와 비교해 마지막 이벤트 modifier만 보관하면 trackpad 제스처 중 Shift/Alt release에서 override가 사라지는 것을 실제 Start/Move 이벤트 테스트로 재현했다. 공개 raw event의 Start/Move-OR/End/idle 상태로 교정했으며 수정 후 통과했다. 처음 private `i.wheel` 접근은 컴파일에서 거절되어 제거했다.
4. **출력 종류 구분:** 설치 Codex0.163.0에 대응하는 [assistant history renderer](https://github.com/openai/codex/blob/rust-v0.163.0/codex-rs/tui/src/history_cell/messages.rs)와 [tool-output renderer](https://github.com/openai/codex/blob/rust-v0.163.0/codex-rs/tui/src/exec_cell/render.rs)를 비교했다. assistant Markdown 원문과 literal tool stdout의 차이는 실제 보고 사례를 조사할 가설이며, 사용자 사례의 원인으로 확정하지 않는다. [공개 Grok 소스](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-pager/src/scrollback/blocks/markdown_content.rs)의 SHA는 설치 build와 달라 동등성을 주장하지 않는다.

초기 Claude fixture의 auto permission metadata 관찰은 새 manual fixture와 분리해 정정했다. 42열 카드형 또는 잘린 이력은 analyzer의 INVALID이며 PASS로 계산하지 않는다. 자세한 원본은 [정제 조사](2026-10-22-claude-table-source-rendering.md), [backend 증거](2026-10-22-application-wheel-pr2a.md), [12-case matrix](../../scripts/qa/agent-terminal-table-resize/QA-2026-10-22.md)에 있다.

## 실제 검증

| 명령 / 경로 | 실제 결과 |
| --- | --- |
| 최종 root `cargo test --offline --locked -p deppy-sijo application_wheel_ -- --test-threads=1` | 9PASS/0FAIL; 실제 UI→tail dispatcher→worker→PTY bytes와 prompt evidence 없음 포함 |
| 독립 reviewer가 동일 app test binary의 `application_wheel_` 직접 실행 | 9PASS/0FAIL |
| 독립 reviewer가 동일 binary의 `resize` 직접 실행 | 31PASS/0FAIL |
| agent affected suites | 650PASS/5ignored; runtime428/session82/terminal140; own tree에는 PR3 integration target 미포함 |
| 최종 root `cargo test --offline --locked -p runtime -p session -p terminal -- --test-threads=1` | **652PASS/0FAIL/6ignored**; runtime428/session82lib+2integration/terminal140; filtered/ignored 중복 합산 안 함 |
| agent terminal/session/runtime strict all-target Clippy | exit0;4개 test-only assertion lint 교정 후 |
| root Python unittest discover | 18PASS/0FAIL |
| root installed Codex inline-history 격리 runner | VALID/visibilityPASS/resizePASS/semanticsPASS;80→42→120→80에서78→40→118→78열 |
| root Claude evidence assertion | 24stage 및 hash/manual-modal 판정 PASS |
| root backend evidence verifier | 13로그/8제품 source hash PASS |
| 최종 root `cargo test --offline --locked -p deppy-sijo -- --test-threads=1` | **3,557PASS/0FAIL/43ignored**; unit3518 + integration39; 하위 자식 재실행1개 중복 합산 안 함 |
| 최종 root `cargo clippy --offline --locked -p deppy-sijo --all-targets -- -D warnings` | exit0 |
| root `cargo test --offline --locked -p runtime resize -- --test-threads=1` | 12PASS/0FAIL |
| root workspace fmt / diff check | exit0 / exit0; 최종 문서·커밋 후 재확인 |

[통합 증거 manifest](assets/table-resize-integration-20261022/manifest.json)에 root·독립 reviewer 로그11개, 최종12개 source hash 및 격리 Codex 출력을 보관했다. 원본 ANSI/viewport padding은 gzip으로 그대로 보존하고 읽기용 화면에서는 우측 공백·마지막 빈 행만 정규화했다. 첫 evidence staged diff check는 원본 공백 때문에 실패했으며 제품 소스 문제가 아니다. root 통합 결과에는 agent tree에 없던 PR3 session integration2PASS/1ignored가 추가되어652PASS/6ignored로 집계했다. 압축 원본과 두 hash로 보존한 뒤 다시 검사했다.

9번째 UI→PTY fixture의 초기 실패는 40px 한 이벤트의 첫 smoothing frame이12.7px로 한 행보다 작은 residual0.995였기 때문이다. 한 frame 안에서 한 행을 넘는80px로 fixture를 고쳤다. 이 실패를 제품 결함 RED로 세지 않는다. modifier regression과 초기 alternate wheel native-only routing은 실제 행동 회귀 증거와 구분한다.

## 남은 범위와 배포 상태

- **Claude classic 과거 표 고정:** 여전히 존재한다. 새 typed wheel은 CLI/사용자가 이미 선택한 fullscreen의 이력 탐색 기반을 제공하며 자동 모드 전환은 하지 않는다.
- **사용자의 정확한 Codex 사례:** 동일한 표·출력 경로/버전 재현이 필요하다. 합성 테스트 PASS는 실제 사례 해결 증거가 아니다.
- 실제 데스크톱 포인터/OS clipboard, 물리 모바일/원격 기기, 기존 사용자 세션 전체 E2E는 수행하지 않았다. source-level selection/native path, remote authority/geometry 회귀와 구분한다.
- 앱 실행·정지·재시작, 사용자 프롬프트·실제 키 복사·유료 호출, 릴리스 빌드 배포·push/main merge는 하지 않았다. 소스0.15.1의 개발 테스트이며 제품 릴리스 완료가 아니다. 추후 배포는 현재 지침대로 더 높은 버전과 artifact version 검증이 필요하다.
