# CPU 사용량 정적 조사 (2026-08-14)

작성일: 2026-08-14
브랜치: `perf/cpu-investigation`
작업 worktree: `/Users/jr/Desktop/projects/deppy-sijo/.claude/worktrees/agent-aa3ef13db7da076d0`

> **[2026-08-14 정정] 이 문서의 여러 전제가 이후 라이브 실측으로 반증됐다.**
> 아래 본문은 정적 분석 시점의 기록으로 남기되, 먼저 마지막 장
> [「7. 라이브 실측 결과 — 이 문서의 정정」](#7-라이브-실측-결과--이-문서의-정정)을 읽어라.
> 특히 **5장의 `[profile]` 추천은 철회됐고 적용도 되돌렸다.**

## 0. 전제 — 이 문서는 실측이 아니라 정적 분석이다

이번 조사는 **컴파일·실행·프로파일러 재실행을 전부 금지**한 상태에서 코드만 읽어
판정한 것이다. 출발점은 이미 떠 있던 디버그 빌드(`./target/debug/deppy-sijo`,
pid 61409, 에이전트 세션 2개 실행 중)를 5초 `sample`한 결과로, 원본은
`/private/tmp/claude-501/-Users-jr-Desktop-projects-deppy-sijo/acd7035b-98a9-4634-88f6-0c67b75faf19/scratchpad/deppy-sample.txt`
에 있다. 메인 스레드 2278 샘플 중 1888(83%)이 `run_ui_and_paint` 내부였다.

**"디버그 빌드라서 느리다"는 가설이며, 이 문서로 확정하지 않는다.** 릴리스 빌드
실측 전에는 단정하지 않는다(6장 절차로 나중에 검증).

---

## 1. 왜 매 프레임 repaint인가

### 판정: 대체로 정상(설계된 output-driven wake) — 단, `output_batch_ms`는 실제
연속 출력 배칭에 적용되지 않는다(설정과 동작이 어긋난다)

**조사 방법**: `request_repaint`/`request_repaint_after` 전체 호출부(약 160곳,
대부분 `crates/app/src/app.rs`)를 grep으로 전수 나열하고, 렌더 핫패스
(`workspace.rs`, `file_tree.rs`, `composer.rs`, 터미널 렌더러)에 있는 호출부를
전부 읽었다.

**애니메이션/자기보정 후보 — 모두 자기종료 조건이 있다, 상시 참 아님**:

- `crates/app/src/ui/composer.rs:660-663` — 컴포저 높이 애니메이션.
  `egui_ctx.animate_value_with_time(...)`(`composer.rs:542-546`)로 목표 높이에
  수렴하며, `(text_h - target).abs() > 0.5`일 때만 repaint 요청. 확장/접힘
  전환 중에만 몇 프레임 지속되고 끝난다.
- `crates/app/src/ui/workspace.rs:4442` — pane 경계 밖 드래그 오토스크롤 중에만
  (`rate != 0.0`). 버튼을 떼면 예약이 끊긴다(idle 0 유지, 주석에 명시).
- `crates/app/src/ui/workspace.rs:4802` — pane 강조 플래시 페이드
  (`now < until`). 1~2초 타이머가 끝나면 멈춘다.
- `crates/app/src/ui/workspace.rs:5329` — `command_sent` 플래그가 설 때만 1회
  더 그린다(`flush_command_repaint`).
- `crates/app/src/ui/file_tree.rs:1747,1821,3112,3152` — 사이드바/네비게이션
  레일 리사이즈 드래그 중, OS 파일 드래그 중, 행높이 자기보정
  (`observed - row_height).abs() > 0.1`)만 조건. 자기보정은 실측값을
  `measured_row_height`에 저장하므로 원칙적으로 첫 교정 이후 조건이 거짓이
  된다(다만 폰트 래스터/서브픽셀 지터로 재발할 가능성을 코드만으로는 완전히
  배제하지 못한다 — 아래 5장 후보 참고).

이 중 어느 것도 "조건 없이 매 프레임 스스로 repaint를 예약"하지 않는다.

**PTY 출력 경로 — change-gated로 설계돼 있다**:

- `crates/runtime/src/in_process.rs:58-67` (`Subscriber.wake` 주석) — 상태
  이벤트 도착 시에만 UI를 깨운다. "push가 dirty 게이트라 출력이 있을 때만
  울리므로 idle 리페인트를 유발하지 않는다"고 명시.
- `crates/app/src/agent_detect_worker.rs:556-565`
  (`publish_outcome`/`PublishResult::Changed`) — ps/lsof/transcript 스캔
  결과가 실제로 바뀔 때만 `ctx.request_repaint()`. `BINDING_INTERVAL`(2.5s),
  `ACTIVITY_INTERVAL`(1.5s), `KINDS_INTERVAL`(1.2s) 자체는 폴링 주기일 뿐 매
  틱마다 repaint하지 않는다.

**`output_batch_ms`(기본 25ms) 배칭이 실제로 적용되는가 — 아니다, 부분적으로만**:

- `crates/app/src/config.rs:375-376`의 필드 주석이 직접 말한다: *"Runtime
  worker의 idle fallback poll 간격... 실제 PTY 출력은 reader wake로 즉시
  pump되고, 연속 viewport는 runtime에서 8ms로 frame pacing한다."*
- 실제 구현: `crates/runtime/src/in_process.rs:45`
  `const ACTIVE_VIEWPORT_FRAME_INTERVAL: Duration = Duration::from_millis(8);`
  그리고 `in_process.rs:1194-1198`:
  ```rust
  // config batch는 출력/명령이 전혀 없을 때의 fallback poll 간격이다. 출력 reader와
  // command sender가 이 thread를 unpark하므로 첫 반응은 timeout과 무관하게 즉시다.
  // 연속 출력은 8ms(또는 더 작은 테스트 batch) frame pacing으로 snapshot만 합친다.
  let viewport_interval = self.batch.min(ACTIVE_VIEWPORT_FRAME_INTERVAL);
  ```
  즉 `viewport_interval = min(output_batch_ms, 8ms)`다. 기본값 25ms > 8ms이므로
  **연속 출력 중에는 사용자가 설정에서 조정하는 `output_batch_ms`(설정 UI:
  `crates/app/src/ui/settings.rs:2171-2176`, i18n 키
  `settings.output_batch_ms`)가 사실상 무시되고 하드코딩된 8ms가 이긴다.**
  `output_batch_ms`는 "출력이 전혀 없을 때의 유휴 폴백 폴링 간격"으로만
  동작한다.
- 결과적으로 에이전트가 활발히 토큰을 스트리밍하는 동안 뷰포트 스냅샷은 최대
  초당 ~125회까지 만들어질 수 있고, 그때마다 subscriber wake →
  `ctx.request_repaint()`가 걸린다. 세션이 2개면 두 배로 겹칠 수 있다.
  **이건 버그가 아니라 "120Hz를 따라가면서도 청크마다 스냅샷을 만드는 폭주는
  막는다"는 의도된 설계**(주석, `in_process.rs:43-45`)지만, 조사 과제가 물은
  "output_batch_ms가 실제로 적용되는가"에는 "연속 출력 시엔 아니다"가 정확한
  답이다.

**종합 판정**: 5초 샘플에서 관측된 83% 점유는, 코드상 근거로 보면 "매 프레임
스스로 도는 버그"보다는 "실제 PTY 출력이 있을 때 8ms 페이싱으로 자주 깨어나는
설계가 두 개의 활성 에이전트 세션 아래에서 실제로 자주 트리거된 것"이 더
유력한 설명이다. 다만 `crates/app/src/app.rs`(약 32,000줄)의 `request_repaint`
호출부 전부를 한 줄씩 실행 조건까지 재현하지는 못했으므로, "다른 숨은
상시-참 조건이 전혀 없다"는 것까지는 **확언할 수 없다** — grep+문맥 읽기로
찾은 후보들은 전부 자기종료/변화-게이트였다는 것만 확인했다.

---

## 2. `App::poll_worktree_jobs`가 70 샘플인 이유

### 판정: 문제 있음 — 확정적 근거 있음

`crates/app/src/app.rs:14931` 시작부:

```rust
fn poll_worktree_jobs(&mut self) {
    let text = self.i18n.clone();          // app.rs:14932
    if let Some((requested_workspace, receiver)) = &self.worktree_rx { ... }
    ...
```

`self.i18n`은 `Catalog`(`crates/i18n/src/lib.rs:7-12`)이고, 구조는

```rust
pub struct Catalog {
    locale: String,
    primary: BTreeMap<String, String>,
    fallback: BTreeMap<String, String>,
}
```

`en-US/messages.txt`가 1,034줄이므로 `primary`/`fallback` 각각 대략
1,000개 안팎의 `String → String` 항목을 가진다(로케일이 fallback과 같으면
`primary`도 `fallback.clone()`으로 채워진다 — `Catalog::load`,
`crates/i18n/src/lib.rs:14-28`). **이 전체를 `poll_worktree_jobs`가 매 프레임
무조건 clone한다** — `self.worktree_rx`/`self.worktree_remove_rx`가 둘 다
`None`인 평상시(워크트리 생성/삭제 작업이 없을 때)에도 예외 없이 실행되고,
clone된 `text`는 오직 드물게만 실행되는 `Ok(Err(_))`/알림 분기에서만 쓰인다.

실측 샘플이 이를 그대로 보여준다(`deppy-sample.txt:5244` 부근 스택):

```
70 App::poll_worktree_jobs                     app.rs:14990
36   i18n::Catalog::clone                       lib.rs:11
36     BTreeMap::clone
18       BTreeMap::clone::clone_subtree
...                                              (String::clone → mimalloc 할당)
```

즉 **poll_worktree_jobs의 70 샘플 중 36개(약 51%)가 정확히 이 무조건
`i18n.clone()` 한 줄**이다. `try_recv()` 2회 자체는 값싸다 — 비용은 그 앞의
불필요한 카탈로그 딥카피다.

같은 패턴이 `crates/app/src/app.rs:22533`(`App::ui` 최상단, 매 프레임 1회)에도
있지만, 이쪽은 렌더 트리 전체에 `catalog: &i18n::Catalog`를 넘기는 데 실제로
쓰이므로 상대적으로 정당화된다(그래도 `Arc<Catalog>`로 바꾸면 이 clone도
포인터 복사로 바뀐다 — 5장/후보 참고). `poll_worktree_jobs`의 clone은 그 정도
정당성이 없다 — 두 채널이 비어 있으면 전혀 쓰이지 않는다.

---

## 3. `file_tree::project_file_panel`이 155 샘플인 이유

### 판정: 정상(캐시·가상화 있음), 다만 매 프레임 위젯 재구성 비용은 존재

`crates/app/src/ui/file_tree.rs:1760`(`project_file_panel`) →
`crates/app/src/ui/file_tree.rs:1881`(`contents`) →
`crates/app/src/ui/file_tree.rs:2772`
(`egui::ScrollArea::vertical().show_rows(ui, row_height, total, |ui, range| ...)`).

- **가상화는 실제로 적용된다**: `show_rows`가 보이는 `range`만 클로저를
  호출하므로(`file_tree.rs:2772-2774`), 스크롤 밖 행은 그리지 않는다.
- **행 목록은 파일시스템에서 매 프레임 재스캔하지 않는다**: `range` 안의
  각 행은 `&self.flat[index]`(`file_tree.rs:2774`)에서 읽는다. `self.flat`은
  파일 트리 워처/백그라운드 채널이 갱신할 때만 재계산되는 캐시된
  `Vec`이고(`fn refresh`, `file_tree.rs:1577` 및 정렬 `file_tree.rs:647`),
  매 프레임 정렬/재할당하지 않는다.
- **비용의 실체는 immediate-mode 위젯 빌드 자체**다: 보이는 행마다
  `ui.interact`(hover/drag/context_menu 등록), 드롭 대상 판정, 우클릭 메뉴
  클로저 등록, `catalog.t(...)` 호출(각각 새 `String` 할당)이 매 프레임
  일어난다. egui는 즉시모드라 이건 설계상 불가피한 비용이며, "잘못된
  캐시 무효화"의 증거는 찾지 못했다.

즉 155 샘플은 "낭비"라기보다 "보이는 행 수 × 위젯당 고정 비용"에 가깝다.
사이드바가 넓거나 파일이 많이 펼쳐져 있으면 보이는 행 수가 늘어 비례해서
커진다 — 최적화 여지는 있지만(예: `catalog.t()` 결과 캐싱), 캐시 자체가
없다거나 매 프레임 전체 트리를 다시 훑는 버그는 아니다.

---

## 4. 터미널 렌더러 캐시 유효성 (`build_row_cache` 170 + `layout_attr_text` 164)

### 판정: 판정 불가에 가까움 — 캐시 설계 자체는 올바르지만, "합당한 부하 vs
과다한 dirty" 여부는 실측 없이 코드만으로 완전히 가르기 어렵다

`crates/terminal/src/renderer_egui.rs`의 캐시 구조를 읽었다.

- `dirty_is_fresh` (`renderer_egui.rs:118-122`): 같은 `snapshot_gen`을
  두 번 그리면(=출력 없는 재도장, 리소스 표시 갱신 등) dirty를 신뢰하지
  않는다 — "2026-07-14 실측: idle에서 rows_rebuilt≈전체 행"이라는 과거
  수정 이력이 주석에 있다(`renderer_egui.rs:95-99`). 이 경로는 idle repaint의
  낭비를 이미 막아 놓은 상태다.
- `draw()`(`renderer_egui.rs:307-329`)의 행별 판정: `dirty = dirty_fresh &&
  row_is_dirty(snapshot, row)`, 그리고 캐시 슬롯이 `None`(리사이즈/폰트
  변경 등 shape 변경)일 때만 `build_row_cache`를 다시 부른다. 새 스냅샷
  세대에서도 **바뀐 행만** 다시 그린다 — "새 스냅샷 = 전체 재빌드"가 아니다.
- `row_is_dirty`/`range_intersects_row`(`renderer_egui.rs:582-597`)는 셀
  단위 `CellRange` 교집합 판정으로 행 단위보다 세밀하다.
- dirty range 생성부(`crates/session/src/session.rs:541-560`,
  `dirty_rows_to_ranges`/`full_dirty_ranges`)를 보면 `mark_dirty_rows`로
  실제 바뀐 행만 모으고, resize/scroll/replay 같은 "전체가 실제로 바뀌는"
  경우에만 `mark_full_dirty`로 전체를 더럽힌다(`session.rs:412-451`).
  "아무 변경 없는데 전체를 dirty로 찍는" 과다-마킹 코드는 찾지 못했다.

즉 코드 리뷰로는 캐시가 (a) fine-grained하고, (b) 이미 한 차례
idle-낭비 버그를 수정한 이력이 있다는 것까지 확인했다. 그렇다면 남는
설명은 (a) 실제로 활성 에이전트 2개의 TUI가 프레임마다 화면 대부분(스피너,
스트리밍 텍스트, 상태줄)을 실제로 갱신하고 있어 dirty 행 수 자체가 크다는
쪽이 유력하다. 다만 **이걸 코드만으로 정량 확정할 수는 없다** —
`cache.counters.rows_rebuilt`/`dirty_rows`(`renderer_egui.rs:114-116`,
`RenderCounters`)를 실측 로그로 뽑아야 (a)/(b)를 가른다. 6장 실측 절차에
이 카운터를 확인하는 항목을 넣었다.

---

## 5. `[profile]` 제안 — ~~적용 완료~~ **철회됨 (7장 참조)**

### 선택지와 트레이드오프

| 옵션 | 빌드 시간 영향 | 디버깅 | 증분 빌드 |
|---|---|---|---|
| `[profile.dev] opt-level = 1` (워크스페이스 전체) | 모든 크레이트가 매번 opt-level 1로 컴파일 — **앱 코드를 고칠 때마다** 그 크레이트 재최적화 비용 발생 | 최적화로 일부 변수 소거/인라인, 여전히 `debug-assertions`는 유지되면 대체로 무난 | 느려짐 — 반복 개발 루프(`dev-run.sh`)마다 영향 |
| `[profile.dev.package."*"] opt-level = 2` (의존성만) | **의존성이 안 바뀌는 한** 최초 1회만 비용 지불, 이후 캐시됨 | 앱 자체 코드는 opt-level 0 그대로라 브레이크포인트·스택트레이스 그대로 유효 | 앱 코드만 고치는 일상 반복에는 영향 없음 |
| `[profile.dev] debug = false` | 링크·디스크 절감, CPU와는 무관 | 디버그 정보 없어짐(비권장) | 무관 |
| 릴리스로만 개발 | 최대 성능 | `cargo build --release`도 dev보다 훨씬 느린 최초/증분 빌드, LTO 등으로 반복 개발에 부적합 | 매우 나쁨 |

### 추천: `[profile.dev.package."*"] opt-level = 2`

이유:

- 이 저장소의 UI/디자인 워크플로(`CLAUDE.md`)는 "화면 변경 → 즉시
  빌드+재기동"을 반복한다. 워크스페이스 크레이트(`app`, `terminal`,
  `session` 등)까지 최적화하면 **바로 그 반복 루프가 느려져** 워크플로와
  충돌한다.
- egui의 텍스트 셰이핑/레이아웃, `alacritty_terminal`의 파서·그리드 연산,
  `wgpu`의 렌더 커맨드 인코딩처럼 매 프레임 hot loop를 도는 코드는 전부
  워크스페이스 밖(의존성)에 있다 — 여기만 최적화해도 1~4장에서 확인한
  `build_row_cache`/`layout_attr_text`/`egui` 내부 비용을 실질적으로
  낮출 수 있다.
- 의존성은 `Cargo.lock`이 고정하는 한 자주 바뀌지 않으므로, 최초 1회
  (또는 `cargo clean`/lock 갱신 후) 느린 빌드를 감수하면 그 뒤로는 캐시된다.

### 적용

루트 `Cargo.toml`에 추가했다(코드 변경은 이 한 곳뿐):

```toml
[profile.dev.package."*"]
opt-level = 2
```

**컴파일은 한 번도 하지 않았으므로 이 설정이 실제로 빌드 시간/런타임 CPU에
미치는 영향은 6장 절차로 별도 실측해야 한다.**

---

## 6. 실측 절차

### `DEPPY_FRAME_STATS=1`가 정확히 하는 일 (코드 근거: `crates/app/src/perf.rs`)

- 환경변수가 없으면 `FrameStats::enabled = false`이고 `begin()`/`end()`는
  즉시 반환한다 — 평소 실행 비용 0(`perf.rs:1,19-27,30-34,39-42`).
- `begin()`은 `App::ui()` 진입 시각을 기록한다
  (`crates/app/src/app.rs:22528-22529`).
- `end()`은 `App::ui()` 종료 직전에 불린다(`crates/app/src/app.rs:25653`
  근처, 주석: "frame_stats.end() 뒤라 JSONL 기록 비용은 ui_ms에 섞이지
  않는다"). 이번 프레임 소요(ms)를 `frame_ms` 벡터에 push한다.
- **5초 윈도마다** `tracing::info!(frames, p95_ms, "frame stats (5s window)")`
  로그를 남긴다(`perf.rs:48-54`). `frames`는 그 5초 동안 실제로 `ui()`가
  호출된 횟수(=프레임 수), `p95_ms`는 그 윈도 프레임들의 **렌더 소요 시간**
  95분위(프레임 간 간격이 아니라 `ui()` 함수 실행 자체에 걸린 시간,
  `percentile95`/`percentile`, `perf.rs:58-71`).
- **idle이면 로그 자체가 안 찍힌다** — `ui()` 호출이 없으면 `begin`도
  `end`도 안 불린다. "5초 동안 이 로그가 안 보인다"가 곧 "그 구간에
  repaint가 0회였다"는 뜻이다. 이게 완료 기준 "idle repaint 0회"의 확인
  방법이다(`perf.rs:1-6` 상단 주석).
- `DEPPY_PERF_HARNESS=1`은 시작 시 셸 세션 11개(`HARNESS_SESSIONS`,
  `perf.rs:76`, 활성 1 + hidden 10, 그중 3개는 `harness_command`가
  1700바이트 라인을 100회/초 찍는 대량 출력 셸)를 자동 구성한다
  (`perf.rs:82-100`). hidden pane이 실제로 CPU를 아끼는지 볼 때 쓴다.

### 절차 (디버그/릴리스 × 에이전트 0/1/2개 매트릭스)

전제: 아래는 전부 **한 번에 한 사람이 실행**해야 한다 — 동시에 다른 CPU
측정이 진행 중이면(이번 조사가 회피한 것과 같은 이유로) 오염된다.

1. **빌드 두 벌 준비**
   - 디버그: `cargo build -p deppy-sijo` (이번에 추가한
     `[profile.dev.package."*"] opt-level = 2`가 자동 적용된다)
   - 릴리스: `cargo build -p deppy-sijo --release`
   - 필요하면 프로파일 적용 전/후를 비교하려고 `git stash`로 `Cargo.toml`의
     `[profile.dev.package."*"]` 블록을 껐다 켰다 하며 두 벌씩 더 빌드.

2. **각 빌드 × 각 에이전트 수(0/1/2개) 조합마다**:
   - 조건 준비: 워크스페이스를 열고 에이전트 세션을 정확히 0/1/2개
     실행(다른 세션은 종료해 조건을 순수하게 유지).
   - 실행: `DEPPY_FRAME_STATS=1 ./target/debug/deppy-sijo` (또는
     `./target/release/deppy-sijo`).
   - 앱을 30~60초 그대로 두고(에이전트가 실제로 응답을 스트리밍하는
     구간을 포함하도록), 로그에 찍히는 5초 윈도별
     `frame stats (5s window): frames=N p95_ms=M`을 최소 4~6개 윈도
     수집.
   - 병행: `sample <pid> 5 -f <out.txt>` 등 외부 프로파일러로 같은 구간을
     교차 검증(이번 조사에서 쓴 `deppy-sample.txt`와 같은 형식). 특히
     `run_ui_and_paint` 점유율, `build_row_cache`/`layout_attr_text` 샘플
     수를 비교해 4장에서 못 가른 (a)/(b)를 확정한다.
   - 기록: 각 조합의 `frames`(초당 프레임수로 환산), `p95_ms`,
     `sample`의 메인 스레드 점유율(%)을 표로 남긴다.

3. **idle 확인**: 에이전트 0개 + 사용자 입력 없이 60초 방치 상태에서
   `DEPPY_FRAME_STATS=1` 로그가 전혀 안 찍히면 "idle repaint 0회" 통과.
   찍힌다면 어떤 조건이 깨어나게 하는지 그 시각 전후 `sample`로 원인을
   좁힌다(1장에서 다 훑지 못한 `app.rs`의 나머지 `request_repaint`
   호출부를 여기서부터 역추적).

4. **비교축**: (디버그 vs 릴리스) × (에이전트 0/1/2) = 6칸 매트릭스를
   채운다. 이 조사의 가설 — "83%는 디버그 빌드 자체의 문제라기보다 활성
   출력에 따른 8ms 페이싱 때문" — 은 "에이전트 0개일 때 디버그도 idle에
   가깝고, 에이전트 수가 늘수록 디버그/릴리스 공통으로 frames/초가
   올라간다"는 패턴이 나오면 뒷받침된다. 반대로 "에이전트 0개인데도
   디버그가 릴리스보다 frames/초·p95_ms가 뚜렷이 높다"면 디버그 빌드
   자체(비최적화 hot loop)가 별도 요인이라는 뜻이다.

---

## 측정 후에 검토할 최적화 후보 (우선순위순, 코드는 이번에 건드리지 않았다)

1. **`App::poll_worktree_jobs`의 무조건 `i18n.clone()` 제거**
   (`crates/app/src/app.rs:14932`) — 실측으로 이미 51%가 이 한 줄임을
   확인했다(2장). 두 채널이 `None`인 공통 경로에서 clone을 건너뛰거나,
   알림 문자열이 실제로 필요한 분기 안으로 `self.i18n.t(...)` 호출을
   늦추면 된다. **가장 확실하고 손댈 곳이 좁은 후보.**
2. **`i18n::Catalog`를 `Arc<Catalog>`로 바꿔 clone을 포인터 복사로**
   — `App::ui()` 최상단의 매 프레임 clone(`app.rs:22533`)과 그 밖에
   catalog를 들고 다니는 다른 경로에도 광범위하게 이득. 다만 API 표면이
   넓어(수백 곳에서 `&i18n::Catalog`를 인자로 받음) 변경 범위가 크다 —
   측정으로 실제 비중을 먼저 확인하고 착수할 것.
3. **file_tree 행 위젯의 `catalog.t()` 반복 호출 캐싱** (3장) — 보이는
   행마다 매 프레임 새 `String`을 만든다. 사이드바가 넓고 파일 수가 많을
   때만 체감 차이가 있을 가능성이 높아 우선순위는 낮음.
4. **터미널 렌더러 dirty 카운터 상시 로깅/계측** — `RenderCounters`
   (`renderer_egui.rs`)가 이미 `rows_rebuilt`/`dirty_rows`를 들고 있다.
   4장의 (a)/(b) 판정을 실측으로 확정하기 전까지는 코드를 건드리지
   않는 게 맞다.
5. **file_tree 행높이 자기보정이 실제로 매 프레임 재발하는지 계측**
   (`crates/app/src/ui/file_tree.rs:3105-3113`) — 이론상 첫 교정 후
   수렴해야 하지만, 폰트 래스터 지터로 반복될 가능성을 코드만으로
   배제하지 못했다(1장). `observed_row_height` 값을 로그로 몇 프레임
   찍어보면 바로 확인 가능.

## 컴파일 확인

이 조사 전체에서 `cargo build`/`cargo check`/`cargo test`/`cargo clippy`,
앱 실행, `sample`/`instruments` 재실행을 **한 번도 하지 않았다**. 수행한
작업은 파일 읽기·grep·문서 작성·`Cargo.toml` 텍스트 편집뿐이다.

---

## 7. 라이브 실측 결과 — 이 문서의 정정

0~6장은 **컴파일·실행 금지 상태의 정적 분석**이었다. 이후 같은 날 실행 중인 앱을
직접 계측한 결과 아래 전제들이 틀렸음이 확인됐다. 기록으로 남긴다.

### 7-1. 측정 방법이 틀렸다 — `sample(1)`은 blocked 시간을 센다

0장의 출발점이었던 "메인 스레드 2278 샘플 중 1888(83%)이 `run_ui_and_paint`"는
**CPU 소비가 아니다.** `sample(1)`은 대기 중인 스레드도 샘플로 세는데, 그 구간을
파고들면 상당 부분이 `[CAMetalLayer nextDrawable]` — 즉 **vsync 대기**였다.
eframe 0.35에 wgpu 옵션을 넘기지 않아 기본 AutoVsync이고 화면은 60Hz다.

실제 CPU는 `ps -M`의 UTIME 누적 델타로만 판단해야 한다. 그렇게 재니:

| 상황 | deppy-sijo 자체 CPU |
|---|---|
| 유휴 | **3~4%** |
| 에이전트 출력 스트리밍 중 | **12~16%** |

### 7-2. "CPU 110%"는 앱이 아니라 자식 프로세스였다

Activity Monitor의 계층 보기가 보여주던 110~120%는 **프로세스 트리 합계**였다.
분해하면:

```
 13.2%  deppy-sijo                     ← 앱 자체
  2.1%  └ claude (앱이 띄운 에이전트)
 98.9%     └ cargo build --release → rustc
────────────────────────────────────
115.0%  트리 합계
```

즉 사용자가 앱 안에서 시킨 컴파일 작업이었다. 앱 최적화로 줄일 수 있는 대상이 아니다.

### 7-3. 메모리도 앱이 아니었다

phys_footprint 590MB 중 **Rust 힙(MALLOC_SMALL)은 16~18MB**다. 나머지는
`Owned physical footprint (unmapped) (graphics)` 302MB + IOAccelerator 190MB +
IOSurface 49MB — 6K 디스플레이(6016×3384) 스왑체인의 구조적 비용이다.
앱 유휴 시 반납되고 다시 그릴 때 재획득돼 ±290MB로 진동한다(누수 아님).
신규 인스턴스 16MB → 2.5시간 가동 18MB로 누수 없음을 확인했다.

### 7-4. 1장의 `output_batch_ms` 판정은 과장이었다

"설정과 동작이 어긋난다"고 썼지만, UI 힌트가 이미 정확히 설명하고 있다:
`settings.output_batch.hint = 출력 도착 시 즉시 깨어나며, 연속 출력은 8ms 간격으로
합쳐집니다 (유휴 폴백 상한)`. 버그가 아니라 문서화된 설계다.

### 7-5. 전환 버벅임의 진짜 원인 — shaping이 아니라 fsevents 동기 블로킹

4장이 의심한 텍스트 재shaping(`build_row_cache`/`layout_attr_text`)은 **전환 직후
샘플의 상위 28위 안에도 없었다.** 실제 원인은 전혀 다른 곳이었다:

```
71  AppFileTreeWatcher::replace
71  notify::fsevent::FsEventWatcher::watch
71  std::sync::mpsc::Receiver::recv      ← UI 스레드가 여기서 정지
```

`notify`의 macOS fsevent 백엔드는 `watch()`마다 내부 run-loop 스레드에 메시지를
보내고 **응답을 동기 대기**한다. 워크스페이스 전환은 루트가 바뀌어 감시 집합
전체를 교체하므로(최대 `FILE_TREE_WATCH_MAX_DIRECTORIES` = 256개) 그 대기가
쌓인다 — 4초 창의 2.9% ≈ **115ms UI 스레드 블로킹**.

수정(전용 스레드 이관) 후 전환 **5회 모두 메인 스레드 블로킹 0**을 확인했다.

### 7-6. 그래서 5장의 `[profile]` 추천을 철회한다

- **효과 없음**: 지연의 정체가 텍스트 shaping이 아니라 채널 대기였다. 의존성
  최적화는 이 경로에 아무 영향이 없다.
- **비용이 크다**: `Cargo.lock` 기준 의존성 635개를 worktree마다 재컴파일해야 한다.
  이 저장소는 worktree를 여러 개 두고 쓰며, 메인 worktree의 `target/debug/deps`만
  518,280개 파일 / 69GB다.
- **역효과**: 그 재빌드가 만드는 파일 churn이 실제로 이 머신을 짓누르던
  FSEvents/syspolicyd/logd 부하(합계 240% 이상)의 원인이었다. 최적화하려다
  진짜 병목을 키우는 셈이다.

루트 `Cargo.toml`의 `[profile.dev.package."*"]` 블록은 제거했다.

### 7-7. 유효했던 판정

2장(`poll_worktree_jobs`의 매 프레임 `i18n::Catalog` 딥카피)은 맞았고 수정됐다.
같은 부류가 `ui/agent_sessions.rs`의 `show()`에도 있어 함께 제거했다.
1장의 repaint 전수 감사(애니메이션 루프가 전부 자기종료 조건을 가진다는 확인)와
3장(`project_file_panel` 정상), 4장의 "판정 불가" 유보도 그대로 유효하다.

### 7-8. 교훈

정적 분석으로 좁힌 후보는 **실측 전에는 가설이다.** 이 문서의 1~4장은 코드 사실로는
정확했지만, "무엇이 사용자가 겪는 문제인가"를 고르는 데는 실패했다. 프로파일러
출력을 읽을 때는 **blocked 시간과 CPU 시간을 반드시 구분**하고, 프로세스 단위
측정에서는 **자식 트리가 합산되는지** 먼저 확인해야 한다.

