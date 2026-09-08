# External Markdown Render Isolation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 외부에서 드롭한 Markdown 문서의 렌더러가 panic하더라도 앱 프로세스를 종료하지 않고 해당 문서 탭만 격리한다.

**Architecture:** 문서별 `render_failed` 상태와 렌더 진입점 하나의 `catch_unwind` 경계를 둔다. 실패한 탭은 다음 프레임부터 다시 렌더하지 않고 OS 기본 앱 열기 안내를 표시하며, panic 직전 편집 내용의 dirty 상태와 닫기 확인을 보존한다.

**Tech Stack:** Rust, egui/eframe, 프로젝트 i18n catalog, tracing, Cargo tests

---

### Task 1: 문서별 실패 상태와 렌더 격리 계약

**Files:**
- Modify: `crates/app/src/app.rs`
- Test: `crates/app/src/app.rs` 내부 tests

- [x] **Step 1: 실패 격리와 dirty 보존 테스트만 먼저 추가한다**

`guard_document_render`가 첫 panic을 `Panicked`로 반환하고, 같은 문서는 다음 프레임에 `Skipped`, 다른 문서는 `Rendered`가 되는 테스트를 추가한다. `OpenDocument::quarantine_render_failure()` 후 `document_close_disposition`이 `ConfirmDirty`인지 확인한다.

- [x] **Step 2: 테스트가 RED인지 확인한다**

Run: `CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo --bin deppy-sijo --locked '문서_렌더_panic' -- --nocapture --test-threads=1`

Expected: `guard_document_render`, `render_failed`, `quarantine_render_failure`가 없어 컴파일 실패.

- [x] **Step 3: 최소 격리 상태와 일반화된 guard를 추가한다**

```rust
#[derive(Debug, PartialEq, Eq)]
enum GuardedDocumentRender<R> {
    Skipped,
    Rendered(R),
    Panicked,
}

fn guard_document_render<S, R>(
    state: &mut S,
    should_skip: impl FnOnce(&S) -> bool,
    render: impl FnOnce(&mut S) -> R,
    quarantine: impl FnOnce(&mut S),
) -> GuardedDocumentRender<R> {
    if should_skip(state) {
        return GuardedDocumentRender::Skipped;
    }
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| render(state))) {
        Ok(output) => GuardedDocumentRender::Rendered(output),
        Err(_) => {
            quarantine(state);
            GuardedDocumentRender::Panicked
        }
    }
}
```

`OpenDocument`에 `render_failed: bool`을 추가하고 새 문서는 `false`로 시작한다. 격리할 때 `recompute_dirty()`를 먼저 호출한 뒤 실패 상태를 설정한다.

- [x] **Step 4: RED 테스트가 GREEN인지 확인한다**

Run: `CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo --bin deppy-sijo --locked '문서_렌더_panic' -- --nocapture --test-threads=1`

Expected: 해당 필터의 테스트가 모두 PASS.

### Task 2: 실제 단일·분할 문서 렌더 경계와 실패 안내

**Files:**
- Modify: `crates/app/src/app.rs`
- Modify: `crates/app/src/panic_policy.rs`
- Modify: `crates/i18n/locales/en-US/messages.txt`
- Modify: `crates/i18n/locales/ja-JP/messages.txt`
- Modify: `crates/i18n/locales/ko-KR/messages.txt`
- Modify: `crates/i18n/locales/zh-Hans/messages.txt`
- Modify: `crates/i18n/locales/zh-Hant/messages.txt`

- [x] **Step 1: 두 렌더 진입점이 안전 wrapper를 쓰는 구조 검증을 추가한다**

production source에서 `self.render_document_tab_body_safely(` 호출이 정확히 두 번인지, wrapper가 `guard_document_render`와 `quarantine_render_failure`를 호출하는지 검사한다.

- [x] **Step 2: 구조 테스트가 RED인지 확인한다**

Run: `CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo --bin deppy-sijo --locked '문서_렌더_panic은' -- --nocapture --test-threads=1`

Expected: 안전 wrapper가 없어 FAIL.

- [x] **Step 3: 안전 wrapper와 실패 화면을 추가한다**

단일·분할 작업면의 기존 `render_document_tab_body` 호출을 모두 `render_document_tab_body_safely`로 교체한다. `Panicked`면 저카디널리티 tracing event를 남기고 다음 repaint를 요청한다. `Skipped`면 `document.error.render_failed`와 기존 `file_tree.open_with_os` 버튼을 표시한다.

- [x] **Step 4: panic 진단에서 사용자 경로와 payload를 제외한다**

panic hook은 payload와 임의 지정 가능한 thread name을 읽지 않고 컴파일 소스 basename과 line만 기록한다. `/Users/person/private/project/src/workspace.rs`가 `workspace.rs`로 축약되는 단위 테스트를 둔다.

- [x] **Step 5: 다섯 locale에 동일 키를 추가하고 검사한다**

Run: `CARGO_BUILD_JOBS=2 cargo test -p i18n --locked -- --test-threads=1`

Expected: 5개 catalog의 `document.error.render_failed`가 일치하고 전체 i18n 테스트 PASS.

### Task 3: 검토, 게이트, 커밋과 PR

**Files:**
- Modify: `docs/CODEX_HANDOFF.md`

- [x] **Step 1: 범위가 문서 격리만 포함하는지 확인한다**

Run: `git diff --name-only origin/main...HEAD`

Expected: `app.rs`, `panic_policy.rs`, 5개 locale, plan, handoff만 표시되고 `file_tree.rs`와 세션 열기 키가 없어야 한다.

- [x] **Step 2: 실제 코드 diff를 Codex로 리뷰하고 Critical/High/Medium을 처리한다**

Run: `codex review --uncommitted`

Expected: 코드 diff에 대한 완료된 리뷰 결과. 종료되지 않으면 PID를 확인해 정확한 프로세스만 중단하고 미완료 리뷰를 PASS로 기록하지 않는다.

- [x] **Step 3: 최종 focused gate를 한 번 실행한다**

Run: `CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo --bin deppy-sijo --locked '문서_렌더_panic' -- --test-threads=1`

Run: `CARGO_BUILD_JOBS=2 cargo test -p i18n --locked -- --test-threads=1`

Run: `cargo fmt --all -- --check`

Run: `git diff --check`

Expected: 실행한 명령이 모두 PASS. 앱 빌드·재실행·화면 확인은 사용자 요청 전까지 하지 않는다.

- [x] **Step 4: handoff를 갱신하고 의미 단위로 커밋한다**

```bash
git add crates/app/src/app.rs crates/app/src/panic_policy.rs crates/i18n/locales docs/superpowers/plans/2026-09-08-md-drop-render-isolation.md docs/CODEX_HANDOFF.md
git commit -m "fix(app): 문서 렌더 실패를 탭 단위로 격리한다"
```

- [x] **Step 5: 일반 push 후 main 대상 PR을 생성한다**

```bash
git push -u origin fix/md-drop-render-isolation
gh pr create --base main --head fix/md-drop-render-isolation --title "fix(app): 외부 Markdown 렌더 실패를 격리한다" --body-file /tmp/deppy-md-drop-pr.md
```
