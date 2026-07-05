# PR-R08 Findings

## Summary
- 전체 판정: Pass with Issues
- Critical: 0
- High: 2
- Medium: 5
- Low: 2

hidden viewport 금지, Warm 상태 viewport 중단/복귀, file watcher throttle, PTY output bounded queue 등 일부 기반은 존재한다. High risk는 idle repaint/approval polling과 folder tree 100k flat directory scalability이다. PTY output 자체는 bounded로 확인되며, 남은 backpressure gap은 local command/status/input path와 사용자 가시성이다.

## Scope Reviewed
- 검토한 파일/모듈: `crates/app/src/{app,config,perf}.rs`, `crates/app/src/ui/{workspace,file_tree,settings}.rs`, `crates/runtime/src/{in_process,command,persistence,remote}.rs`, `crates/session/src/session.rs`, `crates/terminal/src/*`, `crates/storage/src/{logs,db}.rs`, `crates/pty/src/lib.rs`, `crates/persist/src/repo.rs`, `crates/storage-core/src/lib.rs`
- 실행한 명령: `cargo check --workspace --all-targets` pass, `cargo test --workspace --no-run` pass, PR-R08 `rg` search
- 확인한 테스트: compile only. 실제 100k 파일, RSS/CPU, 10MB/min 장시간 부하 테스트는 수행하지 않음.

## Findings

### Finding 1
Severity: High
Area: idle repaint / CPU
Files: `crates/app/src/app.rs`
Evidence: `APPROVAL_POLL_MS` 주석이 idle에서도 500ms 주기 frame을 예약한다고 명시하고 `logic()`이 매번 `ctx.request_repaint_after(Duration::from_millis(Self::APPROVAL_POLL_MS))`를 호출한다.
Why it matters: 빈 앱에서도 2fps repaint와 500ms approval DB polling이 유지되어 "idle repaint 없음" 기준과 충돌한다.
Reproduction: 빈 앱 실행 후 `DEPPY_FRAME_STATS=1` 또는 profiler로 idle frame 발생을 확인한다.
Suggested fix: pending approval source가 활성일 때만 backoff polling하거나 별도 watcher/thread에서 상태 변화 시 wake한다. pending이 없으면 repaint 예약을 중단한다.
Suggested test: 빈 앱 상태에서 approval polling이 `request_repaint_after`를 재등록하지 않는 테스트와 pending approval 삽입 시 wake 테스트.

### Finding 2
Severity: High
Area: folder tree scalability
Files: `crates/app/src/ui/file_tree.rs`
Evidence: `read_children()`이 한 디렉터리의 모든 entry를 동기 `std::fs::read_dir`로 읽고 `sort_nodes()`로 전체 정렬한다. `show_rows`는 렌더링만 가상화한다.
Why it matters: 100k files가 단일 directory에 몰리면 UI thread freeze가 발생할 수 있다.
Reproduction: 100k 파일이 있는 directory를 workspace root로 지정하거나 펼친다.
Suggested fix: directory listing을 background worker로 넘기고 generation token/cancel로 stale 결과를 폐기한다. 큰 directory는 page/chunk 로딩 또는 cap+more sentinel을 둔다.
Suggested test: 100k 파일 perf smoke 또는 mock `read_dir` provider로 UI thread blocking budget 검증.

### Finding 3
Severity: Medium
Area: idle worker wake / config normalization
Files: `crates/app/src/config.rs`, `crates/app/src/ui/settings.rs`, `crates/runtime/src/in_process.rs`
Evidence: UI/주석은 output batch 16~50ms를 제시하지만 load normalization은 `1..=1000`으로 clamp하고 runtime worker는 `recv_timeout(self.batch)`로 깨어난다.
Why it matters: 수동 config에서 1ms worker wake가 가능해 idle CPU 기준을 깨뜨린다.
Reproduction: config `[performance] output_batch_ms = 1` 설정 후 재시작.
Suggested fix: normalization 하한을 UI와 같은 16ms로 맞춘다.
Suggested test: `output_batch_ms = 0` 또는 `1`이 16으로 정규화되는 테스트.

### Finding 4
Severity: Medium
Area: terminal snapshot / repaint cost
Files: `crates/session/src/session.rs`, `crates/terminal/src/{alacritty_backend,renderer_egui}.rs`
Evidence: terminal backend는 dirty rows를 계산하지만 session layer는 `dirty: bool`만 보존한다. `viewport_snapshot()`은 매번 `cols * rows` `Vec<TerminalCell>`을 새로 만들고 `dirty_ranges`는 비어 있다. renderer는 모든 visible cell을 순회한다.
Why it matters: active visible pane 고출력 상황에서 전체 snapshot allocation/full paint 비용이 남는다.
Reproduction: hidden harness와 active 지속 output을 동시에 발생시키고 profiler로 `viewport_snapshot()` allocation/draw loop 확인.
Suggested fix: `TerminalChangeSet.dirty_rows`를 session/runtime event까지 전달하고 `dirty_ranges`를 채운다. renderer는 dirty range/clip 기반 최소 paint.
Suggested test: 한 줄만 바뀌는 output에서 dirty range가 해당 row만 포함되는지 테스트.

### Finding 5
Severity: Medium
Area: terminal cache / RAM budget
Files: `crates/app/src/config.rs`, `crates/app/src/ui/settings.rs`, `crates/terminal/src/alacritty_backend.rs`, `crates/runtime/src/in_process.rs`
Evidence: visible scrollback 설정은 100,000 lines까지 허용한다. hidden 상태는 1,000 lines로 줄이나 설계의 byte budget은 없다. exited backend cap은 24개지만 global cache/RSS budget은 없다.
Why it matters: line count만으로는 workspace 5개, pane 20개, hidden session 10개 조합의 RAM 상한을 보장하기 어렵다.
Reproduction: scrollback 100,000 설정 후 여러 session에 넓은 output 발생.
Suggested fix: approximate byte accounting과 visible/hidden/global budget 초과 시 scrollback/cache drop을 추가한다.
Suggested test: hidden 전환 시 line cap과 byte budget 이하로 줄어드는 테스트.

### Finding 6
Severity: Medium
Area: SQLite write batching / UI DB cost
Files: `crates/runtime/src/persistence.rs`, `crates/persist/src/repo.rs`, `crates/storage/src/db.rs`
Evidence: session spawn/exit은 즉시 `upsert_session()` 호출, mux layout 저장은 구조 변경마다 전체 transaction, storage facade write API는 UI 경로에서 단건 execute를 수행한다.
Why it matters: status update debounce, notification insert batch, log offset update batch, UI thread DB write 금지 기준이 아직 충족되지 않았다.
Reproduction: pane split/close/select burst와 approval polling을 동시에 발생시켜 SQLite busy/frame spike를 관찰한다.
Suggested fix: runtime persistence debounce/batch queue, layout 저장 coalescing, UI write background DB actor.
Suggested test: N회 split/resize burst가 DB layout save bounded 횟수로 coalescing되는 테스트.

### Finding 7
Severity: Medium
Area: queue / local backpressure surface
Files: `crates/runtime/src/in_process.rs`, `crates/pty/src/lib.rs`
Evidence: PTY output queue는 `sync_channel(64)`로 bounded이고 Viewport event는 per-session slot으로 coalesce된다. 그러나 local command/state event channel과 PTY input writer channel은 unbounded이며 backpressure UI event/badge가 없다.
Why it matters: local control/status/input path의 유계 정책과 사용자 가시성이 부족하다.
Reproduction: subscriber가 event drain하지 않는 상태에서 status churn 또는 큰 paste 반복.
Suggested fix: command/input/status queue 용량과 overflow 정책을 명시하고 `RuntimeEvent::BackpressureChanged` 같은 상태 event를 추가한다.
Suggested test: slow subscriber에서 viewport slot 유지, status/control bounded/coalesced, PTY output saturation backpressure event 테스트.

### Finding 8
Severity: Low
Area: process/RSS monitor
Files: workspace `Cargo.toml`, `crates/app/src/perf.rs`, `crates/pty/src/lib.rs`
Evidence: `sysinfo` dependency가 없고 RSS/CPU/process tree aggregation이 없다.
Why it matters: 현재 비용은 0에 가깝지만 PR-B07 Process Resource Monitor acceptance와 거리가 있다.
Reproduction: app 안에서 workspace/session별 RSS/CPU UI/API 검색.
Suggested fix: PR-B07에서 low-frequency process sampler actor를 추가하고 idle sampling interval/backoff를 명시한다.
Suggested test: sampler disabled/enabled 상태의 idle repaint/CPU 및 polling interval 테스트.

### Finding 9
Severity: Low
Area: Remote plain transport backpressure
Files: `crates/runtime/src/remote.rs`
Evidence: TLS command sending has bounded `try_send` backpressure, but plain remote `send_command()` writes directly under a mutex and can block the caller on a slow socket or large paste.
Why it matters: Remote attach is not the main local UX path yet, but plain transport slow consumers can turn large paste or command forwarding into caller blocking.
Reproduction: Use plain remote transport with a slow or stalled socket and send repeated large `WriteInput` commands.
Suggested fix: Give plain remote command send timeout/nonblocking queue parity with TLS before remote attach is productized.
Suggested test: slow plain remote writer returns bounded timeout/backpressure signal instead of blocking indefinitely.

## Second Pass Update
- Finding 7 wording is corrected: do not describe PTY output as unbounded. The remaining issue is local command/status/input policy and user-visible backpressure signaling.
- Added Finding 9 as a low-priority remote slow-consumer gate for plain remote command send.

## Regression Risks
- approval polling event-driven 전환은 외부 proxy pending approval wake를 놓칠 수 있다.
- async folder tree는 DnD path insert, rename/delete refresh, watcher partial reload 순서가 깨질 수 있다.
- dirty range 최적화는 CJK/wide char, cursor-only, alt screen, scrollback offset에서 stale rendering 위험이 있다.
- SQLite batching은 crash 직전 layout/session status 유실 가능성을 만든다.

## Recommended Build PRs
- PR-B03: Folder Tree Scalability & DnD Hardening
- PR-B09: Terminal Cache Budget Manager
- PR-B10: Output Pipeline Backpressure
- PR-B15: SQLite Write Batching
- PR-B07: Process Resource Monitor
- Small Build PR: approval polling idle repaint 제거, `output_batch_ms` lower bound 정렬

## Open Questions
- "Active pane만 render"와 "visible pane만 paint" 용어 충돌을 어떻게 정리할 것인가?
- approval pending 감지는 DB polling 유지인가, proxy GUI wake 신호인가?
- 100k files 시나리오는 단일 디렉터리 100k인지 전체 tree 100k인지 분리해야 한다.
