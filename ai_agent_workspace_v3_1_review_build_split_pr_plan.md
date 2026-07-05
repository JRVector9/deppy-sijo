# Rust AI Agent Workspace — 최종 PR 재정리 문서 v3.1

작성일: 2026-07-04  
대상: v2.5/v2.6/v2.8 기반 구현 완료 코드베이스  
문서 성격: 기존 구현 검토와 신규 개발을 분리하기 위한 최종 PR 운영 문서  
주의: 이 문서는 특정 모델명, 토큰 운용 방식, 내부 실행 도구명을 포함하지 않는다.

---

## 0. 목적

현재 프로젝트는 이미 다음 기능들이 구현된 상태다.

```text
- Pane 단위 동작
- 여러 workspace / pane 기반 작업
- 폴더 트리 표시
- 폴더 트리에서 drag & drop으로 경로를 터미널에 삽입
- 터미널 내부 drag/drop 기반 copy/paste
- UI/runtime 분리 구조
- persistence crate cycle 개선 방향 반영
- redacted log / secret store / project env / mux 구조
```

본 문서는 기존 구현을 재검토하는 작업과 신규 개발 작업을 분리한다.

```text
Review Track:
  이미 구현된 기능을 검토하고 회귀/성능/보안/의존성 문제를 찾는다.

Build Track:
  검토 결과를 바탕으로 신규 개발 또는 하드닝을 수행한다.
```

---

## 1. 유지해야 할 핵심 불변 원칙

아래 원칙은 모든 Review PR과 Build PR에서 위반 금지다.

```text
1. UI는 RuntimeClient만 본다.
2. Runtime은 mux / session / env / terminal / pty를 조율한다.
3. Active pane만 render한다.
4. Hidden workspace/session은 log/status만 처리한다.
5. TerminalViewportSnapshot은 visible pane에만 만든다.
6. Raw log 평문은 기본 저장하지 않는다.
7. Session은 secret store를 직접 모른다.
8. folder tree / DnD / terminal copy-paste UX는 회귀 금지다.
9. mcp/audit/persist/storage 계층에서 crate 순환 의존을 만들지 않는다.
10. secret/env/API key는 DB/config/log/export에 평문 저장하지 않는다.
```

---

## 2. PR 운영 원칙

## 2.1 Review Track

Review Track은 기존 코드를 검토한다.

허용:

```text
- 테스트 추가
- smoke test 추가
- 문서화
- 계측 코드 추가
- 작은 bug fix
- boundary violation detection
- perf/security/i18n 진단
```

금지:

```text
- 대형 리팩터링
- 신규 기능 추가
- public API 대폭 변경
- crate 의존 방향 변경
- DB schema 변경
```

Review Track 산출물:

```text
- findings.md
- risk level: Critical / High / Medium / Low
- affected files
- reproduction steps
- suggested fix
- regression test proposal
```

## 2.2 Build Track

Build Track은 실제 개발을 수행한다.

허용:

```text
- 신규 기능
- 구조 개선
- 성능 최적화
- i18n 구현
- store crate 분리
- resource monitor
- folder tree hardening
- terminal clipboard/IME/CJK 개선
```

필수:

```text
- Review Track에서 발견한 이슈를 반영
- PR별 acceptance criteria 충족
- cargo check/test/fmt/clippy 통과
- PR이 끝날 때 Review Track 재검토 요청
```

---

## 3. 전체 PR 흐름

```text
Phase 0:
  Baseline freeze

Phase 1:
  Review Track으로 현재 구현 검토

Phase 2:
  Critical/High 문제 Build Track에서 수정

Phase 3:
  신규 개발/하드닝 PR 진행

Phase 4:
  Review Track으로 merge 전 재검토

Phase 5:
  Performance/Security/I18n release gate
```

---

# 4. Review Track PR

## PR-R00 — Current Implementation Inventory

### 목표

현재 구현된 기능을 기준선으로 고정한다.

### 검토 대상

```text
- Pane 동작
- Workspace / pane / session 관계
- Folder tree
- Folder tree → terminal path DnD
- Terminal selection / copy / paste
- Terminal internal DnD paste
- RuntimeClient boundary
- Redacted log 경로
```

### 산출물

```text
docs/review/current-implementation-inventory.md
```

### 완료 기준

```text
- 현재 동작 목록 작성
- 회귀 금지 기능 목록 작성
- 주요 smoke test 명령 정리
```

---

## PR-R01 — Boundary Violation Review

### 목표

UI/runtime/terminal/pty/secret 경계 위반을 찾는다.

### 검사

```text
- app crate가 alacritty_terminal 직접 참조 여부
- app crate가 portable-pty 직접 참조 여부
- app crate가 SecretStore 직접 호출 여부
- UI가 SessionManager 직접 호출 여부
- session crate가 secret store 직접 참조 여부
```

### 산출물

```text
docs/review/boundary-findings.md
```

### 완료 기준

```text
- 위반 파일 목록
- 심각도 분류
- 수정 제안
- xtask check-boundary 제안
```

---

## PR-R02 — Dependency Graph / Store Cycle Review

### 목표

crate 순환 의존 위험을 검토한다.

### 검사 명령

```bash
cargo check --workspace --all-targets
cargo test --workspace --no-run
cargo tree --workspace --edges normal,build
cargo tree --workspace --edges normal,build,dev
cargo metadata --format-version 1 > target/cargo-metadata.json
```

### 중점

```text
- storage → mcp
- mcp → storage
- storage → audit
- audit → storage
- storage → persist
- persist → storage
- mux-store / mux-model / persist 의존 방향
```

### 산출물

```text
docs/review/dependency-graph-findings.md
```

---

## PR-R03 — Pane / Mux / Visibility Review

### 목표

pane 단위 동작과 리소스 정책이 제대로 지켜지는지 확인한다.

### 검사

```text
- hidden pane에서 TerminalViewportSnapshot 생성 여부
- hidden pane glyph layout 여부
- active pane만 render되는지
- pane/session 분리가 지켜지는지
- layout source of truth가 mux인지
```

### 산출물

```text
docs/review/pane-mux-findings.md
```

---

## PR-R04 — Folder Tree / Drag-and-Drop Review

### 목표

폴더 트리와 DnD 경로 삽입 기능을 검토한다.

### 검사

```text
- folder tree lazy load 여부
- 큰 workspace에서 freeze 여부
- ignore rules 적용 여부
- folder/file drop 시 shell별 quoting
- path에 공백/CJK/emoji 포함 시 동작
- drop 후 자동 실행 여부
```

### 회귀 금지

```text
- drag path insert
- multi-path insert
- path quoting
- no auto-execute by default
```

### 산출물

```text
docs/review/folder-tree-dnd-findings.md
```

---

## PR-R05 — Terminal Clipboard / Paste / CJK Review

### 목표

터미널 selection/copy/paste/IME/CJK 처리를 검토한다.

### 검사

```text
- bracketed paste
- terminal selection
- CJK wide char
- Japanese path
- Chinese Simplified path
- Chinese Traditional path
- emoji path
- clipboard failure handling
```

### 산출물

```text
docs/review/terminal-clipboard-cjk-findings.md
```

---

## PR-R06 — Redaction / Secret / Env Leak Review

### 목표

secret/env/API key 유출 가능성을 검토한다.

### 검사

```text
- DB에 secret 평문 저장 여부
- config에 secret 평문 저장 여부
- logs에 secret 평문 저장 여부
- export에 secret 평문 포함 여부
- clipboard에 secret 자동 복사 여부
- crash/debug dump에 secret 포함 여부
- env diff preview에서 secret 마스킹 여부
```

### 산출물

```text
docs/review/secret-redaction-findings.md
```

---

## PR-R07 — MCP / Permission / Audit Review

### 목표

MCP tool 승인과 audit log 보안을 검토한다.

### 검사

```text
- tool 호출 전 approval 여부
- schema hash 변경 시 재승인 여부
- input_redacted_json 사용 여부
- input_encrypted_blob 기본 비활성 여부
- stdout valid MCP message만 허용 여부
- stderr redaction 여부
```

### 산출물

```text
docs/review/mcp-permission-audit-findings.md
```

---

## PR-R08 — Resource / Performance Review

### 목표

RSS/CPU/RAM/paint 비용을 검토한다.

### 측정 시나리오

```text
- 빈 앱
- workspace 5개
- pane 20개
- session 10개
- hidden session 10개
- 대량 output 10MB/min
- folder tree 100k files
```

### 산출물

```text
docs/review/performance-findings.md
```

---

## PR-R09 — I18n Readiness Review

### 목표

다국어 지원 준비 상태를 검토한다.

### 검사

```text
- 하드코딩 UI 문자열
- RuntimeEvent가 문자열 직접 전달하는지
- NotificationEvent가 message_id + args인지
- terminal output을 번역하려는 코드가 있는지
- CJK UI overflow 위험
```

### 산출물

```text
docs/review/i18n-readiness-findings.md
```

---

# 5. Build Track PR

## PR-B00 — Fix Critical Boundary Violations

### 입력

```text
PR-R01 findings
```

### 목표

Critical/High boundary violation 수정.

### 완료 기준

```text
- UI는 RuntimeClient만 사용
- terminal/pty/secret 구현체 UI 노출 없음
- xtask check-boundary 통과
```

---

## PR-B01 — Store Crate Cycle Fix

### 입력

```text
PR-R02 findings
```

### 목표

crate 순환 의존을 제거한다.

### 방향

```text
storage-core
mcp-model
mcp-runtime
mcp-store
audit-model
audit-store
mux-model
mux-store
env-store
session-store
persist
```

### 완료 기준

```text
- mcp → storage 직접 의존 없음
- storage → mcp-runtime 직접 의존 없음
- mcp-store → mcp-model
- mcp-runtime → mcp-model
- cargo check --workspace 통과
```

---

## PR-B02 — Pane Resource Guard

### 입력

```text
PR-R03 findings
```

### 목표

active/visible pane만 render되도록 강제.

### 완료 기준

```text
- hidden pane snapshot 생성 0회
- hidden pane glyph layout 0회
- active pane만 paint
```

---

## PR-B03 — Folder Tree Scalability & DnD Hardening

### 입력

```text
PR-R04 findings
```

### 목표

폴더 트리와 DnD 경로 삽입 안정화.

### 완료 기준

```text
- 100k file workspace에서 freeze 없음
- CJK/emoji/space path insert 정상
- shell별 quoting
- drop 후 자동 실행 금지
```

---

## PR-B04 — Terminal Clipboard / Paste / CJK Hardening

### 입력

```text
PR-R05 findings
```

### 목표

터미널 copy/paste/selection/IME/CJK 안정화.

### 완료 기준

```text
- bracketed paste 정상
- 일본어/중국어/영어 selection/copy 정상
- wide char cursor 정상
- IME composition smoke test 통과
```

---

## PR-B05 — Redaction Pipeline Hardening

### 입력

```text
PR-R06 findings
```

### 목표

secret/env/API key 유출 방지 강화.

### 완료 기준

```text
- chunk boundary fixture 통과
- ANSI-inserted secret fixture 통과
- base64/url/json escaped fixture 통과
- DB/config/log/export secret scan 통과
```

---

## PR-B06 — MCP Permission / Audit Hardening

### 입력

```text
PR-R07 findings
```

### 목표

MCP tool 승인과 audit 보안 강화.

### 완료 기준

```text
- approval dialog 필수
- schema hash 변경 시 재승인
- input_redacted_json 기본
- encrypted blob 선택
```

---

## PR-B07 — Process Resource Monitor

### 입력

```text
PR-R08 findings
```

### 목표

workspace/session별 CPU/RAM/RSS를 표시한다.

### 완료 기준

```text
- session별 process tree 표시
- workspace별 RSS aggregation
- high CPU/RAM warning
- kill/restart/suspend 가능
```

---

## PR-B08 — Workspace Auto Suspend

### 입력

```text
PR-R08 findings
```

### 목표

여러 workspace 장기 실행 시 메모리 증가를 제한한다.

### 완료 기준

```text
- Active/Warm/Suspended/Closed 상태 전환
- Suspended workspace snapshot 생성 0회
- 복귀 시 status/log tail 빠른 복원
```

---

## PR-B09 — Terminal Cache Budget Manager

### 입력

```text
PR-R08 findings
```

### 목표

terminal cache/RSS 증가 제한.

### 완료 기준

```text
- global terminal cache budget
- hidden/archived cache drop
- visible session 우선
```

---

## PR-B10 — Output Pipeline Backpressure

### 입력

```text
PR-R08 findings
```

### 목표

대량 출력에서도 UI 안정성 유지.

### 완료 기준

```text
- bounded output queue
- log writer 보존 우선
- render event drop/coalesce 가능
- backpressure badge 표시
```

---

## PR-B11 — I18n Infrastructure

### 입력

```text
PR-R09 findings
```

### 목표

다국어 기반 추가.

### 필수 locale

```text
en-US
ja-JP
zh-Hans
zh-Hant
```

선택 locale:

```text
ko-KR
```

### 완료 기준

```text
- crates/i18n
- Fluent catalog
- fallback locale
- missing key check
- pseudo locale
```

---

## PR-B12 — UI String Migration

### 입력

```text
PR-R09 findings
```

### 목표

하드코딩 UI 문자열 제거.

### 완료 기준

```text
- Settings / Agent / Env / MCP / Notification 문자열 key화
- terminal output은 번역하지 않음
```

---

## PR-B13 — Runtime Message Localization

### 입력

```text
PR-R09 findings
```

### 목표

RuntimeEvent / NotificationEvent를 message_id + args 구조로 변경.

### 완료 기준

```text
- DB/Audit에는 message_id + args 저장
- UI가 현재 locale로 표시
- remote client가 자기 locale로 표시 가능
```

---

## PR-B14 — File Watcher Debounce & Ignore Rules

### 목표

폴더 트리와 파일 변경 감지를 저비용으로 운영.

### 완료 기준

```text
- notify 기반 watcher
- debounced event
- ignore rules
- .env 변경 감지
- 대량 파일 변경 UI freeze 없음
```

---

## PR-B15 — SQLite Write Batching

### 목표

metadata write 비용 감소.

### 완료 기준

```text
- status update debounce
- notification insert batch
- log offset update batch
- prepared statement reuse
- UI thread DB write 금지
```

---

## PR-B16 — Global Activity View

### 목표

여러 workspace/agent를 한눈에 관리.

### 완료 기준

```text
- all running/waiting/error agent 표시
- high resource session 표시
- production env session 표시
- stop all in workspace
```

---

# 6. Release Gate PR

## PR-G00 — Final Performance Gate

### 시나리오

```text
A. 빈 앱
B. workspace 5개 / pane 20개 / session 10개
C. hidden session 10개 / 대량 output
D. folder tree 100k files
E. remote slow consumer
```

### 완료 기준

```text
- RSS/CPU 목표 충족
- idle repaint 없음
- active pane frame time p95 목표 충족
- queue unbounded 증가 없음
```

---

## PR-G01 — Final Security Gate

### 완료 기준

```text
- secret이 DB/config/log/export에 없음
- raw log 기본 비활성
- production env guard 동작
- drag/drop path가 자동 실행되지 않음
- MCP tool schema 변경 시 재승인
```

---

## PR-G02 — Final I18n / CJK Gate

### 완료 기준

```text
- en-US / ja-JP / zh-Hans / zh-Hant key completeness 100%
- pseudo-locale overflow 없음
- CJK path DnD 정상
- CJK terminal selection/copy/paste 정상
```

---

# 7. 에이전트 작업 분리 방식

본 문서는 특정 모델명이나 내부 실행 도구명을 포함하지 않는다.

작업은 역할 기준으로만 분리한다.

## 7.1 Review Agent 역할

```text
- 기존 구현 검토
- 회귀 탐지
- boundary violation 탐지
- perf/security/i18n 문제 탐지
- 구체적 재현 절차 작성
- 수정 제안 작성
```

Review Agent는 가능한 한 코드를 크게 변경하지 않는다.

## 7.2 Build Agent 역할

```text
- Review findings 기반 구현
- 신규 기능 개발
- 테스트 추가
- 문서 갱신
- PR acceptance criteria 충족
```

Build Agent는 구현 후 반드시 Review Agent 재검토를 요청한다.

## 7.3 Review → Build → Review 흐름

```text
Review PR
 → findings.md
 → Build PR
 → tests
 → Review PR 재검토
 → merge
```

---

# 8. 최종 우선순위

가장 먼저 할 것:

```text
1. PR-R00 Current Implementation Inventory
2. PR-R01 Boundary Violation Review
3. PR-R02 Dependency Graph / Store Cycle Review
4. PR-R03 Pane / Mux / Visibility Review
5. PR-R04 Folder Tree / Drag-and-Drop Review
6. PR-R05 Terminal Clipboard / Paste / CJK Review
```

그 다음:

```text
7. PR-B00 Fix Critical Boundary Violations
8. PR-B01 Store Crate Cycle Fix
9. PR-B02 Pane Resource Guard
10. PR-B03 Folder Tree Scalability & DnD Hardening
11. PR-B04 Terminal Clipboard / Paste / CJK Hardening
```

이후:

```text
12. PR-B11 I18n Infrastructure
13. PR-B12 UI String Migration
14. PR-B13 Runtime Message Localization
15. PR-B07 Process Resource Monitor
16. PR-B08 Workspace Auto Suspend
17. PR-B09 Terminal Cache Budget Manager
18. PR-B10 Output Pipeline Backpressure
```

release 전:

```text
PR-G00 Final Performance Gate
PR-G01 Final Security Gate
PR-G02 Final I18n / CJK Gate
```

---

## 9. 최종 판단

현재 구현은 유지한다.  
새 아키텍처로 갈아엎지 않는다.

이 문서는 다음을 보장하기 위한 후속 PR 계획이다.

```text
- 이미 동작하는 pane/folder tree/DnD/copy-paste UX 회귀 방지
- 기존 구현 검토와 신규 개발 분리
- store crate cycle 문제 안전하게 해결
- RAM/CPU/RSS 최소화
- 다국어와 CJK 안정성 확보
- MCP/env/secret/log 보안 강화
- remote/slow consumer 대비
```

최종 결론:

```text
Review Track에서 기존 구현을 먼저 검토한다.
Critical/High 문제만 Build Track에서 즉시 수정한다.
신규 개발은 Review findings 기반으로 분리해 진행한다.
모든 Build PR은 Review 재검토를 통과해야 한다.
```
