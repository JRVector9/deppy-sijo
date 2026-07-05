# Rust AI Agent Workspace — Orchestrator Dispatch Prompt Pack

작성일: 2026-07-04  
용도: 오케스트레이터가 Review Agent / Build Agent / Build PR Review Agent에게 작업을 위임할 때 사용하는 프롬프트 조립 문서  
상태: 최종 운영용

---

## 0. 왜 이 문서가 필요한가

Review PR 문서에는 PR-R00~PR-R09 각각의 리뷰 범위가 정의되어 있다.  
하지만 오케스트레이터가 실제로 서브에이전트에게 작업을 위임할 때는 아래 3가지를 **항상 같이 전달**해야 한다.

```text
1. 공통 리뷰 프롬프트
2. 해당 PR 전용 프롬프트
3. 반드시 읽어야 하는 설계/개선 문서 목록
```

즉, 리뷰 에이전트에게 단순히 다음만 주면 부족하다.

```text
PR-R03 해줘
```

반드시 이렇게 줘야 한다.

```text
너는 기존 구현 리뷰 전용 에이전트다.
아래 문서를 먼저 읽어라.
공통 불변 원칙은 이것이다.
이번 PR은 PR-R03이다.
이번 리뷰 범위는 이것이다.
산출물은 docs/review/PR-R03-...md로 작성하라.
```

---

## 1. 오케스트레이터 기본 역할

오케스트레이터는 직접 코드를 리뷰/수정하지 않는다.  
오케스트레이터는 작업을 나누고, 에이전트별 입력을 정확히 구성하고, 산출물을 수집한다.

오케스트레이터의 역할:

```text
- Review PR 번호 선택
- 해당 PR 전용 프롬프트 선택
- 공통 리뷰 프롬프트와 결합
- 필요한 문서 목록 첨부
- 산출물 경로 지정
- 결과 findings 수집
- Critical / High finding을 Build Track으로 넘김
- Build PR 완료 후 Review Agent에게 재검토 요청
```

---

## 2. 필수 문서 목록

모든 Review Agent에게 아래 문서를 반드시 읽게 한다.

```text
1. 최종 설계 문서
   ai_agent_workspace_final_architecture_v2_6_FOLDER_TREE.md

2. Persistence/store 개선 문서
   ai_agent_workspace_v2_8_persistence_store_improvement_FINAL.md

3. Review/Build 분리 PR 문서
   ai_agent_workspace_v3_1_review_build_split_pr_plan.md

4. Review/Build Prompt Pack
   ai_agent_workspace_review_build_prompt_pack.md
```

필요에 따라 추가 문서를 붙인다.

```text
- 특정 PR 관련 findings
- 관련 설계 section
- 이전 Review Agent 산출물
- Build PR diff
- test output
```

---

## 3. 오케스트레이터가 Review Agent에게 보내는 최종 프롬프트 템플릿

아래 템플릿을 그대로 사용한다.

```text
너는 Rust AI Agent Workspace 프로젝트의 기존 구현 리뷰 전용 서브에이전트다.

이번 작업은 신규 개발이 아니라 기존 구현 검토다.
코드를 크게 수정하지 말고, 현재 구현이 설계 문서와 맞는지 검토하라.

반드시 먼저 아래 문서를 읽어라.

1. 최종 설계 문서:
   ai_agent_workspace_final_architecture_v2_6_FOLDER_TREE.md

2. Persistence/store 개선 문서:
   ai_agent_workspace_v2_8_persistence_store_improvement_FINAL.md

3. Review/Build 분리 PR 문서:
   ai_agent_workspace_v3_1_review_build_split_pr_plan.md

4. Review/Build Prompt Pack:
   ai_agent_workspace_review_build_prompt_pack.md

이번 리뷰 PR 번호:
  <REVIEW_PR_ID>

이번 리뷰 PR 이름:
  <REVIEW_PR_TITLE>

이번 리뷰 범위:
  <REVIEW_SCOPE>

공통 불변 원칙:
1. UI는 RuntimeClient만 본다.
2. Runtime은 mux / session / env / terminal / pty를 조율한다.
3. Active pane만 render한다.
4. Hidden workspace/session은 log/status만 처리한다.
5. TerminalViewportSnapshot은 visible pane에만 만든다.
6. Raw log 평문은 기본 저장하지 않는다.
7. Session은 secret store를 직접 모른다.
8. folder tree / drag-and-drop / terminal copy-paste UX는 회귀 금지다.
9. mcp/audit/persist/storage 계층에서 crate 순환 의존을 만들지 않는다.
10. secret/env/API key는 DB/config/log/export에 평문 저장하지 않는다.

금지:
- 대형 리팩터링 금지
- 신규 기능 구현 금지
- public API 대폭 변경 금지
- DB schema 변경 금지
- 설계 문서와 무관한 취향성 수정 금지
- 발견 없이 “문제 없음”만 쓰는 것 금지

허용:
- 읽기 중심 코드 리뷰
- 테스트 추가 제안
- smoke test 추가 제안
- 작은 확인용 스크립트 추가 제안
- 문제 재현 절차 작성
- 위험도 분류
- 수정 방향 제안

산출물:
  docs/review/<REVIEW_PR_ID>-findings.md

산출물 형식:
# <REVIEW_PR_ID> Findings

## Summary
- 전체 판정: Pass / Pass with Issues / Block
- Critical:
- High:
- Medium:
- Low:

## Scope Reviewed
- 검토한 파일/모듈
- 실행한 명령
- 확인한 테스트

## Findings

### Finding 1
Severity: Critical / High / Medium / Low
Area:
Files:
Evidence:
Why it matters:
Reproduction:
Suggested fix:
Suggested test:

## Regression Risks
## Recommended Build PRs
## Open Questions

리뷰 완료 전 실행 가능한 경우 아래 명령을 실행하라.

cargo check --workspace --all-targets
cargo test --workspace --no-run

의존성 리뷰 PR이면 추가로 실행하라.

cargo tree --workspace --edges normal,build
cargo tree --workspace --edges normal,build,dev
cargo metadata --format-version 1 > target/cargo-metadata.json

아래는 이번 PR 전용 지시다.

<PR_SPECIFIC_PROMPT>
```

---

## 4. PR별 오케스트레이터 입력값

## 4.1 PR-R00

```text
REVIEW_PR_ID:
  PR-R00

REVIEW_PR_TITLE:
  Current Implementation Inventory

REVIEW_SCOPE:
  현재 구현된 기능의 inventory를 작성한다.
  pane 단위 동작, workspace/pane/session 관계, folder tree, folder tree → terminal path drag/drop,
  terminal selection/copy/paste, terminal internal drag/drop paste, RuntimeClient boundary,
  redacted log 경로를 검토하고 회귀 금지 baseline을 문서화한다.

PR_SPECIFIC_PROMPT:
  PR-R00 — Current Implementation Inventory 전용 프롬프트를 사용한다.

산출물:
  docs/review/PR-R00-current-implementation-inventory.md
```

## 4.2 PR-R01

```text
REVIEW_PR_ID:
  PR-R01

REVIEW_PR_TITLE:
  Boundary Violation Review

REVIEW_SCOPE:
  UI/runtime/terminal/pty/secret 경계 위반을 찾는다.
  app crate가 alacritty_terminal, portable-pty, SecretStore를 직접 참조하는지 확인한다.
  UI가 SessionManager를 직접 호출하는지 확인한다.
  session crate가 secret store를 직접 참조하는지 확인한다.

PR_SPECIFIC_PROMPT:
  PR-R01 — Boundary Violation Review 전용 프롬프트를 사용한다.

산출물:
  docs/review/PR-R01-boundary-violation-findings.md
```

## 4.3 PR-R02

```text
REVIEW_PR_ID:
  PR-R02

REVIEW_PR_TITLE:
  Dependency Graph / Store Cycle Review

REVIEW_SCOPE:
  crate 순환 의존과 store crate 분리 상태를 검토한다.
  storage → mcp, mcp → storage, storage → audit, audit → storage,
  storage → persist, persist → storage를 확인한다.
  storage-core, mcp-model, mcp-runtime, mcp-store, audit-store, mux-store,
  env-store, session-store, persist orchestrator 상태를 확인한다.

PR_SPECIFIC_PROMPT:
  PR-R02 — Dependency Graph / Store Cycle Review 전용 프롬프트를 사용한다.

추가 실행 명령:
  cargo tree --workspace --edges normal,build
  cargo tree --workspace --edges normal,build,dev
  cargo metadata --format-version 1 > target/cargo-metadata.json

산출물:
  docs/review/PR-R02-dependency-graph-store-cycle-findings.md
```

## 4.4 PR-R03

```text
REVIEW_PR_ID:
  PR-R03

REVIEW_PR_TITLE:
  Pane / Mux / Visibility Review

REVIEW_SCOPE:
  pane, mux, visibility, rendering resource policy를 검토한다.
  hidden pane에서 TerminalViewportSnapshot이 생성되는지 확인한다.
  hidden pane glyph layout 여부를 확인한다.
  active pane만 render되는지 확인한다.
  Pane과 Session이 분리되어 있는지 확인한다.

PR_SPECIFIC_PROMPT:
  PR-R03 — Pane / Mux / Visibility Review 전용 프롬프트를 사용한다.

산출물:
  docs/review/PR-R03-pane-mux-visibility-findings.md
```

## 4.5 PR-R04

```text
REVIEW_PR_ID:
  PR-R04

REVIEW_PR_TITLE:
  Folder Tree / Drag-and-Drop Review

REVIEW_SCOPE:
  folder tree와 drag-and-drop path insert 기능을 검토한다.
  folder tree lazy loading, 큰 workspace freeze 가능성, ignore rules,
  path quoting, multi-path drop, CJK/emoji path, drop 후 자동 실행 여부를 확인한다.

PR_SPECIFIC_PROMPT:
  PR-R04 — Folder Tree / Drag-and-Drop Review 전용 프롬프트를 사용한다.

산출물:
  docs/review/PR-R04-folder-tree-dnd-findings.md
```

## 4.6 PR-R05

```text
REVIEW_PR_ID:
  PR-R05

REVIEW_PR_TITLE:
  Terminal Clipboard / Paste / CJK Review

REVIEW_SCOPE:
  terminal selection, copy, paste, drag/drop paste, CJK/IME 처리를 검토한다.
  bracketed paste, grapheme boundary, CJK wide char, Japanese/Chinese/Korean/emoji path,
  clipboard failure handling을 확인한다.

PR_SPECIFIC_PROMPT:
  PR-R05 — Terminal Clipboard / Paste / CJK Review 전용 프롬프트를 사용한다.

산출물:
  docs/review/PR-R05-terminal-clipboard-paste-cjk-findings.md
```

## 4.7 PR-R06

```text
REVIEW_PR_ID:
  PR-R06

REVIEW_PR_TITLE:
  Redaction / Secret / Env Leak Review

REVIEW_SCOPE:
  secret, env, API key, token 유출 가능성을 검토한다.
  DB/config/log/export/crash/debug dump/clipboard/env diff preview/MCP audit에 secret이 남는지 확인한다.

PR_SPECIFIC_PROMPT:
  PR-R06 — Redaction / Secret / Env Leak Review 전용 프롬프트를 사용한다.

산출물:
  docs/review/PR-R06-redaction-secret-env-leak-findings.md
```

## 4.8 PR-R07

```text
REVIEW_PR_ID:
  PR-R07

REVIEW_PR_TITLE:
  MCP / Permission / Audit Review

REVIEW_SCOPE:
  MCP tool 권한 승인, audit log, schema hash, stdio protocol strictness를 검토한다.
  tool 호출 전 approval, schema 변경 시 재승인, input_redacted_json, input_encrypted_blob,
  stdout valid MCP message only, stderr redaction을 확인한다.

PR_SPECIFIC_PROMPT:
  PR-R07 — MCP / Permission / Audit Review 전용 프롬프트를 사용한다.

산출물:
  docs/review/PR-R07-mcp-permission-audit-findings.md
```

## 4.9 PR-R08

```text
REVIEW_PR_ID:
  PR-R08

REVIEW_PR_TITLE:
  Resource / Performance Review

REVIEW_SCOPE:
  RSS, RAM, CPU, repaint, queue, file watcher, terminal cache 비용을 검토한다.
  빈 앱, workspace 5개, pane 20개, session 10개, hidden session 10개,
  대량 output 10MB/min, folder tree 100k files 시나리오를 검토한다.

PR_SPECIFIC_PROMPT:
  PR-R08 — Resource / Performance Review 전용 프롬프트를 사용한다.

산출물:
  docs/review/PR-R08-resource-performance-findings.md
```

## 4.10 PR-R09

```text
REVIEW_PR_ID:
  PR-R09

REVIEW_PR_TITLE:
  I18n Readiness Review

REVIEW_SCOPE:
  다국어 지원 준비 상태를 검토한다.
  en-US, ja-JP, zh-Hans, zh-Hant를 필수 locale로 보고,
  하드코딩 UI 문자열, RuntimeEvent 문자열 직접 전달, NotificationEvent 문자열 직접 전달,
  error string 직접 표시, terminal output 번역 여부, CJK UI overflow 가능성을 확인한다.

PR_SPECIFIC_PROMPT:
  PR-R09 — I18n Readiness Review 전용 프롬프트를 사용한다.

산출물:
  docs/review/PR-R09-i18n-readiness-findings.md
```

---

## 5. 오케스트레이터 실행 예시

## 5.1 PR-R02 위임 예시

```text
[공통 리뷰 프롬프트 전체]

이번 리뷰 PR 번호:
  PR-R02

이번 리뷰 PR 이름:
  Dependency Graph / Store Cycle Review

이번 리뷰 범위:
  crate 순환 의존과 store crate 분리 상태를 검토한다.
  storage → mcp, mcp → storage, storage → audit, audit → storage,
  storage → persist, persist → storage를 확인한다.
  storage-core, mcp-model, mcp-runtime, mcp-store, audit-store, mux-store,
  env-store, session-store, persist orchestrator 상태를 확인한다.

아래는 이번 PR 전용 지시다.

[PR-R02 전용 프롬프트 전체]
```

## 5.2 PR-R04 위임 예시

```text
[공통 리뷰 프롬프트 전체]

이번 리뷰 PR 번호:
  PR-R04

이번 리뷰 PR 이름:
  Folder Tree / Drag-and-Drop Review

이번 리뷰 범위:
  folder tree와 drag-and-drop path insert 기능을 검토한다.
  folder tree lazy loading, 큰 workspace freeze 가능성, ignore rules,
  path quoting, multi-path drop, CJK/emoji path, drop 후 자동 실행 여부를 확인한다.

아래는 이번 PR 전용 지시다.

[PR-R04 전용 프롬프트 전체]
```

---

## 6. Build Track 위임 템플릿

Review Agent가 findings를 만든 뒤, Build Agent에게는 아래 템플릿을 사용한다.

```text
너는 Build Track 구현 에이전트다.

입력:
- Review Track findings.md
- 관련 설계 문서
- 해당 Build PR acceptance criteria

목표:
- Review findings를 기반으로 필요한 구현만 한다.
- unrelated refactor는 하지 않는다.
- 기존 pane/folder tree/DnD/copy-paste UX를 깨지 않는다.
- 테스트와 함께 제출한다.

반드시 지킬 것:
1. UI는 RuntimeClient만 본다.
2. TerminalBackend 구현체를 UI에 노출하지 않는다.
3. raw log 평문 기본 저장 금지
4. session은 secret store 직접 참조 금지
5. env는 storage 구현체 직접 참조 금지
6. hidden pane은 render/snapshot 생성 금지
7. active pane만 paint
8. store crate cycle을 만들지 않는다.
9. i18n 대상 문자열은 message_id + args 구조를 따른다.
10. CJK/IME/clipboard/DnD 회귀 테스트를 추가한다.

작업 방식:
1. 먼저 findings.md를 요약한다.
2. Critical/High/Medium/Low를 구분한다.
3. 이번 Build PR에서 해결할 범위를 명확히 제한한다.
4. 설계 문서와 충돌하는 부분이 있으면 구현 전에 보고한다.
5. 코드 변경 후 테스트를 추가한다.
6. 변경 범위 밖의 리팩터링은 하지 않는다.

산출물:
- 코드 변경
- 테스트
- 변경 요약
- risk notes
- rollout/rollback notes
- 후속 review 요청 목록
```

---

## 7. Build PR 리뷰 위임 템플릿

Build Agent가 구현을 마친 뒤, Build PR Review Agent에게 아래 템플릿을 사용한다.

```text
너는 Build PR 리뷰 전용 에이전트다.

목표:
- 구현 PR이 설계와 acceptance criteria를 만족하는지 검토한다.
- 특히 회귀, 리소스, 보안, i18n, CJK, DnD, terminal boundary를 본다.

반드시 먼저 확인:
1. PR scope가 문서와 일치하는가
2. Review findings가 실제로 해결되었는가
3. 기존 UX를 깨지 않는가
4. RuntimeClient boundary를 지키는가
5. crate 의존 순환을 만들지 않는가
6. hidden pane render/snapshot 금지를 지키는가
7. redacted log 기본값을 지키는가
8. secret/env/API key가 누출되지 않는가
9. CJK/IME/clipboard/drag-drop 테스트가 있는가
10. RSS/CPU/queue/backpressure 영향이 있는가
11. 실패 시 rollback이 가능한가

검토 명령:
cargo check --workspace --all-targets
cargo test --workspace --no-run
cargo tree --workspace --edges normal,build
cargo tree --workspace --edges normal,build,dev

PR 성격에 따라 추가:
- dependency 관련 PR:
  cargo metadata --format-version 1 > target/cargo-metadata.json

- performance 관련 PR:
  cargo xtask perf-smoke

- security 관련 PR:
  cargo xtask security-scan

- i18n 관련 PR:
  cargo xtask i18n-check

판정:
- Approve
- Request Changes
- Block

산출물 형식:

# Build PR Review

## Verdict
Approve / Request Changes / Block

## Summary

## Acceptance Criteria Check
- [ ] criteria 1
- [ ] criteria 2

## Findings

### Finding 1
Severity:
Area:
Evidence:
Why it matters:
Required fix:

## Regression Risks

## Required Follow-up

## Final Notes

주의:
- 반드시 재현 가능한 근거를 포함한다.
- "좋아 보임" 같은 추상 리뷰 금지.
- Block 판정은 Critical 또는 unresolved High finding이 있을 때만 사용.
```

---

## 8. 최종 운영 순서

```text
1. 오케스트레이터가 Review Agent에게 공통 리뷰 프롬프트 + PR 전용 프롬프트를 함께 전달한다.
2. Review Agent는 docs/review/PR-Rxx-*.md를 생성한다.
3. 오케스트레이터는 findings를 모아 Critical / High / Medium / Low로 정리한다.
4. Critical/High만 먼저 Build Track으로 넘긴다.
5. Build Agent는 Build Track 위임 템플릿으로 구현한다.
6. Build PR Review Agent는 Build PR 리뷰 템플릿으로 검토한다.
7. Approve 후 merge한다.
```

---

## 9. 최종 판단

오케스트레이터는 단순히 PR 번호만 넘기면 안 된다.

반드시 다음을 함께 넘긴다.

```text
- 공통 리뷰 프롬프트
- 해당 PR 전용 프롬프트
- 읽어야 할 문서 목록
- 산출물 경로
- 실행해야 할 명령
- 금지사항
```

이 문서는 그 조립 방식을 정의한다.
