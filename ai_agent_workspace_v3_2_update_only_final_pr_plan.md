# Rust AI Agent Workspace — 구현 완료 후 최종 업데이트 PR 문서 v3.2

작성일: 2026-07-05  
대상: v2.5 / v2.6 / v2.8 기반 구현 완료 코드베이스  
문서 성격: **리뷰 결과를 반영한 Build-only 최종 업데이트 PR 계획서**  
주의: 이 문서는 리뷰 PR을 포함하지 않는다. 기존 리뷰는 완료되었거나 별도 산출물로 존재한다고 가정한다.

---

## 0. 목적

현재 프로젝트는 이미 다음 기능이 구현된 상태다.

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

본 문서의 목적은 리뷰 결과에서 확인된 보완사항을 바탕으로, **신규 개발/하드닝/성능 최적화/릴리스 게이트 PR만 정리**하는 것이다.

---

## 1. 유지해야 할 핵심 불변 원칙

모든 업데이트 PR은 아래 원칙을 반드시 지킨다.

```text
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
```

---

## 2. 업데이트 PR 운영 원칙

## 2.1 리뷰 결과 기반 개발

모든 PR은 기존 리뷰 산출물을 입력으로 받는다.

```text
입력:
  docs/review/*-findings.md

필수:
  - finding severity 확인
  - Critical / High 먼저 처리
  - Medium / Low는 명시적으로 scope 포함 여부 결정
  - 기존 pane/folder tree/DnD/copy-paste UX 회귀 금지
```

## 2.2 PR 공통 완료 조건

모든 PR은 최소한 아래를 만족해야 한다.

```bash
cargo fmt --check
cargo clippy --workspace --all-targets
cargo check --workspace --all-targets
cargo test --workspace --no-run
```

PR 성격에 따라 추가한다.

```bash
cargo xtask check-deps
cargo xtask security-scan
cargo xtask perf-smoke
cargo xtask i18n-check
cargo xtask smoke-db-migrations
```

---

# 3. Phase A — Architecture / Boundary / Dependency 안정화

---

## PR-U00 — Findings Intake & Update Baseline

### 목표

기존 리뷰 산출물을 정리하고, 업데이트 PR의 기준선을 고정한다.

### 입력

```text
docs/review/PR-R00-current-implementation-inventory.md
docs/review/PR-R01-boundary-violation-findings.md
docs/review/PR-R02-dependency-graph-store-cycle-findings.md
docs/review/PR-R03-pane-mux-visibility-findings.md
docs/review/PR-R04-folder-tree-dnd-findings.md
docs/review/PR-R05-terminal-clipboard-paste-cjk-findings.md
docs/review/PR-R06-redaction-secret-env-leak-findings.md
docs/review/PR-R07-mcp-permission-audit-findings.md
docs/review/PR-R08-resource-performance-findings.md
docs/review/PR-R09-i18n-readiness-findings.md
```

### 작업

```text
1. docs/update/update-findings-summary.md 생성
2. Critical / High / Medium / Low finding 집계
3. 각 finding을 PR-Uxx에 매핑
4. 중복 finding 병합
5. release blocker 분류
```

### 완료 기준

```text
- 모든 finding이 하나 이상의 PR-Uxx 또는 Deferred Backlog에 매핑됨
- Critical / High finding은 Deferred 불가
- 현 baseline의 회귀 금지 기능 목록 확정
```

---

## PR-U01 — Boundary Violation Fix

### 목표

UI/runtime/terminal/pty/secret 경계 위반을 수정한다.

### 작업

```text
1. app crate에서 alacritty_terminal 직접 참조 제거
2. app crate에서 portable-pty 직접 참조 제거
3. app crate에서 SecretStore 직접 호출 제거
4. UI → RuntimeClient 경로로 통일
5. terminal backend 구현체 타입 UI 노출 제거
6. xtask check-boundary 추가 또는 강화
```

### 완료 기준

```text
- UI는 RuntimeClient만 사용
- UI가 SessionManager 직접 호출하지 않음
- TerminalBackend 구현체 타입 UI 노출 없음
- SecretStore 직접 호출은 runtime/env injection 계층으로 이동
- cargo xtask check-boundary 통과
```

---

## PR-U02 — Store Crate Cycle Fix

### 목표

`storage → mcp → storage` 같은 crate 순환 의존을 근본적으로 제거한다.

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

### 작업

```text
1. storage-core는 DB infra만 담당
2. mcp-model은 저장 가능한 MCP 순수 타입만 담당
3. mcp-runtime은 실행/transport/protocol만 담당
4. mcp-store는 SQL / Row / Repository만 담당
5. audit-store / mux-store / env-store / session-store 분리
6. persist는 cross-store orchestration만 담당
7. 기존 storage facade는 bridge로 freeze하거나 제거
8. xtask check-deps 강화
```

### 금지 의존

```text
mcp-runtime → mcp-store 금지
mcp-store → mcp-runtime 금지
storage-core → mcp-* 금지
store crate → runtime 금지
store crate → app 금지
store crate → egui 금지
```

### 완료 기준

```text
- cyclic package dependency 없음
- mcp → storage 직접 의존 없음
- storage → mcp-runtime 직접 의존 없음
- mcp-store → mcp-model
- mcp-runtime → mcp-model
- storage-core는 domain crate를 모름
- cargo xtask check-deps 통과
- cargo check --workspace --all-targets 통과
```

---

## PR-U03 — DB Migration & Store Repository Gate

### 목표

store crate 분리 후 migration과 repository 테스트를 안정화한다.

### 작업

```text
1. store별 repository 테스트 추가
2. 빈 DB → 최신 schema smoke
3. 구버전 DB fixture → 최신 migration smoke
4. PRAGMA foreign_key_check
5. WAL mode 확인
6. destructive migration copy-table 검증
7. migration 전 DB backup 동작 확인
```

### 완료 기준

```text
- cargo xtask smoke-db-migrations 통과
- mcp-store insert/list/update/delete 테스트
- audit-store redacted audit insert 테스트
- mux-store layout save/load 테스트
- env-store secret env는 credential_id만 저장
- session-store session create/update/status 테스트
```

---

# 4. Phase B — Pane / Folder Tree / Terminal UX 하드닝

---

## PR-U04 — Pane & Mux Resource Guard

### 목표

pane 수가 많아져도 active/visible pane만 렌더링되도록 강제한다.

### 작업

```text
1. PaneVisibilityTracker 구현
2. hidden pane TerminalViewportSnapshot 생성 카운터 추가
3. hidden pane glyph layout 카운터 추가
4. Mux layout 변경 시 dirty pane만 갱신
5. multi-pane stress test 추가
```

### 완료 기준

```text
- hidden pane snapshot 생성 0회
- hidden pane glyph layout 0회
- active pane만 paint
- pane 20개 생성 후 idle CPU 목표 유지
- pane/session 분리 유지
```

---

## PR-U05 — Folder Tree Scalability & DnD Hardening

### 목표

폴더 트리가 큰 프로젝트에서도 CPU/RAM을 적게 쓰고, DnD path insert가 안전하게 동작하도록 한다.

### 작업

```text
1. folder tree node lazy loading 검증/보강
2. virtualized tree rendering
3. node cache budget
4. ignore rules 적용
5. path canonicalization
6. shell별 path quoting
7. multi-path drop 처리
8. Unicode path test
9. DnD insert preview
```

### 기본 ignore

```text
.git/
node_modules/
target/
dist/
build/
.next/
.turbo/
vendor/
logs/
.cache/
.DS_Store
```

### Drag/drop 정책

```text
- drop은 기본적으로 path insert만 한다.
- 자동 실행/Enter 전송 금지.
- "drop 후 Enter"는 명시적 opt-in 설정에서만 허용.
- shell별 quoting 적용:
  - PowerShell
  - cmd
  - bash
  - zsh
  - fish
- 여러 경로 drop 시 안전 quoting 후 공백 구분.
```

### 완료 기준

```text
- 100k file workspace에서 UI freeze 없음
- folder tree initial load lazy
- 공백/CJK/emoji path insert 정상
- drop이 명령 자동 실행을 유발하지 않음
- folder tree DnD 회귀 테스트 통과
```

---

## PR-U06 — Terminal Clipboard / Paste / DnD Hardening

### 목표

터미널 내부 selection/copy/paste/drag/drop paste를 안정화한다.

### 작업

```text
1. Terminal selection model 재검토
2. grapheme cluster 기반 selection boundary
3. CJK wide cell selection 테스트
4. bracketed paste 처리
5. paste normalization
6. clipboard abstraction platform crate로 이동
7. terminal drop text/path paste와 normal paste 경로 통합
8. paste payload redaction scan 옵션
```

### 완료 기준

```text
- 일본어/중국어/영어 selection copy 정상
- wide char selection에서 글자 잘림 없음
- bracketed paste 정상
- multiline paste 안전
- terminal 내부 drag/drop paste 정상
- clipboard 실패 시 localized error 표시
```

---

## PR-U07 — CJK / IME / Unicode Terminal QA

### 목표

일본어, 중국어, 영어 환경에서 terminal path/selection/input이 안정적으로 동작하게 한다.

### 대상

```text
English
Japanese
Chinese Simplified
Chinese Traditional
Korean optional
Emoji paths
Mixed-width strings
```

### 작업

```text
1. unicode width 기반 display width 검증
2. grapheme boundary 검증
3. IME composition event handling 검증
4. CJK font fallback 검증
5. line wrap / cursor movement 검증
6. double-width char selection/copy 검증
7. CJK path drag/drop 검증
```

### 테스트 문자열

```text
English:
  src/main.rs

Japanese:
  プロジェクト/設定ファイル.rs

Chinese Simplified:
  项目/配置文件.rs

Chinese Traditional:
  專案/設定檔.rs

Korean optional:
  프로젝트/설정파일.rs

Emoji:
  project/🚀-deploy/config.json
```

### 완료 기준

```text
- CJK path insert 정상
- CJK terminal display width 정상
- IME 조합 중 깨짐 없음
- copy/paste 후 문자열 손상 없음
- wide char cursor 위치 정상
```

---

# 5. Phase C — Redaction / MCP / Security 하드닝

---

## PR-U08 — Redaction Pipeline Hardening

### 목표

secret/env/API key 유출 방지와 redaction 성능을 동시에 확보한다.

### 작업

```text
1. compiled pattern cache
2. streaming redaction
3. lookbehind buffer
4. ANSI-stripped matching
5. encoded variant matching
6. redaction failure conservative drop
7. export-time scan
8. clipboard/export/crash dump redaction scan
```

### fixture

```text
chunk_boundary.txt
ansi_inserted_secret.txt
base64_secret.txt
url_encoded_secret.txt
json_escaped_secret.txt
database_url.txt
bearer_token.txt
ssh_private_key.txt
```

### 완료 기준

```text
- chunk boundary fixture 통과
- ANSI-inserted secret fixture 통과
- base64/url/json escaped fixture 통과
- DB/config/log/export secret scan 통과
- redaction CPU 비용 측정 첨부
```

---

## PR-U09 — Project Env Safety & Production Guard

### 목표

프로젝트별 환경변수 관리에서 production secret 오주입을 방지한다.

### 작업

```text
1. Env Diff Preview 강화
2. production profile 실행 전 경고
3. workspace allowlist
4. agent별 production env 차단
5. MCP server로 production env 주입 시 별도 경고
6. .env import 시 secret 의심값 탐지
7. .env export 시 secret 제외
```

### 완료 기준

```text
- production profile 실행 전 guard 표시
- secret env는 credential_id만 저장
- env diff에서 secret은 masked
- .env export에 secret 포함 안 됨
- MCP env binding은 mcp_server_env_profiles 사용
```

---

## PR-U10 — MCP Permission / Audit Hardening

### 목표

MCP tool 승인과 audit 보안을 강화한다.

### 작업

```text
1. tool 호출 전 approval 필수
2. once/session/always scope 검증
3. schema hash 변경 시 재승인
4. input_redacted_json 기본
5. input_encrypted_blob opt-in
6. stdout valid MCP message only
7. stderr redaction
8. MCP server crash/restart audit
```

### 완료 기준

```text
- approval dialog 필수
- schema hash 변경 시 재승인
- audit input은 redacted 기본
- encrypted blob은 선택
- MCP stdout에 로그가 섞이면 protocol error 처리
```

---

## PR-U11 — Final Security Gate

### 목표

release 전 보안 기준을 통과한다.

### 작업

```text
1. DB/config/log/export secret scan
2. clipboard leak check
3. DnD path injection check
4. MCP tool approval audit check
5. env production guard check
6. encrypted raw log policy check
7. dependency graph forbidden edge check
```

### 완료 기준

```text
- secret이 DB/config/log/export에 없음
- raw log 기본 비활성
- production env guard 동작
- drag/drop path가 자동 실행되지 않음
- MCP tool schema 변경 시 재승인
- cargo xtask security-scan 통과
```

---

# 6. Phase D — CPU / RAM / RSS / Backpressure 최적화

---

## PR-U12 — Process Resource Monitor

### 목표

앱 자체뿐 아니라 child process까지 포함해 CPU/RAM/RSS를 추적한다.

### 작업

```text
1. process monitor 구현
2. session별 process tree
3. workspace별 CPU/RAM/RSS aggregation
4. sampling interval 1~2초
5. CPU는 최소 2회 refresh 후 표시
6. high CPU/RAM warning
7. kill/restart/suspend action
```

### 표시 항목

```text
- app RSS
- workspace RSS
- session process tree RSS
- CPU %
- runtime duration
- output rate
- log size
```

### 완료 기준

```text
- 여러 agent 실행 시 workspace별 자원 표시
- hidden session 과다 사용 감지
- high CPU process 찾기 가능
- UI frame마다 process monitor refresh 없음
```

---

## PR-U13 — Workspace Auto Suspend

### 목표

여러 workspace를 오래 열어도 메모리 증가를 제한한다.

### 상태

```rust
pub enum WorkspaceRuntimeState {
    Active,
    Warm,
    Suspended,
    Closed,
}
```

### 정책

```text
Active:
  visible panes render

Warm:
  recent log tail + status only

Suspended:
  layout/session metadata only
  renderer/cache drop

Closed:
  DB metadata only
```

### 완료 기준

```text
- 일정 시간 hidden workspace는 Warm/Suspended 전환
- Suspended workspace에서 TerminalViewportSnapshot 생성 0회
- 복귀 시 status/log tail 빠르게 복원
```

---

## PR-U14 — Terminal Cache Budget Manager

### 목표

터미널 캐시/RSS 증가를 제한한다.

### 정책

```text
Global terminal cache budget:
  기본 128MB

Visible session:
  10,000 lines 또는 16MB

Hidden session:
  1,000 lines 또는 2MB

Archived session:
  terminal state drop
```

### 완료 기준

```text
- cache budget 초과 시 hidden/archived cache 회수
- active visible pane 우선
- RSS 감소 확인
- cache eviction 이벤트 추적 가능
```

---

## PR-U15 — Output Pipeline Backpressure

### 목표

대량 출력에서도 UI가 멈추지 않도록 한다.

### 정책

```text
우선순위:
  1. redacted log writer
  2. status detector
  3. active visible terminal update
  4. hidden render event

queue pressure:
  - render event drop/coalesce 가능
  - log writer 보존 우선
  - log writer 병목 지속 시 PTY read 일시 중단
  - log drop 금지
```

### 완료 기준

```text
- 10MB/min hidden output 3개에서도 UI responsive
- active pane frame time p95 목표 유지
- output queue unbounded 증가 없음
- backpressure badge 표시
```

---

## PR-U16 — File Watcher Debounce & Ignore Rules

### 목표

폴더 트리와 파일 변경 감지를 저비용으로 운영한다.

### 작업

```text
1. watcher debounce
2. ignore rules
3. batch event
4. file tree invalidation 최소화
5. .env 변경 감지
6. generated file storm 방지
```

### ignore 기본값

```text
.git/
node_modules/
target/
dist/
build/
.next/
.turbo/
vendor/
logs/
.cache/
.DS_Store
```

### 완료 기준

```text
- node_modules/target/.git 이벤트 무시
- 대량 파일 변경에도 UI freeze 없음
- .env 변경 시 경고
- file watcher CPU 비용 측정 첨부
```

---

## PR-U17 — Status Detector Cost Control

### 목표

상태 감지 정확도와 CPU 비용을 균형 있게 관리한다.

### 작업

```text
1. agent별 regex compile cache
2. stream regex batch 처리
3. hidden session snapshot 금지 유지
4. backend grid text read-only scan
5. idle heuristic 최소화
6. confidence score
7. user override
```

### 완료 기준

```text
- waiting/running/done/error 감지
- false positive 감소
- hidden session 감지 비용 제한
- 상태 감지 비용 측정 첨부
```

---

## PR-U18 — SQLite Write Batching

### 목표

metadata write 비용을 줄인다.

### 작업

```text
1. status update debounce
2. notification insert batch
3. log offset update batch
4. prepared statement reuse
5. background DB worker
6. UI thread DB write 금지
```

### 완료 기준

```text
- high output 상황에서 SQLite write 폭주 없음
- WAL 유지
- UI block 없음
- write batch 통계 표시 가능
```

---

## PR-U19 — Remote Slow Consumer Backpressure

### 목표

느린 remote/browser client가 runtime 전체를 막지 않게 한다.

### 정책

```text
- client별 bounded outbound queue
- terminal delta drop/coalesce 가능
- status event 보존
- command ack/failure event
- degraded mode
```

### 완료 기준

```text
- 느린 client가 있어도 local runtime 영향 제한
- event queue memory bounded
- disconnect/reconnect 안정
```

---

## PR-U20 — Final Performance Gate

### 목표

release 전 실제 사용 시나리오 성능을 검증한다.

### 시나리오

```text
Scenario A:
  빈 앱

Scenario B:
  workspace 5개
  pane 20개
  session 10개
  active visible pane 2개

Scenario C:
  hidden session 10개
  3개 session 대량 output

Scenario D:
  folder tree 100k files

Scenario E:
  remote client slow consumer
```

### 완료 기준

```text
- RSS/CPU 목표 충족
- idle repaint 없음
- active pane frame time p95 목표 충족
- queue unbounded 증가 없음
- 결과를 docs/performance/final-gate.md에 기록
```

---

# 7. Phase E — I18n / CJK / Accessibility

---

## PR-U21 — I18n Infrastructure

### 목표

다국어 기반을 추가한다.

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

### 구조

```text
crates/i18n/
locales/en-US/
locales/ja-JP/
locales/zh-Hans/
locales/zh-Hant/
```

### 완료 기준

```text
- fallback locale
- missing key check
- pseudo locale
- locale setting 저장
- current + fallback locale만 로드
```

---

## PR-U22 — UI String Migration

### 목표

하드코딩 UI 문자열을 translation key로 이전한다.

### 대상

```text
- Settings
- Credentials
- Project Environment
- Folder Tree
- Terminal actions
- Agent status
- Notifications
- Connector Center
- MCP permission dialog
- Error dialogs
- Resource monitor
```

### 완료 기준

```text
- 사용자 노출 문자열 key화
- terminal output은 번역하지 않음
- path / command / env key / MCP tool name 번역 금지
```

---

## PR-U23 — Runtime Message Localization

### 목표

RuntimeEvent / NotificationEvent / AppError를 다국어 구조로 바꾼다.

### 정책

```text
RuntimeEvent:
  message_id + args

NotificationEvent:
  message_id + args

AppError:
  error_code + args

DB/Audit:
  번역 문자열이 아니라 message_id + args 저장
```

### 완료 기준

```text
- 과거 notification도 현재 locale로 표시 가능
- remote client가 자기 locale로 표시 가능
- logs에는 error_code/message_id 저장
```

---

## PR-U24 — I18n / CJK Layout Gate

### 목표

일본어/중국어/영어 UI와 terminal path UX를 release gate로 검증한다.

### 작업

```text
1. en-US / ja-JP / zh-Hans / zh-Hant 번역 completeness 검사
2. pseudo-locale overflow 검사
3. CJK path DnD 검사
4. CJK terminal selection/copy 검사
5. IME composition smoke test
```

### 완료 기준

```text
- 모든 필수 locale key 100%
- pseudo-locale에서 버튼/패널 잘림 없음
- CJK path drop/copy/paste 정상
- CJK terminal selection/copy/paste 정상
```

---

## PR-U25 — Global Activity View

### 목표

여러 workspace/agent를 한눈에 관리한다.

### UI

```text
All Running Agents
 ├─ Workspace A / Claude / Waiting / CPU 12%
 ├─ Workspace B / Codex / Running tests / RSS 900MB
 ├─ Workspace C / Gemini / Error
 └─ Workspace D / OpenCode / Done
```

### 기능

```text
- Go to waiting agent
- Stop all in workspace
- Show production env sessions
- Show high CPU sessions
- Show sessions using credential X
```

### 완료 기준

```text
- 사용자가 방치된 agent를 찾을 수 있음
- high resource session 제어 가능
- production env session 식별 가능
```

---

# 8. 최종 실행 순서

## 8.1 즉시 처리

```text
1. PR-U00 Findings Intake & Update Baseline
2. PR-U01 Boundary Violation Fix
3. PR-U02 Store Crate Cycle Fix
4. PR-U03 DB Migration & Store Repository Gate
```

## 8.2 UX 회귀 방지

```text
5. PR-U04 Pane & Mux Resource Guard
6. PR-U05 Folder Tree Scalability & DnD Hardening
7. PR-U06 Terminal Clipboard / Paste / DnD Hardening
8. PR-U07 CJK / IME / Unicode Terminal QA
```

## 8.3 보안

```text
9. PR-U08 Redaction Pipeline Hardening
10. PR-U09 Project Env Safety & Production Guard
11. PR-U10 MCP Permission / Audit Hardening
12. PR-U11 Final Security Gate
```

## 8.4 리소스 최적화

```text
13. PR-U12 Process Resource Monitor
14. PR-U13 Workspace Auto Suspend
15. PR-U14 Terminal Cache Budget Manager
16. PR-U15 Output Pipeline Backpressure
17. PR-U16 File Watcher Debounce & Ignore Rules
18. PR-U17 Status Detector Cost Control
19. PR-U18 SQLite Write Batching
20. PR-U19 Remote Slow Consumer Backpressure
21. PR-U20 Final Performance Gate
```

## 8.5 다국어 / CJK / 운영 UX

```text
22. PR-U21 I18n Infrastructure
23. PR-U22 UI String Migration
24. PR-U23 Runtime Message Localization
25. PR-U24 I18n / CJK Layout Gate
26. PR-U25 Global Activity View
```

---

## 9. 최종 판단

이 문서는 기존 Review Track을 제거하고, 리뷰 결과를 기반으로 실제 업데이트해야 할 PR만 정리한다.

최종 목표:

```text
- 이미 구현된 pane/folder tree/DnD/copy-paste UX 회귀 방지
- review findings 기반 보완사항 처리
- storage/mcp/audit/persist 순환 의존 완전 차단
- RAM/CPU/RSS 자원 최소화
- 여러 workspace / 여러 pane / 여러 agent 장기 실행 안정화
- 일본어/중국어/영어 다국어와 CJK terminal UX 확보
- MCP/env/secret/log 보안 강화
- remote slow consumer 대비
```

최종 운영 기준:

```text
Review Track은 별도 완료된 것으로 본다.
이제는 PR-U00부터 PR-U25까지 Build/Hardening/Gate 트랙만 진행한다.
각 PR은 findings 기반으로 scope를 제한하고, 완료 후 acceptance criteria를 만족해야 한다.
```
