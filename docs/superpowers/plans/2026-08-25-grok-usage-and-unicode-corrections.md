# Grok Usage and Unicode Corrections Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Grok의 오래된/종료된 사용량 probe 상태를 정확히 처리하고, 파일 트리의 raw OS 이름을 보존하면서 모든 파일명 UI를 NFC로 표시한다.

**Architecture:** Grok worker 결과는 완료 시각을 포함한 typed payload로 전달하고 설치본의 명시적 최종 상태만 조기 종료한다. 파일 트리는 raw `OsString` identity와 NFC display projection을 분리하며, 공통 UI helper가 문서·트리·터미널 메뉴의 표시 문자열만 정규화한다.

**Tech Stack:** Rust 2024, std `mpsc`/`Instant`/`OsString`, `unicode-normalization`, egui 0.35, Cargo/libtest, macOS Developer-ID packaging.

---

## 파일 구조

- `crates/app/src/grok_usage.rs`: Grok probe channel timestamp, final-state recognition, focused regressions.
- `crates/app/src/ui/mod.rs`: raw `OsStr`/`Path`에서 NFC 표시 문자열을 만드는 공통 projection과 단위 테스트.
- `crates/app/src/ui/file_tree.rs`: raw listing/tree component identity, stable display sort, rename/delete/location UI wiring, invalid-byte regressions.
- `crates/app/src/ui/workspace.rs`: 터미널 선택 경로의 파일 열기 메뉴 표시명 wiring.
- `crates/app/src/app.rs`: host listing raw name 보존, canonical-equivalent rename no-op, document tab/confirm display wiring, host regressions.
- `docs/CODEX_HANDOFF.md`: RED/GREEN, 리뷰, 패키징, 재실행 증거.

### Task 1: Grok 완료 시각과 최종 no-data 상태

**Files:**
- Modify/Test: `crates/app/src/grok_usage.rs:32-103,204-223,297-347,415-680`

- [ ] **Step 1: 오래 지연된 channel 결과 회귀 테스트 작성**

`UsageState`에 완료 시각이 601초 전인 값을 channel로 넣고 production drain helper를 호출한 뒤 저장 시각이 worker 시각과 같고 `fresh_usage_after`가 `None`인지 검증한다.

```rust
#[test]
fn buffered_probe_result_keeps_worker_completion_time() {
    let usage = GrokUsage {
        weekly_remaining_percent: Some(70),
        monthly_remaining_percent: None,
        credits_left: None,
    };
    let completed_at = Instant::now() - STALE_AFTER - Duration::from_secs(1);
    let (sender, receiver) = mpsc::sync_channel(1);
    sender.send(Some((completed_at, usage))).unwrap();
    let mut state = UsageState {
        pending: Some(receiver),
        ..UsageState::default()
    };

    receive_pending_probe(&mut state);

    let (stored_at, stored) = state.usage.unwrap();
    assert_eq!(stored_at, completed_at);
    assert_eq!(fresh_usage_after(stored, stored_at.elapsed()), None);
}
```

- [ ] **Step 2: timestamp RED 실행**

Run:

```bash
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p deppy-sijo --locked --bin deppy-sijo buffered_probe_result_keeps_worker_completion_time -- --test-threads=1
```

Expected: `receive_pending_probe` 부재 또는 old channel payload 타입 불일치로 exit 101.

- [ ] **Step 3: 설치본 최종 상태 회귀 테스트 작성·RED 실행**

```rust
#[test]
fn installed_terminal_no_data_states_finish_the_probe() {
    for panel in [
        "No billing data available.",
        "Usage limits are managed by your team.",
        "Couldn't load usage: Loading session usage",
    ] {
        assert!(usage_panel_rendered(panel), "{panel}");
    }
}
```

Run:

```bash
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p deppy-sijo --locked --bin deppy-sijo installed_terminal_no_data_states_finish_the_probe -- --test-threads=1
```

Expected: 첫 미지원 문구에서 assertion failure, exit 101.

- [ ] **Step 4: 최소 Grok 구현**

channel의 성공 payload를 `(Instant, GrokUsage)`로 만들고 drain을 함수로 추출한다. 완료 시각은 fetch가 반환된 직후 worker에서 한 번만 찍는다.

```rust
type CompletedProbe = Option<(Instant, GrokUsage)>;

#[derive(Default)]
struct UsageState {
    usage: Option<(Instant, GrokUsage)>,
    pending: Option<mpsc::Receiver<CompletedProbe>>,
    last_request: Option<Instant>,
}

fn receive_pending_probe(state: &mut UsageState) {
    let Some(receiver) = state.pending.as_ref() else { return };
    match receiver.try_recv() {
        Ok(Some(completed)) => {
            state.usage = Some(completed);
            state.pending = None;
        }
        Ok(None) | Err(mpsc::TryRecvError::Disconnected) => state.pending = None,
        Err(mpsc::TryRecvError::Empty) => {}
    }
}
```

worker는 `sender.send(usage.map(|usage| (Instant::now(), usage)))`를 사용한다. `usage_panel_rendered` 허용목록에는 compact 형태 `nobillingdataavailable`, `usagelimitsaremanagedbyyourteam`, `couldntloadusage`만 추가한다.

- [ ] **Step 5: Grok GREEN과 전체 모듈 실행**

Run:

```bash
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p deppy-sijo --locked --bin deppy-sijo buffered_probe_result_keeps_worker_completion_time -- --test-threads=1
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p deppy-sijo --locked --bin deppy-sijo installed_terminal_no_data_states_finish_the_probe -- --test-threads=1
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p deppy-sijo --locked --bin deppy-sijo grok_usage::tests -- --test-threads=1
cargo fmt --all -- --check
git diff --check
```

Expected: focused 1/1, focused 1/1, Grok group 17 passed with live test ignored, static checks exit 0.

- [ ] **Step 6: Grok 의미 단위 커밋**

```bash
git add crates/app/src/grok_usage.rs
git commit -m "fix(usage): Grok 측정 시각과 종료 상태 보존"
```

### Task 2: raw 파일명 identity 보존

**Files:**
- Modify/Test: `crates/app/src/app.rs:11696-11759`
- Modify/Test: `crates/app/src/ui/file_tree.rs:649-711,960-990,1478-1502,6520-6660,7380-7520`

- [ ] **Step 1: host listing invalid UTF-8 RED 작성**

Unix에서 raw `b"broken-\xff"` 파일을 만들고 `run_file_tree_listing` 결과의 item 이름 바이트가 동일한지 확인한다.

```rust
#[cfg(unix)]
#[test]
fn file_tree_listing_preserves_invalid_utf8_name_bytes() {
    use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
    let root = unique_temp_dir("file-tree-invalid-utf8");
    std::fs::create_dir_all(&root).unwrap();
    let raw = std::ffi::OsString::from_vec(b"broken-\xff".to_vec());
    std::fs::write(root.join(&raw), b"x").unwrap();

    let snapshot = run_file_tree_listing(
        &root,
        &root,
        ui::file_tree::FILE_TREE_LISTING_MAX_ITEMS,
        ui::file_tree::FILE_TREE_LISTING_MAX_BYTES,
    ).unwrap();
    let item = snapshot.items().iter().find(|item| item.name().as_bytes() == raw.as_bytes()).unwrap();
    assert_eq!(item.name().as_bytes(), raw.as_bytes());
    std::fs::remove_dir_all(root).unwrap();
}
```

- [ ] **Step 2: host listing RED 실행**

Run:

```bash
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p deppy-sijo --locked --bin deppy-sijo file_tree_listing_preserves_invalid_utf8_name_bytes -- --test-threads=1
```

Expected: lossy U+FFFD bytes 때문에 lookup unwrap failure, exit 101.

- [ ] **Step 3: tree identity/collision RED 작성·실행**

`FileTreeListingItem::try_new`에 서로 다른 invalid raw 이름 두 개를 넣고 snapshot→tree→flat 경로가 두 raw component를 각각 보존하는지 검증한다. 표시명은 같아도 `HashSet<PathBuf>` 길이가 2여야 한다.

Run:

```bash
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p deppy-sijo --locked --bin deppy-sijo invalid_utf8_rows_keep_distinct_raw_paths -- --test-threads=1
```

Expected: current `String` API가 `OsString`을 받지 못하거나 path identity assertion이 실패해 exit 101.

- [ ] **Step 4: raw identity 최소 구현**

`FileTreeListingItem`과 `TreeNode`에 raw 이름과 display projection을 분리한다.

```rust
pub struct FileTreeListingItem {
    name: std::ffi::OsString,
    display_name: Arc<str>,
    is_dir: bool,
}

impl FileTreeListingItem {
    pub fn try_new(name: std::ffi::OsString, is_dir: bool) -> Result<Self, FileTreeMaintenanceErrorCode> {
        let bytes = name.as_encoded_bytes();
        if name.is_empty() || bytes.contains(&0) || bytes.len() > FILE_TREE_PATH_MAX_BYTES {
            return Err(FileTreeMaintenanceErrorCode::InvalidSnapshot);
        }
        let display_name: String = name.to_string_lossy().nfc().collect();
        Ok(Self { name, display_name: Arc::from(display_name), is_dir })
    }

    pub fn name(&self) -> &std::ffi::OsStr { &self.name }
    pub fn display_name(&self) -> &str { &self.display_name }
}
```

host는 `entry.file_name()`을 그대로 전달하고 raw encoded byte 길이로 cap을 계산한다. snapshot/tree 정렬은 `display_name` 뒤 raw `name` tie-break를 사용한다. `node_ref`, `node_mut`, merge, hidden, expanded-path, flat path는 raw `OsStr`/`OsString`을 사용한다. 테스트 fixture의 String 호출은 `OsString::from`으로만 기계적으로 맞춘다.

- [ ] **Step 5: raw identity GREEN과 file-tree 그룹 실행**

Run:

```bash
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p deppy-sijo --locked --bin deppy-sijo file_tree_listing_preserves_invalid_utf8_name_bytes -- --test-threads=1
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p deppy-sijo --locked --bin deppy-sijo invalid_utf8_rows_keep_distinct_raw_paths -- --test-threads=1
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p deppy-sijo --locked --bin deppy-sijo ui::file_tree::tests -- --test-threads=1
```

Expected: focused 1/1, focused 1/1, complete file-tree group all pass.

### Task 3: 모든 파일명 UI의 NFC display projection

**Files:**
- Modify/Test: `crates/app/src/ui/mod.rs`
- Modify/Test: `crates/app/src/ui/file_tree.rs:2725-2778,3335-3383,6571-6590`
- Modify/Test: `crates/app/src/ui/workspace.rs:7068-7087`
- Modify/Test: `crates/app/src/app.rs:16761-16772,27559-27573`

- [ ] **Step 1: 공통 display helper RED 작성**

```rust
#[test]
fn nfd_file_name_and_path_are_displayed_as_nfc() {
    let raw = std::path::Path::new("/tmp/\u{1112}\u{1161}\u{11AB}\u{1100}\u{1173}\u{11AF}.md");
    assert_eq!(path_file_name_display(raw), "한글.md");
    assert_eq!(path_display(raw), "/tmp/한글.md");
}
```

Run:

```bash
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p deppy-sijo --locked --bin deppy-sijo nfd_file_name_and_path_are_displayed_as_nfc -- --test-threads=1
```

Expected: helper 부재로 exit 101.

- [ ] **Step 2: canonical-equivalent rename no-op RED 작성**

NFD raw source와 NFC 요청 이름을 `app_host_rename_is_noop`에 넣어 true이며 원본 파일이 남는 계약을 고정한다.

```rust
#[test]
fn canonical_equivalent_rename_display_value_is_a_noop() {
    let source = std::path::Path::new("/tmp/\u{1112}\u{1161}\u{11AB}\u{1100}\u{1173}\u{11AF}.md");
    assert!(app_host_rename_is_noop(source, "한글.md"));
}
```

Run:

```bash
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p deppy-sijo --locked --bin deppy-sijo canonical_equivalent_rename_display_value_is_a_noop -- --test-threads=1
```

Expected: helper 부재로 exit 101.

- [ ] **Step 3: 공통 helper와 UI wiring 최소 구현**

`ui/mod.rs`에 표시 전용 함수를 추가한다.

```rust
pub(crate) fn os_str_display(value: &std::ffi::OsStr) -> String {
    use unicode_normalization::UnicodeNormalization as _;
    value.to_string_lossy().nfc().collect()
}

pub(crate) fn path_display(path: &std::path::Path) -> String {
    use unicode_normalization::UnicodeNormalization as _;
    path.to_string_lossy().nfc().collect()
}

pub(crate) fn path_file_name_display(path: &std::path::Path) -> String {
    path.file_name().map(os_str_display).unwrap_or_else(|| path_display(path))
}
```

트리 행/위치/rename/delete, document tab/confirm, workspace open-file menu가 helper를 사용하게 한다. `CopyPath`, DnD, PTY path insertion, file request에는 helper를 사용하지 않는다. production rename host는 exact destination equality 또는 source file name의 NFC display와 requested name equality를 no-op으로 처리한다.

- [ ] **Step 4: NFC GREEN과 관련 그룹 실행**

Run:

```bash
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p deppy-sijo --locked --bin deppy-sijo nfd_file_name_and_path_are_displayed_as_nfc -- --test-threads=1
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p deppy-sijo --locked --bin deppy-sijo canonical_equivalent_rename_display_value_is_a_noop -- --test-threads=1
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p deppy-sijo --locked --bin deppy-sijo 한글 -- --test-threads=1
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p deppy-sijo --locked --bin deppy-sijo document -- --test-threads=1
```

Expected: focused 1/1, focused 1/1, Korean group and document group all pass.

- [ ] **Step 5: Unicode 의미 단위 커밋**

```bash
git add crates/app/src/app.rs crates/app/src/ui/mod.rs crates/app/src/ui/file_tree.rs crates/app/src/ui/workspace.rs
git commit -m "fix(app): raw 파일명과 NFC 표시 분리"
```

### Task 4: 통합 리뷰, 전체 게이트, Workstep 전달

**Files:**
- Modify: review가 지적한 source/test only
- Modify: `docs/CODEX_HANDOFF.md`
- Create: `~/Library/CloudStorage/SynologyDrive-sync_data/Obsidian-Vault/프로젝트 일지/deppy-sijo/2026-08-25 Grok 사용량과 파일명 Unicode 교정.md`

- [ ] **Step 1: 두 구현 커밋 통합 후 root 교차 리뷰**

raw identity가 display helper로 역류하지 않았는지, Grok timestamp가 failure retention을 바꾸지 않았는지, 새 테스트가 실제로 선택됐는지 확인한다.

- [ ] **Step 2: 집중·정적·전체 게이트 실행**

```bash
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p deppy-sijo --locked --bin deppy-sijo grok_usage::tests -- --test-threads=1
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p deppy-sijo --locked --bin deppy-sijo ui::file_tree::tests -- --test-threads=1
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p deppy-sijo --locked --bin deppy-sijo ui::workspace::tests -- --test-threads=1
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p terminal --locked -- --test-threads=1
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p pty --locked -- --test-threads=1
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p deppy-sijo --locked --bin deppy-sijo -- --test-threads=1
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo clippy -p deppy-sijo --all-targets --locked -- -D warnings
cargo run -p xtask --locked -- i18n-check
cargo run -p xtask --locked -- check-boundary
cargo fmt --all -- --check
git diff --check
```

Expected: all selected tests pass, only documented ignored tests remain, strict Clippy/i18n/boundary/fmt/diff exit 0.

- [ ] **Step 3: 직접 Codex 코드 리뷰와 교정**

```bash
codex review --uncommitted
```

Critical/High는 전부 수정하고 Medium은 범위 내에서 수정한다. 모든 수정은 새 RED를 먼저 관찰하고 focused GREEN 뒤 전체 게이트를 다시 실행한다.

- [ ] **Step 4: source 커밋과 Obsidian 일지**

```bash
git add crates/app/src/grok_usage.rs crates/app/src/app.rs crates/app/src/ui/mod.rs crates/app/src/ui/file_tree.rs crates/app/src/ui/workspace.rs docs/CODEX_HANDOFF.md docs/superpowers/plans/2026-08-25-grok-usage-and-unicode-corrections.md
git commit -m "fix(app): Grok 사용량과 파일명 표시 교정"
```

일지에는 RED/GREEN, Codex findings/반영, 최종 SHA, 패키징 해시를 기록한다.

- [ ] **Step 5: Developer-ID 서명 재빌드·검증**

```bash
DEPPY_SIGN_IDENTITY='Developer ID Application: VectorNine INC (ZDTU5LS35K)' CARGO_NET_OFFLINE=true CARGO_BUILD_JOBS=2 sh scripts/package-macos.sh
codesign --verify --deep --strict --verbose=2 'target/bundle/Deppy Sijo.app'
codesign -dv --verbose=4 'target/bundle/Deppy Sijo.app' 2>&1
unzip -t 'target/bundle/Deppy Sijo-macos.zip'
shasum -a 256 'target/bundle/Deppy Sijo.app/Contents/MacOS/deppy-sijo' 'target/bundle/Deppy Sijo.app/Contents/Resources/deppy-helper' 'target/bundle/Deppy Sijo-macos.zip'
```

Expected: package script, deep/strict signature, archive test exit 0; identifier `app.vector9.deppy-sijo`, team `ZDTU5LS35K`.

- [ ] **Step 6: exact bundle 재실행**

현재 exact executable PID를 다시 조회·검증한 뒤 SIGTERM으로 종료하고 bounded wait 후 새 bundle을 `open -n`으로 실행한다. 새 PID가 exact executable, PPID 1, 두 번의 생존 검사에서 유지되는지 확인한다.

```bash
pgrep -fl '/Users/jr/Desktop/projects/deppy-sijo/target/bundle/Deppy Sijo.app/Contents/MacOS/deppy-sijo'
open -n 'target/bundle/Deppy Sijo.app'
```

Expected: 이전 PID 종료, 새 exact bundle PID 하나가 안정적으로 실행.
