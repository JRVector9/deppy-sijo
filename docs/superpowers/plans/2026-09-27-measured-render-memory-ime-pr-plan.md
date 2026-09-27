> **2026-09-27 구현 상태:** 7개 제품 코드와 허용된 headless 전후 테스트/실측, source 리뷰, release 재빌드를 완료했다. GUI/native IME는 AGENTS.md의 실행 허락 미수신으로 미실행이다. 기존 Workspace 실패2건과 cold/full 비용을 [최종 보고서](../../reviews/2026-09-27-seven-improvements-final.md)에 기록했다. 아래 조사 단계의 문장은 당시 상태를 보존한다.

# 실측 기반 렌더링·메모리·한글 개선 PR 계획

> **For agentic workers:** 구현 요청을 받은 뒤 `execute-plan` 절차로 PR별 작업을 진행한다. 사용자 또는 적용 규칙의 허용 없이 서브에이전트를 만들거나 Deppy를 실행·재실행하지 않는다. 아래 체크박스는 계획이며 완료 표시가 아니다.

**Goal:** 실제 관찰과 최신 코드 측정으로 확인한 비용·문자 유실을 독립적으로 검증하고 개선한다.

**Architecture:** 현재 egui/wgpu, Alacritty, worker/UI 분리 구조를 유지한다. 비용이 반복되는 공통 저장소부터 줄이고, 문자 정확성과 IME 표시를 분리하여 고친다. 셀 표현과 행 공유는 원격 wire 계약까지 확인한 후 진행한다.

**Tech Stack:** Rust, egui/eframe 0.36.1, wgpu/Metal, vendored Alacritty 0.26, macOS winit 0.30.13, postcard runtime protocol.

---

## 1. 조사 범위와 현재 완료 상태

- 요청은 **실측·코드 확인·PR 계획 보고**다. 이번 단계에서 제품 수정·커밋·push·GUI 실행은 하지 않았다.
- 최신 코드 기준: `feat/cloud-agent-mcp`, `78f054b9` (`/Users/jr/Desktop/projects/deppy-sijo-cloud-agent-mcp`). `cargo build -p deppy-sijo --release`를 실제 재실행해 exit0, 0.41초를 확인했다. GUI는 시작하지 않았다.
- 실행 중인 실제 앱: PID71086, `/Users/jr/Desktop/projects/deppy-sijo-agent-wait-audit/target/debug/deppy-sijo`, 2026-09-24 시작. 해당 작업 트리 HEAD는 `2a1583d1`이고 소스가 dirty다. 현재 파일 상태로 실행 바이너리의 정확한 소스를 역추정하지 않는다.
- 이 앱을 종료하거나 입력을 보내지 않고 `ps` 31회/30.23초, `sample` 5초, `vmmap -summary`를 실행했다. 현재 세션의 출력/작업/가시성은 통제하지 않았다. **idle 벤치로 부르지 않는다.**
- 최신 제품 backend/renderer를 링크하는 저장소 밖 release probe를 다시 실행했다. 별도의 독립 workspace manifest와 로그를 `docs/reviews/measurements/2026-09-27-render-memory-ime/`에 보존했다.
- 별도 격리 release GUI 벤치 실행은 AGENTS.md 때문에 허락을 요청했으며 아직 실행하지 않았다. 데이터/설정/lock은 앱의 기존 `DEPPY_RENDER_BENCH` 경로에서 PID별 temp 디렉터리로 분리된다. `$HOME`은 변경하지 않는다.
- 네이티브 입력기 타이핑, 최신 release의 GUI frame p95, GPU 실행 시간, 변경 전후 개선율은 아직 측정하지 않았다. 이 계획의 목표 수치는 **통과 기준 제안**이며 달성 결과가 아니다.

## 2. 실제 관찰

| 측정 | 결과 | 해석 범위 |
| --- | --- | --- |
| 실행 중인 debug 앱 CPU delta | 30.23초 동안 11.97 CPU초, 평균39.59% | 코어1개 기준; 자식 프로세스 제외; 최신 release/idle 지표 아님 |
| debug 앱 RSS | 375.06~375.80MiB | 30초 관찰; 장기 누수 여부 판정 불가 |
| debug 앱 physical footprint | vmmap842.6M, sample853.5M, 기존 peak999.7M | RSS와 다른 OS 집계; 그래픽 자원 포함 |
| debug 앱 그래픽 관련 resident 영역 | owned unmapped(graphics)342.3M, IOAccelerator202.0M | 영역별 값; 힙 또는 텍스처 누수로 단정하거나 합산해 총 GPU 메모리로 부르지 않음 |
| debug 앱 call stack | runtime worker8개에서 `Session::cache_footprint` 관찰 | 실제 실행 경로 확인; inclusive wall sample을 CPU 점유율로 환산하지 않음 |
| 최신 backend footprint 조회 | history1k:1.22~1.28µs /5k:6.77~6.83µs /20k:27.93~29.48µs | 각1000회 평균; 할당0; 히스토리 길이에 비례하는 비용 |
| 최신 backend snapshot300×80 | live3회/775,217B; compressed400행 offset83회/1,351,216B | snapshot당 누적 요청 할당, RSS 아님; 100회 평균 |
| 최신 backend snapshot 시간 | live 약85~97µs; compressed 약114~118µs | release fixture CPU; 앱 전체 frame 시간 아님 |
| 최신 renderer cached300×80 | ASCII81 shapes/0.18ms; 한글11,852 shapes/0.51~0.54ms | D2Coding,100회; draw+tessellation CPU; GPU drawcall 수 아님 |
| 셀 표현 | 현재16B; 별도 정상 정렬 flags 모델12B | 셀 payload25% 축소 가능; 앱 전체 메모리25% 아님 |
| 실제 출력→snapshot→복사 | `한` 유지; `가ᇹ`→`ᄀ`, `a + U+0301 + U+0308`→`a` | 재현된 grapheme 유실; 현대 한글 IME commit 오류와 구분 |
| IME 소스 확인 | `Preedit { text, .. }`에서 active range 버림 | 기능 제약 확정; 후보창 오동작/지연의 native 재현은 미완료 |

주요 재현 파일: `live-debug-summary.json`, `sample-path-summary.json`, `latest-backend-renderer.log`, `probe/{Cargo.toml,Cargo.lock,src/main.rs}`. raw OS 로그는 `/tmp/deppy-live-{sample,vmmap}-20260927.txt`다.

## 3. PR 분할과 순서

| PR | 범위 | 근거 수준 | 의존성 | 완료 기준 요약 |
| --- | --- | --- | --- | --- |
| 1 | 히스토리 메모리 집계 O(1) | 실제 앱 stack + 최신 backend 실측 | 독립 | 집계 불변식 유지, history20배 증가에도 조회 시간 거의 일정 |
| 2 | 압축 행 scratch 재사용·메타데이터 조회 | 최신 backend 할당 실측 + 호출부 확인 | PR1 이후 권장, 논리적 독립 | 동일 ASCII fixture83회→5회 이하, metadata-only snapshot0회 |
| 3 | 결합 문자 표시·복사 보존 | 실제 제품 API로 유실 재현 | 독립; PR4/6/7보다 먼저 | 옛한글·다중 결합 문자 무손실; 원격 delta 포함 |
| 4 | 한글/CJK text run 병합 | 실제 renderer shape·CPU 실측 | PR3 | glyph 셀 위치 유지, 고정 fixture shape90% 이상 감소 목표 |
| 5 | IME range·caret·후보 위치 | 소스 확정, native 검증 필요 | 독립; PR3/4 이후 통합검증 | char range 보존, 같은 pass 표시, commit/소유권 회귀 없음 |
| 6 | snapshot 셀16B→12B | 실제 size_of + 별도 모델 확인 | PR3 이후 | payload25% 감소, serde/wire 의미 보존 |
| 7 | 변경 행만 만드는 snapshot | full conversion/할당 실측 | PR2/3/6 이후 | dirty1 row 공유, 메타데이터 변경 셀 복사0, 누적 dirty 유실 없음 |

권장 착수 순서: **PR1→PR2→PR3→PR4→PR5→PR6→PR7**. PR3는 사용자에게 보이는 확정 결함이라 PR1/2와 별도로 앞당겨 착수할 수 있다. 각 PR은 제품 동작과 측정 결과가 완결된 상태로 리뷰한다.

## PR1 — 히스토리 메모리 집계의 반복 순회 제거

**위치와 역할**

- `third_party/alacritty_terminal-0.26.0/src/grid/storage.rs:408`: 압축 heap sum와 논리 compressed count를 저장소 mutation 때 갱신.
- `third_party/alacritty_terminal-0.26.0/src/grid/compressed.rs:63`: heap estimate를 slot 생성/삭제 mutation에서만 계산. 매 행에 cache field를 추가하지 않는다.
- `third_party/alacritty_terminal-0.26.0/src/grid/tests.rs`: stale 슬롯·resize·rotation·inflate 집계 검증.
- `crates/terminal/src/alacritty_backend.rs:579`: active/inactive grid 합산 계약 유지.
- `crates/runtime/src/in_process.rs:4232`: budget/archive 호출부; PR1에서는 archive 정책 변경 금지.

**작업**

- [x] 기준 fixture에서 history1k/5k/20k 조회 비용을 세 번 기록한다.
- [x] 테스트용 느린 full scan을 oracle로 두고 encode/replace/drop/clear/inflate/shrink/grow/rotate 이후 저장 합계와 대조한다. **할당된 stale cache heap**과 **논리 범위의 compressed count**는 서로 다른 합계다.
- [x] 압축 row의 heap estimate를 mutation 지점에서 계산해 storage 합계를 덧셈/뺄셈한다. 조회는 저장소의 두 합계를 읽는다. per-row 추가 필드를 피하고 grid당 counters의 고정 비용만 허용한다. visible grid·inactive alt-screen도 계속 계산에 포함한다.
- [x] overflow/underflow 또는 갱신 누락이 예산 축소로 이어지지 않도록 debug invariant와 실제 stale 슬롯 회귀를 확인한다.
- [x] 같은 probe와 budget/archive 테스트를 실행하고 source diff를 리뷰한 뒤 독립 커밋한다.

**검증 명령**

```bash
cargo test --manifest-path third_party/alacritty_terminal-0.26.0/Cargo.toml --lib compressed
cargo test -p terminal cache
cargo test -p runtime cache
```

통과 기준: 집계 oracle와 항상 일치, 기존 stale-slot 검증 통과. 같은 환경에서 history1k 대비20k의 조회 시간이2배 이내라는 **수동 benchmark 목표**를 둔다. µs 임계값을 flaky 단위 테스트로 만들지 않는다. 현재 전체 CPU39.6%의 특정 비율이 줄 것이라는 약속은 하지 않는다.

## PR2 — 압축 행 읽기 할당과 메타데이터용 snapshot 제거

**파일**

- `third_party/alacritty_terminal-0.26.0/src/grid/compressed.rs:140`: 기존 Row에 복원하는 `decode_into` 공통 경로.
- `third_party/alacritty_terminal-0.26.0/src/grid/storage.rs:391`: `read_line`에서 scratch capacity 재사용.
- `crates/terminal/src/backend.rs:180`, `crates/terminal/src/alacritty_backend.rs`: snapshot 없는 offset/alt-screen 조회와 타 backend 구현.
- `crates/session/src/session.rs:445`, `:556`: replay·prompt jump에서 경량 조회 사용.
- 같은 파일의 codec/session 테스트: 행 잔재 및 동작 회귀.

**작업**

- [x] 연속 두 행 중 첫 행만 hyperlink/zerowidth/underline을 갖는 codec fixture를 추가한다. 두 번째 행에 이전 extras가 남으면 실패해야 한다.
- [x] 폭이 같으면 Row buffer를 유지하고 셀·occ·extras를 완전히 초기화해 복원한다. 폭이 바뀌면 그때만 capacity 조정한다. 기존 `decode`도 같은 codec을 사용한다.
- [x] metadata-only backend 조회를 추가한다. 지원하지 않는 backend는 명시적 unsupported/fallback 계약을 둔다. alt-screen 복원 보존 동작은 유지한다.
- [x] 압축 snapshot, copy/search, replay, prompt jump를 검증하고 동일 fixture 할당을 다시 측정한다.
- [x] 리뷰 후 독립 커밋한다.

```bash
cargo test --manifest-path third_party/alacritty_terminal-0.26.0/Cargo.toml --lib compressed
cargo test -p terminal deppy_압축
cargo test -p session scroll
cargo test -p session ansi_replay
```

목표: extras 없는300×80 compressed fixture에서83→5회 이하. 희소 extras 복원에 필요한 실제 할당은 별도 표시하며 모든 Unicode 행이 무조건3회라는 계약을 만들지 않는다. metadata-only 두 경로는 테스트 backend의 snapshot 호출 횟수가0이어야 한다. RSS 절감률은 별도 앱 측정 전까지 제시하지 않는다.

## PR3 — 옛한글·결합 문자 표시와 복사 유실 수정

**파일**

- `crates/terminal/src/alacritty_backend.rs:29`, `:402`: base/zerowidth 전체 문자열 보존.
- `crates/terminal/src/viewport_snapshot.rs`: 일반 셀은 고정 크기, 여러 scalar만 희소 부가 정보로 보관.
- `crates/terminal/src/renderer_egui.rs:811`, `:1189`: shaping과 selection copy에 동일 accessor 사용.
- `crates/runtime/src/protocol.rs:344`, `:384`: keyframe/row delta의 grapheme 부가 정보·비교·재구성.
- `crates/runtime/src/remote.rs`: 메시지 검증/구형 peer 처리. 변경 wire 의미에 맞춰 버전 gate를 갱신한다.

**작업**

- [x] production backend→snapshot→`selection_text`에 `가ᇹ`와 `a\u{0301}\u{0308}` 무손실 기대를 넣어 현재 유실을 RED로 고정한다. 단순 현대 한글/ASCII는 baseline으로 둔다.
- [x] NFC 단일 scalar는 현재 빠른 경로 유지. 나머지는 희소 grapheme storage로 전달하며, 숨김 문자나 wide spacer의 기존 의미를 보존한다.
- [x] 희소 데이터가 바뀐 행도 wire delta의 변경 행으로 잡히게 한다. 문자/바이트/원격 크기 한도를 검사한다.
- [x] 표시·선택·복사·검색 기대를 실제 결과와 비교한다. 검색은 원래 backend의 문자열 정책과 맞추고 정상화 정책을 임의로 바꾸지 않는다.
- [x] 원격 keyframe/delta roundtrip과 구버전 거부를 검증하고 리뷰·커밋한다.

```bash
cargo test -p terminal selection
cargo test -p terminal search
cargo test -p runtime protocol
cargo test -p runtime old_peer
```

완료 기준: 원래 문자열의 grapheme 의미가 display/copy에서 유지되고 wire roundtrip에도 동일. NFC를 적용한 결과를 기대할 경우 정확한 정책을 fixture에 명시한다. ASCII snapshot에 셀별 String 비용을 추가하지 않는다.

## PR4 — 한글/CJK 문자별 galley를 연속 run으로 병합

**파일**

- `crates/terminal/src/renderer_egui.rs:863`: wide 문자 분기와 spacer 처리.
- 같은 파일 `PendingTextRun:921`, `fit_galley_to_cells:969`: 문자열 길이와 셀 advance를 분리.
- 같은 파일의 render/selection test와 측정 fixture: cell position·shape 수 검증.

**작업**

- [x] 같은 스타일의 `한글가나다`가 한 run을 공유하도록 RED fixture를 만든다. 속성/색 전환, ASCII/CJK 혼합, emoji, 결합 문자, 폰트 fallback은 별도 경계를 기대한다.
- [x] wide spacer를 이전 owning glyph의 셀 폭으로 소비하고 연속 run을 유지한다. grapheme별 advance를 사용해 누적 glyph 위치를2셀씩 맞춘다.
- [x] 축소 배율·행 끝·underline/strikeout·selection·원형 숫자 크기 등 기존 fixture를 확인한다.
- [x] 같은 D2Coding300×80 fixture를 세 번 반복해 shapes와 draw+tess CPU를 비교하고 리뷰·커밋한다.

```bash
cargo test -p terminal renderer_egui
cargo test -p terminal --release render_tessellation_bench -- --ignored --nocapture
```

목표: 현재11,852 shapes의90% 이상 감소. 셀 위치·선택 범위가 정확해야 하며 개선율이 낮더라도 잘못된 병합을 허용하지 않는다. shape 감소를 GPU drawcall 감소로 환산하지 않는다. native GUI 허락 후 한글 화면의 UI/paint 분리 측정으로 실제 효과를 판단한다.

## PR5 — IME 조합 범위와 후보창 위치 보존

**파일**

- `crates/app/src/ui/workspace.rs:1829`, `:7518`: preedit text/char range/owner를 함께 관리.
- 같은 파일 `:7062`: 그리기 전에 해당 pass의 조합 상태를 확정하는 순서.
- `crates/terminal/src/renderer_egui.rs:704`, `:733`: 단일 galley, 내부 caret/선택 표시, 오른쪽 경계 fitting과 IME cursor rect.
- 기존 workspace 한글/key reconciliation 및 renderer IME 테스트.

**작업**

- [x] `Preedit` 범위가 `2..3`일 때 모델·caret 위치가 보존되는 RED를 만든다. 빈 문자열/범위 없음/범위 초과/취소를 각각 검증한다.
- [x] UTF-8 byte 또는 macOS UTF-16 값으로 재해석하지 않고 egui의 char range를 사용한다. owner/session이 달라지면 이전 조합을 잘못 넘기지 않는다.
- [x] 표시 상태 갱신을 같은 pass에 수행하되 PTY commit 전송은 기존 reconciliation 한 번만 통과시킨다.
- [x] 한 번 layout한 galley로 배경·조합 문자열·선택을 그린다. 조합 caret 기준의 후보창 rect를 viewport 안으로 맞춘다.
- [x] 공식 IME 소유권/숨김 cursor/터미널과 TextEdit 포커스 전이 테스트를 실행한다. winit4478 backport는 유지한다.
- [ ] 현재 작업의 명시적 실행 허락 후 macOS 두벌식 타이핑 matrix를 실행한다. 코드 리뷰·커밋과 headless 검증은 완료했다.

```bash
cargo test -p deppy-sijo --bin deppy-sijo 한글 -- --nocapture
cargo test -p terminal ime -- --nocapture
```

Native matrix: `한글`+Space/Comma/Enter, 조합 중 좌우 이동·취소, 오른쪽 pane 끝, 좁은 split, Cmd+V, 탭 전환, hidden cursor TUI. 일반 shell의 무실행 입력 수집 fixture를 사용하며 기존 작업 중인 에이전트에 타이핑하지 않는다. 자동 test만 통과한 상태를 native IME 완료로 판정하지 않는다. winit surrounding-text/replacement-range 확장은 이 PR에 넣지 않는다.

## PR6 — snapshot 고정 셀 payload16B→12B

**파일**

- `crates/terminal/src/viewport_snapshot.rs:55`: attrs5bit+wide/spacer2bit 통합, typed helper, 주석 정정.
- `crates/terminal/src/alacritty_backend.rs`, `renderer_egui.rs`: accessor로 셀 의미 보존.
- `crates/runtime/src/protocol.rs`와 serde DTO: field/wire 순서 계약 유지 또는 명시적 version gate.
- snapshot 생성 fixture와 wire golden/roundtrip 검증.

**작업**

- [x] size/flag 조합/serde roundtrip 기대를 고정한다. 런타임 feature와 실제 target의 alignment를 확인한다.
- [x] `char + RGB + RGB + flags:u8`의 정상 정렬 표현으로 변경한다. `repr(packed)`와 비정렬 unsafe 접근은 쓰지 않는다. PR3의 희소 grapheme는 셀 밖에 둔다.
- [x] backend·renderer·원격 DTO에서 wide/spacer/SGR를 같은 helper로 읽고 쓴다.
- [x] 동일 snapshot fixture의 allocation bytes와 wire roundtrip을 다시 확인하고 리뷰·커밋한다.

```bash
cargo test -p terminal
cargo test -p runtime protocol
cargo test -p runtime old_peer
```

완료 기준: 기본 Cell size12B,300×80 cell payload384,000→288,000B. 임시 Vec→Arc traffic와 side table는 별도 집계한다. 앱 전체 RSS 감소율은 합성 payload 계산으로 추정하지 않는다.

## PR7 — 메타데이터와 변경 행만 갱신하는 snapshot

**파일**

- `crates/terminal/src/backend.rs`, `alacritty_backend.rs:401`: dirty row 기반 snapshot builder/cache 계약.
- `crates/terminal/src/viewport_snapshot.rs`: immutable 행 공유 표현과 접근 API.
- `crates/session/src/session.rs:377`: dirty 범위 소비와 snapshot 생성 순서.
- `crates/runtime/src/event.rs:298`: latest-wins로 건너뛴 세대의 dirty 합치기.
- `crates/runtime/src/protocol.rs`: local row storage와 flat wire DTO 경계.
- `crates/terminal/src/renderer_egui.rs`, `crates/app/src/ui/workspace.rs`: 공유 행 읽기, 숨김 pane snapshot/cache 반환 유지.

**작업**

- [x] cursor-only 변경, dirty1, UI가 여러 세대를 건너뜀, resize/scroll/palette/alt-screen full resync fixture를 만든다.
- [x] backend/session 한 worker가 snapshot builder를 소유하고, 바뀐 행만 새 Arc로 만든다. metadata-only 갱신은 행 Arc를 재사용한다. UI가 보유한 이전 snapshot을 in-place 변경하지 않는다.
- [x] 내부 immutable 행 공유와 원격 DTO를 분리한다. local 경로에서 매번 다시 flat Vec로 합치는 구현은 받아들이지 않는다. 원격 전송의 flatten 비용은 별도 기록한다.
- [x] grapheme 변경과 generation gap을 포함한 전체/델타 결과를 기존 full snapshot oracle와 비교한다.
- [x] 동일 headless fixture를 전후 3회 비교하고 source 리뷰·커밋한다.
- [ ] 현재 작업의 명시적 실행 허락 후 격리 앱 시나리오의 CPU/RSS·peak live heap을 비교한다.

```bash
cargo test -p session snapshot
cargo test -p runtime viewport
cargo test -p runtime protocol
cargo test -p terminal renderer_egui
```

완료 기준: metadata-only cell payload 할당/복사0, dirty1에서 변경1행만 생성. 같은 grid의 full repaint/원격 계약/숨김 전환은 정확해야 한다. 전체 저장소 재설계나 UI framework 교체를 섞지 않는다. 여러 reader가 이전 snapshot을 보유할 때 peak live heap이 늘지 않는지 앱 측정으로 확인한다.

## 4. 앱 실측의 추가 실행 절차

실행 허락이 온 경우에만 `measurements/.../prepared_gui_runner.py`를 사용한다. 기존 PID71086을 유지하고 순서대로18초씩 `idle1/idle8/dirty1/bulk/agenttui/fullscreen/switch8/createdelete`를 실행한다. 각 앱은 내장 deadline으로 정상 종료하며 실행 창이 사용자 화면을 잠깐 차지할 수 있다. 정상 allocator(mimalloc)를 사용하며 `bench-alloc` System allocator 측정과 섞지 않는다.

```bash
# 사용자 허락 전에는 이 명령을 실행하지 않는다.
python3 /tmp/deppy-live-measure-20260927/run_gui_bench.py
```

현재 준비된 계측의 한계:

- `bench::frame_begin/end`의 `ui_ms`는 App UI 구간이다. egui tessellation·wgpu 제출·GPU 실행·present를 모두 포함하는 end-to-end frame latency가 아니다.
- 현 frame JSONL은 종료 시 flush하며 frame별 발생 시간/steady phase가 없다. 첫5초를 제외한 외부 CPU/RSS와 startup 포함 UI p95를 같은 구간 지표로 표현하지 않는다.
- `cause`는 `repaint_causes()`의 첫 원인만 기록한다. 무변화 repaint를 고칠 때는 원인 전체와 frame 시각/세대를 추가 계측한 뒤 변경한다.
- sampler는250ms마다 OS/process 조회를 한다. 측정 overhead가 포함된다. 변동이 작은 CPU 차이는 반복하고 stats 없는 측정으로 교차 확인한다.
- 앱 측정용 workspaces는 split pane 수와 다르다. 다분할·scrollback scrolling·한글 dense 화면·native IME는 추가 승인된 GUI fixture가 필요하다.

## 5. 보류할 큰 작업

**전체 paint replay 제거/retained GPU texture**는 지금 PR로 확정하지 않는다. cached300×80 headless0.54ms와 debug main-thread stack만으로 renderer 전면 교체를 입증하지 못했다. 실제 최신 release 다분할 p95와 repaint 원인을 측정한 뒤, 충분한 병목이 남으면 별도 PR을 만든다.

**그래픽 메모리 누수 수정**도 확정하지 않는다. 장시간 실행 앱의 높은 graphics footprint는 관찰됐지만 같은 workload에서 create/delete 뒤 회복되는지 측정하지 않았다. atlas/texture descriptor는 실제 GPU heap과 같지 않다.

## 6. PR 공통 리뷰와 종료 기준

- [x] 먼저 회귀/불변식 테스트를 실행해 현재 실패 또는 baseline을 기록한다. 통과하지 않은 test를 PASS라고 쓰지 않는다.
- [x] 해당 PR의 focused tests와 동일 측정 fixture를 실행한다. source correctness와 성능 수치를 따로 판정한다.
- [x] Unicode/wide/copy/원격 gap/hidden cache/IME ownership 계약 중 영향을 받는 범위만 추가 검증한다.
- [x] 실제 source diff를 CLI 코드 리뷰하고 발견된 결함을 반영한다. unrelated worktree 변경은 넣지 않는다.
- [x] `cargo fmt --all -- --check`, `git diff --check`, release rebuild 결과를 기록한다.
- [x] PR 단위 커밋과 handoff·측정 결과를 남긴다. 사용자 요청 없이 push 또는 앱 실행·재실행하지 않는다.

초기 조사 단계에는 이 일곱 PR의 구현과 개선 후 측정을 실행하지 않았다. 현재 코드/허용된 측정의 완료 상태와 남은 native 검증은 맨 위 링크를 따른다. 계획 self-review에서 live/debug/release/headless의 구분, 정확한 파일 위치, wire/grapheme/IME 회귀, 실행 허락 제한을 확인했다.
