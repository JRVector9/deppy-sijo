# SSH Workspace Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
> 해당 skill을 사용할 수 없으면 현재 환경의 동일한 plan 실행 절차로 대체한다. 모델은 저장소 AGENTS의 GPT-6.1-Sol/xhigh를 따른다. 사용자가 제안한 순서의 개발과 병렬 서브에이전트 구현을 승인했다. 소스 구현과 검증을 완료했으며, 앱 재실행은 승인되지 않았다.

**Goal:** SSH로 연결한 서버의 폴더를 독립적인 워크스페이스로 추가하고 터미널·파일·Git·에이전트를 같은 원격 root에서 사용한다.

**Architecture:** Local/SSH target을 분리하고 macOS OpenSSH 연결 워커와 서버 headless helper를 사용한다. 기존 runtime command/event framing과 bounded queue를 재사용하되 SSH stdio·원격 파일/Git API·원격 runtime 소유권은 추가한다. 원격 helper의 실제 Linux 빌드·실행 호환성은 구현 단계의 gate다.

**Tech Stack:** Rust workspace, egui 공용 popup, SQLite migration, macOS OpenSSH/ssh-agent, Linux PTY, 기존 runtime protocol.

---

상태: **PR1~6 소스 구현·검증 완료**. 별도 `feat/ssh-workspace-20261022` 브랜치에서 Linux helper·파일/Git·App/PWA와 시안 적용을 통합했고, GitHub Rust SSH 비교로 찾은 호환성 및 개발 asset 누락도 최소 수정했다. 전체 gate와 기본 SSH 노출 검증이 통과했고 범위별 로컬 커밋을 남겼다. 실행 결과와 정확한 남은 작업은 `docs/CODEX_HANDOFF.md`를 따른다. 외부 PR/출시/앱 재실행은 하지 않았다.

시안: [SSH 워크스페이스](../../mockups/ssh-workspace-add-20261022.html). 본문은 예시 데이터만 사용한다.

## UI 결정

- 기존 560pt `WorkspaceAddUi` 셸에서 `내 폴더 / GitHub 저장소 / SSH 서버`를 선택한다. 입력36pt, 버튼34pt, 고정 footer·내부 body scroll, 기존 palette/radius3pt를 유지한다.
- 기본 입력은 SSH 주소 하나. 기존 ssh config와 agent를 사용한다. port/identity 선택은 접힌 연결 옵션에 둔다.
- 연결된 뒤 서버 폴더를 단일 선택한다. 경로 입력/상위 폴더/현재 폴더 선택/하위 폴더 진입을 지원한다. 미연결·인증 중·지문 확인·설치 동의·오프라인은 추가 비활성.
- 신규 host는 지문 확인. 바뀐 지문은 연결 중단하고 별도 host 설정의 명시적 복구 없이는 자동 덮어쓰기하지 않는다.
- 첫 helper 설치는 사용자 동의 후 계정의 `~/.local/share/deppy/remote-host/<version>`에 수행. 시안의 처음 연결 상태에서 다음 준비 상태를 거쳐야 폴더를 열 수 있다.
- 주소/port/인증 대상을 바꾸면 connection generation 증가, 이전 folder list/selection/async response 무효화. 비밀 인증 입력은 취소/대상 변경 시 폐기한다.
- 설치된 서버 에이전트만 선택한다. 인증정보·로컬 API 키를 자동으로 서버에 복사하지 않는다. PWA는 기존 Mac gateway를 통해 동일 원격 세션을 제어한다.

## 코드 근거

- **추가 화면** — `crates/app/src/workspace_add.rs:505`: Local / GitHub 두 종류뿐. GitHub의 SSH URL은 로컬 복제 주소입니다. SSH source와 별도 연결 상태가 필요.
- **워크스페이스 식별** — `crates/storage/src/db.rs:3639`: WorkspaceRow.path는 로컬 루트이며 볼륨·inode 검증도 로컬 파일 기준입니다. host / account / root를 구분한 target 필요.
- **App 런타임** — `crates/app/src/app.rs:8396`: WorkspaceRuntime이 InProcessRuntimeClient를 직접 소유합니다. Remote runtime 소유권·정리·자원 관측 분리 필요.
- **전송 기반** — `crates/runtime/src/remote.rs:933`: RemoteRuntimeServer/Client, 바이너리 프레이밍·token·TLS·수명 관리가 존재합니다. 재사용 가능하지만 SSH bootstrap/stdio는 추가 필요.
- **파일·문서** — `crates/app/src/document_io.rs:234`: 문서 open/save와 파일트리 작업이 로컬 std::fs 기반입니다. 서버 root를 받는 provider와 원격 저장 충돌 처리 필요.
- **Git** — `crates/app/src/git_cli.rs:25`: 공용 helper가 로컬 /usr/bin/git을 실행합니다. 동일 서버에서 실행하는 Git provider 필요.
- **공용 팝업** — `docs/design/popup-components.md:90`: 560pt 양식·36pt 입력·34pt 버튼 규칙과 재사용 셸이 있습니다. 기존 팝업에 SSH 선택지와 상태만 확장.

## 방식 선택

1. **OpenSSH + remote helper 추천**: 서버에서 PTY·파일·Git·관측을 함께 처리하며 사용자가 선택한 위치와 실행 위치가 일치한다. bootstrap·프로토콜 확장이 필요하다.
2. SSH 셸만 실행: 빠르지만 파일트리/Git/에이전트 상태는 원격 workspace로 완성되지 않는다.
3. SSHFS: 일부 로컬 API 재사용 가능하지만 별도 마운트/설치/지연·끊김 수명 관리가 필요하고 실행 위치도 별도다.

## 계약 · 데이터 수명

- `WorkspaceTarget`은 `Local { root }` 또는 `Ssh { connection_id, remote_workspace_id, canonical_root }`를 표현한다. target이 remote이면 legacy `WorkspaceRow.path`를 로컬 filesystem API로 넘기지 않는 adapter를 PR 1에서 만든다. 기존 SQL path는 legacy projection이며 별도 target 테이블/typed projection이 source of truth다. remote row에 가짜 local directory를 생성하지 않는다.
- identity는 alias 문자열만 쓰지 않는다. 검증한 host key + 실제 account + 원격 canonical root/서버 workspace ID를 묶고, alias·접속 port는 연결 설정으로 구분한다. 키 변경은 기존 identity 자동 교체가 아니다.
- 요청은 `{workspace_id, runtime_instance, connection_generation, request_id}`를 포함한다. 취소·대상 변경 이후 response는 받아도 UI/DB/입력 queue에 적용하지 않는다. Local과 remote session ID의 scope를 섞지 않는다.
- handshake는 version/capabilities/max bytes를 협상한다. `ListDirectory`, `StatRoot`, `ReadFile`, `WriteFileIfVersion`, `GitStatus`, `GitDiff`, `InspectAgents`는 구조화된 요청으로 제한하고 JSON/shell interpolation 문자열을 조립하지 않는다.
- SSH 연결은 `/usr/bin/ssh` argv로 분리한다. helper 실행 명령은 고정 템플릿, remote root는 프로토콜 payload로 전달한다. `known_hosts` 의미를 직접 다시 구현하지 않고 OpenSSH 검증을 사용하며 승인한 host key의 기록은 exact host/port를 대조해 원자적으로 처리한다. 키 자동수락/검증 off 금지.
- connection worker는 bounded request/event queue와 deadline을 소유한다. stderr를 계속 비우되 크기 제한·민감값 삭제, UI thread I/O 금지. 설정 변경·취소·sleep/wake에서 소유 child/pipe/reader를 회수한다.
- helper의 stdio bridge와 세션 supervisor는 수명을 구분한다. 연결 단절만으로 세션을 다시 spawn하거나 입력을 재전송하지 않는다. 마지막 확인한 runtime/session와 서버 lease가 같을 때만 reattach하고 새 snapshot을 받는다. 유지 불가/만료는 명시적으로 종료/복원 상태로 보여준다.
- 파일 root는 서버에서 canonicalize·권한 확인. UI 표시용 한글 정규화가 실제 원격 경로 bytes/식별을 바꾸지 않게 한다. 응답 목록은 page/byte 제한, 심볼릭 링크 root 탈출은 거절한다. WriteFileIfVersion은 원격 stamp를 비교하고 원자적 교체한다.
- read-only Git 조회와 write Git 명령은 구분한다. 모든 git cwd는 선택한 remote root; stale generation의 변경은 취소한다. 로컬 `/usr/bin/git`에 원격 문자열을 넘기지 않는다.

## PR 순서

`PR 1 → (PR 2 ∥ PR 3) → PR 4 → PR 5 → PR 6`. 2/3 병렬은 target/protocol 계약 고정 뒤 가능. App 통합을 여러 worker가 동시에 수정하지 않는다. 사용자 노출은 통합 완료 전 feature flag 기본 off.

## PR 1: 원격 워크스페이스 식별·저장 경계

**의존:** 선행 없음

로컬 경로와 SSH 서버 경로를 구분하는 데이터 모델과 서비스 계약을 먼저 고정합니다. 기존 워크스페이스는 Local로 복원합니다.

**파일**

- `crates/core/src/workspace_target.rs (신규)`
- `crates/storage/src/db.rs`
- `crates/storage/src/workspace_target.rs (신규)`
- `crates/storage/src/workspace_identity.rs`

**구현·검증 작업**

- [x] 같은 /home/ubuntu/project라도 서로 다른 서버·계정이면 다른 워크스페이스입니다.
- [x] 기존 DB migration 후 로컬 경로·볼륨 증명·종료 표식이 유지됩니다.
- [x] Remote path를 로컬 canonicalize/볼륨 확인에 전달하지 않습니다.
- [x] RED: 위 경계 동작을 검증하는 새 regression fixture를 작성하고 수정 전 실패를 확인한다. 컴파일/fixture 오류를 제품 RED로 세지 않는다.
- [x] GREEN: 최소 구현 후 `cargo test --locked -p deppy-core -p storage workspace_target`로 해당 regression과 인접 회귀를 실행한다.
- [x] 코드리뷰·diff/fmt·변경 crate Clippy 후 PR 범위만 커밋한다.

**완료 조건:** Local/SSH target 분리·migration·중복 등록 판정이 테스트로 고정됨. UI에는 아직 노출하지 않음.

## PR 2: SSH 연결·인증·취소 워커

**의존:** PR 1 이후

macOS OpenSSH를 백그라운드에서 실행하고 기존 SSH 설정·ssh-agent·점프 호스트를 사용합니다. 처음 본 서버는 지문 확인, 변경된 지문은 접속 중단으로 처리합니다.

**파일**

- `crates/ssh-transport/ (신규)`
- `crates/ssh-askpass/ (신규)`
- `crates/app/src/ssh_connection.rs (신규)`
- `scripts/package-macos.sh`

**구현·검증 작업**

- [x] alias, user@host, ssh user@host, ssh://user@host:port 입력을 지원하고 임의 명령·옵션은 거절합니다.
- [x] 키 암호·암호·추가 인증은 요청별 임시 입력으로만 전달하고 로그·argv·DB에 저장하지 않습니다.
- [x] DNS 지연·인증 실패·취소에서 UI 스레드를 차단하지 않고 자기 소유 SSH/askpass 프로세스만 회수합니다. 연결 단절·deadline fixture를 검증했으며 실제 Mac sleep/wake는 미실행입니다.
- [x] RED: 위 경계 동작을 검증하는 새 regression fixture를 작성하고 수정 전 실패를 확인한다. 컴파일/fixture 오류를 제품 RED로 세지 않는다.
- [x] GREEN: 최소 구현 후 `cargo test --locked -p ssh-transport -p ssh-askpass`로 해당 regression과 인접 회귀를 실행한다.
- [x] 코드리뷰·diff/fmt·변경 crate Clippy 후 PR 범위만 커밋한다.

**완료 조건:** 가짜 ssh fixture에서 호스트 검증·인증·타임아웃·취소·자식 회수 통과. UI와 연결은 다음 단계.

## PR 3: 원격 실행 구성요소·런타임 연결

**의존:** PR 1 이후, PR 2와 병렬 가능

서버에서 PTY와 에이전트를 실행할 headless helper를 만듭니다. 기존 Runtime command/event·프레임 형식·큐 제한을 재사용하고, SSH stdio 연결 및 재접속 수명을 추가합니다.

**파일**

- `crates/remote-host/ (신규)`
- `crates/runtime/src/remote.rs`
- `crates/runtime/src/protocol.rs`
- `crates/runtime/src/client.rs`
- `crates/app/src/remote_bootstrap.rs (신규)`

**구현·검증 작업**

- [x] 초기 대상은 Linux x86_64/arm64. helper 바이너리·의존성의 실제 Linux 빌드/실행이 통과해야 지원으로 표시합니다.
- [x] 설치 전 명시적 동의. 버전별 경로·검증된 번들 파일·원자적 교체·원복을 사용하며 sudo나 외부 포트 개방이 필요하지 않습니다.
- [x] 연결이 끊겼을 때 입력을 자동 재전송하지 않습니다. 재접속 후 같은 workspace/runtime/session인지 확인하고 snapshot을 다시 받습니다.
- [x] RED: 위 경계 동작을 검증하는 새 regression fixture를 작성하고 수정 전 실패를 확인한다. 컴파일/fixture 오류를 제품 RED로 세지 않는다.
- [x] GREEN: 최소 구현 후 `cargo test --locked -p runtime ssh_stream && cargo test --locked -p deppy-remote-host`로 해당 regression과 인접 회귀를 실행한다.
- [x] 코드리뷰·diff/fmt·변경 crate Clippy 후 PR 범위만 커밋한다.

**완료 조건:** 실 Linux fixture에서 생성·입력·resize·중단·재접속과 helper 호환성 검증 통과. 세션 유지 정책은 supervisor/lease로 명시함.

## PR 4: 서버 폴더·문서·Git 서비스

**의존:** PR 1 + PR 2 + PR 3 이후

목록·읽기·저장·변경 관측·Git 조회를 실제 서버에서 처리하는 provider를 추가합니다. 원격 root 범위와 계정 권한을 매 요청에서 확인합니다.

**파일**

- `crates/remote-host/src/files.rs (신규)`
- `crates/remote-host/src/git.rs (신규)`
- `crates/app/src/workspace_files.rs (신규)`
- `crates/app/src/document_io.rs`
- `crates/app/src/git_cli.rs`
- `crates/app/src/ui/file_tree.rs`

**구현·검증 작업**

- [x] 폴더명 공백·한글·따옴표·심볼릭 링크·권한 없음·빈 폴더·경로 소실을 구분합니다.
- [x] 목록 페이지/응답 byte/동시 요청/관측 수를 제한하고 숨김·비활성 workspace의 감시를 중지합니다.
- [x] 파일 저장은 원격 버전 비교와 원자적 교체를 사용합니다. Git status/working-file diff는 같은 원격 root·연결 세대에서 조회하고 변경 명령은 명시적으로 미지원 처리합니다.
- [x] RED: 위 경계 동작을 검증하는 새 regression fixture를 작성하고 수정 전 실패를 확인한다. 컴파일/fixture 오류를 제품 RED로 세지 않는다.
- [x] GREEN: 최소 구현 후 `cargo test --locked -p deppy-remote-host files && cargo test --locked -p deppy-remote-host git`로 해당 regression과 인접 회귀를 실행한다.
- [x] 코드리뷰·diff/fmt·변경 crate Clippy 후 PR 범위만 커밋한다.

**완료 조건:** 파일·Git 요청이 동일 서버 root로 라우팅되고 외부 root 탈출/오래된 응답/로컬 오실행이 차단됨.

## PR 5: 시안 적용·원격 세션과 에이전트

**의존:** PR 1 ~ PR 4 이후

기존 워크스페이스 추가 팝업에 SSH를 넣고 연결→폴더 선택→추가를 한 화면으로 만듭니다. App runtime 소유권을 Local/Remote로 분리해 원격 workspace에 연결합니다.

**파일**

- `crates/app/src/workspace_add.rs`
- `crates/app/src/ssh_workspace_add.rs (신규)`
- `crates/app/src/app.rs`
- `crates/app/src/ui/file_tree.rs`
- `crates/app/src/ui/agent_terminal.rs`
- `crates/i18n/locales/*/messages.txt`
- `docs/design/popup-components.md`
- `docs/mockups/shared-popup-components-2026-09-30.html`

**구현·검증 작업**

- [x] 팝업560pt·입력36pt·버튼34pt·footer8pt 간격을 유지하고 본문 내부만 스크롤합니다.
- [x] 주소를 바꾸면 이전 서버의 폴더 선택/응답을 무효화하고 추가 버튼을 잠급니다. 취소/재시도에서는 주소와 표시 이름을 보존합니다.
- [x] 서버 배지·원격 폴더·연결 상태를 sidebar와 terminal에 표시하고 서버에 설치된 에이전트만 실행합니다. 로컬 API 키 자동 전송은 하지 않습니다.
- [x] RED: 위 경계 동작을 검증하는 새 regression fixture를 작성하고 수정 전 실패를 확인한다. 컴파일/fixture 오류를 제품 RED로 세지 않는다.
- [x] GREEN: 최소 구현 후 `cargo test --locked -p deppy-sijo ssh_workspace && cargo test --locked -p deppy-sijo workspace_add`로 해당 regression과 인접 회귀를 실행한다.
- [x] 코드리뷰·diff/fmt·변경 crate Clippy 후 PR 범위만 커밋한다.

**완료 조건:** 시안 모든 상태의 egui interaction·스크린샷 대조 및 실제 Linux 서버의 생성/입력/파일/Git 통합 검증 통과.

## PR 6: 재시작·독립성·PWA·출시 검증

**의존:** PR 5 이후

네트워크·절전·앱 재시작과 두 서버/여러 세션 조합을 검증하고 지원 범위를 확정합니다. PWA는 Mac을 통해 같은 원격 세션을 보여줍니다.

**파일**

- `crates/app/src/ssh_connection.rs (신규 기능의 후속)`
- `crates/runtime/src/remote/liveness.rs`
- `crates/web-remote/src/session_core.rs`
- `crates/web-remote/src/ws_api.rs`
- `docs/qa/ssh-workspace.md (신규)`
- `Cargo.toml`
- `Cargo.lock`
- `CHANGELOG.md`

**구현·검증 작업**

- [x] 동일 경로·동일 에이전트·다른 서버/계정/세션에서 제목·상태·입력이 섞이지 않습니다.
- [x] 앱 재시작은 저장 정보 복원과 서버 접속을 구분합니다. 서버 오프라인이어도 UI를 사용할 수 있고 닫기/삭제는 원격 작업 종료와 명확히 구분합니다.
- [x] 연결 단절·중복 응답·late ACK·PWA resize/긴 한글 입력·기존 로컬/GitHub 회귀를 검증합니다. 물리 Mac sleep/wake는 별도 미실행 항목이며 disconnect/deadline/세대 변경 fixture로 대체했다고 주장하지 않습니다.
- [x] RED: 위 경계 동작을 검증하는 새 regression fixture를 작성하고 수정 전 실패를 확인한다. 컴파일/fixture 오류를 제품 RED로 세지 않는다.
- [x] GREEN: 최소 구현 후 `cargo test --locked --workspace && cargo clippy --locked --workspace --all-targets -- -D warnings`로 해당 regression과 인접 회귀를 실행한다.
- [x] 코드리뷰·diff/fmt·변경 crate Clippy 후 PR 범위만 커밋한다.

**완료 조건:** 테스트와 리소스 정리/중복 전송0 증거 확보 후 기능 flag 해제. 실제 출시 때 최신 배포 버전보다 높은 minor 증가·번들 버전 검증. 재실행은 별도 사용자 요청 필요.

## 최종 검증 매트릭스

| 축 | 실행할 케이스 | 합격 기준 |
| --- | --- | --- |
| 입력 | alias/user@host/ssh prefix/URI/IPv6/비기본 port/옵션 주입/공백 경로 | 유효 대상만 접수, 임의 command 실행 없음 |
| 인증 | key/ssh-agent/passphrase/password/additional prompt/host first use/changed key | 요청별 인증·서버 검증·취소, 비밀 로그/DB/argv 0 |
| 플랫폼 | Linux x86_64·arm64 helper, 미지원 OS, helper 불일치/설치실패 | 검증된 대상만 지원 표시, 실패는 재시도/원복 |
| 대상 독립성 | 서버2×계정2×동일 root×동일 agent·session IDs | 제목·상태·이력·파일·입력 소유권 혼동 0 |
| 수명 | reconnect/offline/sleep/wake/restart/cancel/late reply | UI 정지0, 소유 프로세스/reader/lease 누수0, 입력 재전송0 |
| 파일/Git | 한글/공백/권한/없는 경로/symlink/대형 목록/외부변경/remote git 없음 | 원격 root 보존·bounded 처리·저장 충돌 표시 |
| 화면 | 560pt 팝업, 작은 viewport, keyboard, 긴 라벨/주소, light/dark | 고정 footer·본문 scroll·공용 디자인 일치·가로 overflow0 |
| 회귀/PWA | 로컬/GitHub 추가, workspace 종료/재열기, PWA resize/긴 한글 입력 | 기존 동작 보존 및 같은 서버 세션 라우팅 |

## 구현 후 검증 기록

- 실제 결과와 원래 실패/coherent 재검증 cohort는 [QA 보고서](../../qa/ssh-workspace.md)와 [작은 evidence 기록](../../qa/evidence/ssh-workspace-20261022/README.md)을 따른다.
- [Rust SSH 비교·설계 감사](../../qa/2026-10-22-rust-ssh-design-review.md): OpenSSH 프로토콜을 구현하지 않고 설치된 `/usr/bin/ssh`를 재사용한다. 호스트 인증서, FingerprintHash, LogVerbose, -G command context의 실제 회귀와 개발 asset 준비를 검증했다.
- 최종 전체 runner는 source/scripts freeze 상태에서 10개 gate 모두 통과했다. App3345PASS/0FAIL/42ignored, no-default App3342PASS/0FAIL/41ignored이며 strict Clippy/fmt/boundary/deps/i18n도 통과했다. 최종 옵션을 포함한 실제 Host/Linux 및 설치 동의 각각1PASS도 확인했다. SSH 기본 노출은 App Cargo와 package를 함께 변경했고 별도 일반 default App3345PASS/0FAIL/42ignored, Python23개, strict Clippy/fmt/package gate/boundary/deps 모두 통과했다. Cargo metadata에서도 default=["ssh-workspaces"]를 확인했다. no-default-features 경로는 유지한다.
- 실제 물리 Mac sleep/wake·App 실행/재실행·사용자 서버 설치·출시 artifact 전달은 이번 작업에 포함되지 않는다. source0.13.0 investigation build를 release로 세지 않는다.

## 참고한 1차 자료

- [OpenSSH ssh_config](https://man.openbsd.org/ssh_config.5): Host/IdentityFile/ProxyJump/host key checking. 기존 설정을 재사용하는 근거.
- [VS Code Remote SSH](https://code.visualstudio.com/docs/remote/ssh): SSH 접속 후 remote folder를 열고 서버 구성요소로 동작하는 UX 참고. 구현을 그대로 이식한다는 뜻은 아니다.

## 최초 시안 단계의 검증 범위 (구현 전 기록)

HTML 상호작용·레이아웃·정적 코드 근거만 검증한다. 브라우저 artifact-only QA는 Safari에서 최종 **33PASS/0FAIL**을 확인했다. 실제 native double-click으로 하위 폴더 이동과 현재 폴더 선택도 별도로 확인했다. 1280/768/390px iframe viewport의 가로 넘침·팝업 폭·footer 가시성과 36px 입력/34px 버튼을 검사했다. `node --check` inline JS 문법 검사 및 `git diff --check`도 실행했다. 실제 SSH 연결·자격증명·원격 설치·Linux cross-build·제품 테스트는 실행하지 않았으며 계획된 작업이다. 실제 feature 출시 때 app version을 현재 마지막 배포보다 높은 minor로 올리고 binary/두 plist값을 검증해야 한다. 앱 재실행은 별도 현재 작업의 명시 요청이 있어야 한다.
