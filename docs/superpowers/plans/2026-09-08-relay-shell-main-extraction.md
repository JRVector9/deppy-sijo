# Relay shell main stacked 이관 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** R1 Rust 코어 위에 #146의 브라우저 셸·shared viewer·배포 artifact 검증을 독립 R2 PR로 게시한다.

**Architecture:** main → `feat/relay-reconnect-core-main` → `feat/relay-shell-main` 순서다. R1에 이미 이관한 Rust handshake와 동일한 공개 벡터로 WebCrypto/IndexedDB 등록 commit·세대 fencing·채널 nonce 계약을 확인한다. 앱 adapter와 실제 배포는 후속이며 이 PR은 활성화를 주장하지 않는다.

**Tech Stack:** ES modules, WebCrypto, IndexedDB, Node test runner, loopback headless Chrome, Rust web-remote static routes, POSIX shell artifact hash.

---

## 파일 경계

- 생성: `web/relay-shell/{build.sh,index.html,manifest.webmanifest,relay-crypto.js,relay-shell.js,relay-terminal.js,relay-shell.css,sw.js}`, `web/relay-shell/tests/{channel-concurrency,reconnect}.test.mjs`.
- 생성: `web/shared/{mobile-theme.css,viewer-core.css,viewer-core.js}`.
- 생성: `deploy/relay-shell/README.md`, `deploy/relay-shell/{staging,production}/{headers.conf,shell.env.example}`.
- 생성/수정: `.github/workflows/relay-shell-release.yml`, `.github/workflows/build-test.yml`의 Node job, `.gitignore`의 dist ignore.
- 수정: `crates/web-remote/assets/{app.css,app.js,index.html,offline.html,pairing.html}`, `crates/web-remote/src/static_srv.rs`의 shared viewer 정적 응답과 테스트.
- 생성/수정: `crates/web-remote/tests/chrome_support/mod.rs`, `tests/{chrome_support_contract,relay_shell_chrome,relay_shell_deploy,viewer_core_chrome,relay_webcrypto_vectors}.rs`, `tests/fixtures/{relay-hello-v1.json,relay-shell-v1.js,viewer-core-contract.js,relay-webcrypto-v1.js}`.
- 수정 hunk 하나: `crates/web-remote/src/relay_client/handshake.rs::the_v1_hello_fixture_matches_this_implementation`.
- 문서: 이 계획과 `docs/CODEX_HANDOFF.md`. R1 계약 설명은 그대로 상속한다.

실제 파일명은 commit 전 `git status --short`로 대조한다. app/secret/terminal/i18n/package/xtask/ureq 변경은 금지한다. R1의 protocol/server/storage/relay Rust 제품 hunk를 R2에 다시 넣지 않는다.

### Task 1: R1 독립 게시와 R2 분기

- [x] **Step 1: R1의 fixture 없는 archive 검증 및 PR 게시를 확인한다.**

```sh
git diff --cached --name-only
git log -2 --oneline
gh pr view feat/relay-reconnect-core-main --json url,headRefOid,baseRefName
```

예상: R1 base main, staged 제품 파일에 shell/새 JSON 없음. 실제 gate 결과는 R1 handoff에 기록한다.

- [x] **Step 2: R1 최종 commit에서 남은 hunk를 유지한 채 R2 branch를 생성한다.**

```sh
git switch -c feat/relay-shell-main
git diff --name-only
```

예상: 위 R2 파일만 diff에 남는다. 원본 #146 수정·cherry-pick 전체 commit·rebase·force-push는 하지 않는다.

### Task 2: 브라우저·교차 언어·artifact 검증

- [x] **Step 1: 기존 hunk의 Node 상태 회귀를 실행한다.**

```sh
node --test web/relay-shell/tests/*.test.mjs
```

예상: nonce 순서, stale generation, IndexedDB commit 실패·복구 계약 16 tests PASS. 새로운 회귀 결함이 나오면 먼저 assertion 실패를 보존한 뒤 최소 source만 수정한다.

- [x] **Step 2: Rust web-remote와 실제 headless Chrome 회귀를 직렬 실행한다.**

```sh
CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/private/tmp/deppy-relay-core-main-20260908/target cargo test -p web-remote --locked -- --test-threads=1
CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/private/tmp/deppy-relay-core-main-20260908/target cargo test -p web-remote --locked --test relay_webcrypto_vectors --test relay_shell_chrome --test viewer_core_chrome -- --ignored --test-threads=1
```

예상: shared static route·Rust/JS 벡터 PASS. Chrome은 임시 profile/루프백/격리 child만 사용한다. Chrome이 없거나 환경상 실행 불가면 미실행을 명시하고 PASS로 처리하지 않는다. Deppy UI 빌드·실행은 하지 않는다.

- [x] **Step 3: .test 오리진의 로컬 artifact와 digest만 검증한다.**

```sh
SHELL_ORIGIN=https://shell.example.test RELAY_ORIGIN=wss://relay.example.test RELAY_SHELL_DIST=/private/tmp/deppy-relay-core-artifact-20260908 sh web/relay-shell/build.sh
RELAY_SHELL_DIST=/private/tmp/deppy-relay-core-artifact-20260908 sh web/relay-shell/build.sh verify
```

예상: archive/manifest digest PASS, Publish는 BLOCKED 유지. 공개 JSON fixture와 테스트 키는 artifact 파일 목록에 없다.

### Task 3: 리뷰·최종 게이트·stacked PR

- [x] **Step 1: bounded Codex CLI의 실제 source 리뷰를 확인하고 지적을 수정한다.** 이미 수행한 R1/R2 결합 source 리뷰의 shell/WebCrypto/static 계약 결과를 재사용하되 이후 제품 변경은 별도 검토한다. 문서만 리뷰 입력으로 주지 않는다.
- [x] **Step 2: fmt/diff/strict Clippy와 제외 경계를 확인한다.**

```sh
cargo fmt --all -- --check
git diff --check
CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/private/tmp/deppy-relay-core-main-20260908/target cargo clippy -p web-remote --all-targets --locked -- -D warnings
git diff --exit-code feat/relay-reconnect-core-main -- crates/app crates/secret crates/terminal crates/i18n Cargo.toml Cargo.lock crates/relay-protocol crates/relay-server crates/storage crates/web-remote/src/push.rs xtask
```

- [x] **Step 3: handoff/Obsidian 일지에 실제 결과·GitGuardian 근거·BLOCKED를 기록하고 commit/push/PR한다.**

```sh
git add web deploy/relay-shell .github/workflows .gitignore crates/web-remote/assets crates/web-remote/src/static_srv.rs crates/web-remote/src/relay_client/handshake.rs crates/web-remote/tests docs/CODEX_HANDOFF.md docs/superpowers/plans/2026-09-08-relay-shell-main-extraction.md
git commit -m 'feat(relay): 브라우저 셸과 공유 viewer를 코어 위에 이관한다'
git push -u origin feat/relay-shell-main
gh pr create --base feat/relay-reconnect-core-main --head feat/relay-shell-main --title 'feat(relay): 브라우저 셸과 공유 viewer 분리 이관' --body-file /private/tmp/deppy-relay-shell-pr-body.md
```

GitGuardian 원본 incident 37016215는 JSON:35의 공개 순차바이트0x00..0x1f 테스트 벡터다. 실제 credential·운영 grant를 복사하지 않았으며 문자열 분할/억제 설정으로 탐지를 우회하지 않는다. DNS/TLS/배포 자격증명/외부 배포/실기기/24h soak는 BLOCKED로 남긴다. 원본 #146과 #149는 건드리지 않는다.

## 실행 중 재현과 최소 수정

- Chrome viewer 첫 실행은60초report timeout, 동일소스 재시도는PASS였다. 임시 Node 서버/CDP에서 status=ok·91frames·errors=[] 확인 후 test HTTP helper를 조사했다.
- chrome_support_contract::fragmented_loopback_request_keeps_its_static_response가 received=Ok(0)으로 실제RED. macOS accepted socket의 비차단 상속으로 부분 헤더만 읽고 종료했다.
- serve_one에서 set_nonblocking(false),2초 write idle을 추가했다. 제품 JS에는 변경 없이 helper 회귀와 전체 web-remote·Chrome3·strict Clippy/fmt를 다시 검증한다.

최종 실제 결과: web-remote337 PASS/4 ignored 뒤 Chrome3 실제PASS, Node16 PASS, artifact build/verify·strict Clippy/fmt/diff/boundary PASS. scoped source와 helper 재리뷰 확정 결함 없음.
