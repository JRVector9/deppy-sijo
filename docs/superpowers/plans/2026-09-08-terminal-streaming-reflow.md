# Terminal Streaming Reflow Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 압축 scrollback의 전체 원시 확장 없이 기존 Alacritty resize 결과를 보존한다.

**Architecture:** 기존 generic resize를 동등성 기준으로 유지하고 Cell 전용 streaming resize를 추가한다. 입력 행은 한 번에 하나만 복원하고 완성 출력 행은 즉시 압축한다. 출력은 보존 한도 bounded deque에 쌓고 수정 가능한 마지막 행과 폭 이하 carry만 원시 셀로 유지하며, 최종 visible 영역만 복원한다. primary/alt 선택은 Term의 기존 조건을 그대로 사용한다.

**Tech Stack:** Rust, vendored alacritty_terminal 0.26, terminal crate, cargo test/clippy, Codex CLI.

---

사용자 승인 범위는 PR C 구현·검증·push·main 대상 PR 생성까지다. 이미 승인된 설계를 순서대로 직접 실행한다. app/runtime/UI/protocol과 앱 빌드·실행은 제외한다. 디스크 저장, 전역 예산 정책 및 협력적 CPU 스케줄링도 제외한다.

### Task 1: 실제 resize 압축 보존 RED

**Files:**
- Modify/Test: `third_party/alacritty_terminal-0.26.0/src/term/mod.rs`
- Modify: `docs/CODEX_HANDOFF.md`

- [x] **Step 1: 회귀 추가.** 2,000행 scrollback을 만든 실제 Term을 압축하고 resize 후 `assert!(term.grid().compressed_row_count() > 1000)`로 cold history 보존을 검증한다. 기존 구현은 inflate_all 때문에 실패해야 한다.
- [x] **Step 2: RED 실행.** `CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/private/tmp/deppy-streaming-target-20260908 cargo test --manifest-path third_party/alacritty_terminal-0.26.0/Cargo.toml --locked streaming_resize -- --test-threads=1`. 기대: 압축 행 수 assertion 실패.

### Task 2: 압축/원시 행 streaming 경로

**Files:**
- Create: `third_party/alacritty_terminal-0.26.0/src/grid/streaming_resize.rs`
- Modify: `third_party/alacritty_terminal-0.26.0/src/grid/{mod.rs,resize.rs,storage.rs}`
- Modify: `third_party/alacritty_terminal-0.26.0/src/term/mod.rs`
- Modify: `crates/terminal/src/alacritty_backend.rs`

- [x] **Step 1: 행 전송 API 추가.** Storage의 rezero/truncate 뒤 raw와 compressed 슬롯을 소유권으로 함께 꺼내 iterator로 순회한다. 각 슬롯은 `match compressed { Some(row) => row.decode(columns), None => raw }`로 한 행만 복원한다. 최종 설치도 두 배열을 함께 설치한다.
- [x] **Step 2: 출력 builder 추가.** 완성 행은 `CompressedRow::encode(&row, row.len())`로 보관하고 마지막 수정 가능 행만 원시 형태로 남긴다. grow의 이전 행 참조와 shrink의 wrap flag 설정 순서를 유지한다. 최종 reverse/truncate는 셀 복사 없이 metadata만 이동한다.
- [x] **Step 3: resize 연결.** `self.grid.resize_streaming(!is_alt, num_lines, num_cols)` 및 inactive의 반대 조건으로 연결한다. 높이 변경은 기존 grow_lines/shrink_lines를 재사용하고 새 visible 영역만 복원한다. 유효 폭은 1..=u16::MAX, 행 및 scrollback 합은 i32 범위, `checked_add/checked_mul` preflight를 mutation 전에 수행한다.
- [x] **Step 4: cold 유지.** 출력 history는 압축 상태로 설치하고 화면 행만 복원한다. terminal backend resize 뒤 HOT 정책으로 cold 압축을 유지한다. generic oracle에는 압축 입력을 전달하지 않는다.
- [x] **Step 5: GREEN 실행.** Task 1 명령 및 전체 vendor tests. 기대: 새 회귀와 기존 resize/cursor/alt tests 모두 통과.
- [x] **Step 6: checkpoint 기록 후 커밋.** `git commit -m "perf(terminal): 압축 이력을 유지하는 스트리밍 리플로 구현"`.

### Task 3: 동등성 및 scratch 상한

**Files:**
- Test: `third_party/alacritty_terminal-0.26.0/src/grid/streaming_resize.rs`
- Test: `crates/terminal/src/alacritty_backend.rs`

- [x] **Step 1: differential cases 추가.** 동일 grid 두 개 중 기준은 inflate_all+generic resize, 대상은 streaming resize. 결과 행은 read_line으로 비교하고 cursor/saved_cursor/display_offset/input_needs_wrap을 직접 비교한다. soft-wrap/hard newline/wide/combining/hyperlink 및 primary/alt, 연속 확대·축소·행 변경을 포함한다.
- [x] **Step 2: 실제 작업량 측정.** builder에서 관찰한 최대 원시 작업 셀 수를 기록한다. 10k와 100k 연속 soft-wrap 입력에서 같은 폭 조합의 scratch 상한이 동일하며 `peak_scratch_cells <= 8 * (old_columns + new_columns)`임을 assertion한다. 최종 history raw 수는 0이어야 한다. 경과 시간·압축 byte 수를 출력하는 ignored 측정을 실제 실행한다.
- [x] **Step 3: 위 테스트가 새 결함을 드러내면 RED 증거를 기록한 뒤 수정하고 재실행한다.** 정상 압축 저장 자체의 크기와 metadata O(history), 원시 scratch O(width)를 구분해서 기록한다.

### Task 4: review·gate·공유

**Files:**
- Modify: `docs/CODEX_HANDOFF.md`
- Modify: 이 plan의 체크박스

- [x] **Step 1: 정적 Codex 리뷰.** `codex review --uncommitted` 또는 source commit을 대상으로 실행한다. 실제 소스만 리뷰하며 앱 실행/빌드/편집을 금지하는 prompt를 전달한다. 지적을 수정하고 해당 회귀를 실행한다.
- [x] **Step 2: 최종 gate.** vendor 전체 tests 및 strict clippy, `cargo test -p terminal -p session --locked -- --test-threads=1`, 해당 crate strict clippy, `cargo fmt --all -- --check`, 신규 vendor 파일 rustfmt check(기존 상류 포맷 보존), `git diff --check`. jobs=2, 앱 빌드 없음. 빌드 정체는 ps로 정확 PID를 먼저 확인한다.
- [x] **Step 3: 최신 main 반영.** 필요하면 일반 merge만 사용하고 소스 영향에 맞춰 gate를 재실행한다. rebase/force push 금지.
- [x] **Step 4: 한국어 커밋/push/PR.** `git push -u origin perf/terminal-streaming-reflow`; `gh pr create --base main --head perf/terminal-streaming-reflow --title 'perf(terminal): 압축 이력의 리사이즈 메모리 피크 제한' --body-file /private/tmp/deppy-streaming-reflow-pr.md`. 실제 결과와 한계를 PR/handoff에 기록한다.

## 실행 결정 및 실제 측정

- Task 2의 최초 Vec 출력 방식은 10k행 200→2에서 추가 heap peak95,623,738 bytes로 RED였다. bounded VecDeque로 고쳐813,400 bytes GREEN. 출력 행도 최종 보존 한도를 초과해 쌓지 않는다.
- 공개 ReflowMetrics는 동시에 소유한 원시 작업 버퍼의 capacity 관찰 최대치다. 기존 resident history/visible grid와 codec allocations는 제외한다. 별도 test allocator의 실제 net live heap peak로 보완한다. process RSS 또는 allocator 내부 realloc 순간 피크의 엄밀한 상한을 주장하지 않는다.
- 100k행 release 측정: 80→120 44ms/5,608,024B; 200→100 96ms/6,811,032B; 500→80 293ms/7,651,808B; 200→2 1098ms/7,991,800B. scratch280/400/1440/594 cells는 10k에서도 동일했다.
- preflight는 checked 산술/좌표 검증이며 OS OOM 복구나 전역 cache budget 예약이 아니다. 전체 resident 예산과 CPU cooperative scheduling은 후속 범위다.
- vendor 전체 formatter는 기존 upstream 포맷을 대량 변경하므로 그 변경만 회수했다. workspace fmt와 새 vendor 파일 rustfmt check를 적용한다. codec 기존 테스트의 clippy needless_range_loop4개는 동등한 iterator로 최소 수정했다.

- 최종 Codex 리뷰 P2(상위 Term preflight 이전 vi cursor 변경)는 실제 RED→GREEN으로 수정했다. Term이 양쪽 grid를 먼저 검증하도록 공유 API를 추가했고 제한 재리뷰에서 남은 확정 결함0. 최종 vendor152 unit+45 ref+1 memory+1 doc, terminal87/session55 및 strict clippy PASS.

- 공유 완료: source commit 1e88de09c7e4199fc2a0c41c92849ed31aa469a3; PR https://github.com/JRVector9/deppy-sijo/pull/160 . 이후 handoff/완료 체크 변경만 문서 커밋한다.
