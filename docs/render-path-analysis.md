# egui Shape 발행 / Tessellation 렌더 경로 정적 분석 (B3)

작성일: 2026-07-13
기준 커밋: `17019ae` (분석 시점 main HEAD)
대상: `crates/terminal/src/renderer_egui.rs`, `crates/app/src/ui/workspace.rs`, egui/epaint/eframe 0.35 레지스트리 소스
성격: **읽기 전용 정적 분석 + 계측**. 저장소 코드는 변경하지 않았다.

---

## 0. 요약 (잠정 판정)

1. **질문 2 = 참.** 텍스트가 하나도 안 바뀐 프레임(dirty 0행)에서도 **모든 visible row의 텍스트/배경
   shape가 그대로 다시 발행되고, epaint가 전부 다시 테셀레이션한다.** 갤리 캐시는 *레이아웃(shaping)*만
   재사용하고 shape 발행·테셀레이션은 회피하지 못한다. 실측으로 f1(cold)과 f2(dirty 0)의 shape·vertex·
   index 수가 **완전히 동일**함을 확인했다.
2. **질문 3 = "매 프레임 테셀레이션" 확정.** egui 0.35는 테셀레이션 캐시가 **없다**. 소스에 명시적
   설계 결정으로 적혀 있다(`egui-0.35.0/src/context.rs:2764-2766`). 캐시된 갤리도 매 프레임 정점을
   복사한다(`epaint-0.35.0/src/tessellator.rs:2054-2100`).
3. **그런데 그 비용이 작다.** Apple M2 / release 기준 200×60 컬러풀 그리드에서 shape 발행 0.058ms +
   테셀레이션 0.270ms = **프레임당 0.33ms (16.6ms 예산의 2.0%)**. 80×24는 0.06ms. 즉 PR-05가
   *제거해 줄 수 있는 CPU 상한*이 0.33ms/frame이다. **정적 분석 기준 PR-05는 "성능"만으로는 정당화되지
   않는다 → 보류 권고.**
4. **진짜 병목은 다른 데 있었다.** `paint_selection_row`가 **선택된 셀마다 rect 1개**를 발행한다
   (`renderer_egui.rs:429-451`). 200×60 전체 선택 드래그 = shape 12,062개 / mesh 4.40MiB /
   **2.23ms/frame (예산의 13.4%)**. 이건 PR-05(wgpu 재작성)가 아니라 **bg_run처럼 run 병합하는
   ~15줄 수정**으로 18배 줄어든다. 계획서 §7 "드래그 선택 60fps" 목표의 실제 리스크는 여기다.
5. **[중대 정정] 이 앱은 이미 glow가 아니라 wgpu로 돈다.** `docs/terminal-current-state-audit.md:55`의
   "현재 glow/OpenGL 확정, wgpu는 lock에만 존재·미컴파일"은 **사실과 반대**다. eframe 0.35의 default
   feature에 `wgpu`가 들어 있고 `glow`는 없다(`eframe-0.35.0/Cargo.toml:60-68`). `cargo tree -e normal`
   에 glow/glutin은 **0건**, wgpu는 13건. `Renderer::default()`는 우리 feature 조합에서 `Wgpu`로
   확정된다(`eframe-0.35.0/src/epi.rs:592-615`). → **PR-04는 사실상 이미 달성**, PR-05의
   `egui_wgpu::CallbackTrait` 경로도 이미 링크되어 있다(egui-wgpu 0.35 / wgpu 29.0.4).

### 확정/반증에 필요한 실측 지표

정적 분석으로 **끝난 것**: 질문 1·2·3·4·5·7, 그리고 렌더러 백엔드 정체(=wgpu).
아직 **실측이 필요한 것**은 3개뿐이다.

| # | 지표 | 왜 필요한가 | 임계 |
|---|---|---|---|
| M1 | 실제 앱의 **repaint 원인 분포** (터미널 무변화 프레임 비율) | 본 분석의 "낭비" 크기는 *터미널이 안 바뀐 repaint가 초당 몇 번 오는가*에 정비례한다. 이벤트 드리븐이라 idle=0이지만, 다른 UI(호버/툴팁/스피너)가 유발하는 repaint는 미측정 | 무변화 repaint가 지속적으로 >30fps면 재검토 |
| M2 | 대량 출력 중 **실측 frame p95** (`DEPPY_FRAME_STATS=1 DEPPY_PERF_HARNESS=1`) | 본 분석은 렌더 경로만 쟀다. PTY/파서/스냅샷 비용은 별개 | p95 ≤ 16ms |
| M3 | **Retina 1x/2x 글자 품질** 육안 비교 (§8 절차) | PR-06의 유일한 진행 조건 | Ghostty 대비 열등하지 않을 것 |

---

## 1. 방법과 신뢰도

- **정적 분석**: HEAD(`17019ae`) 블롭 기준으로 코드 경로를 따라갔다. 인용한 라인 번호는 전부 HEAD blob
  기준(`git show HEAD:<path>`)으로 재검증했다. (작업 트리에는 다른 에이전트의 미커밋 수정이 있어
  트리 대신 HEAD를 기준으로 삼았다.)
- **계측**: 저장소 밖 scratch 크레이트에서 HEAD의 `crates/terminal`을 path 의존성으로 붙여
  **실제 `renderer_egui::draw`를 호출**하고, `egui::Context::run_ui` → `Context::tessellate`를 돌려
  shape/vertex/index를 직접 셌다. 시간은 release 빌드 300회 중앙값, allocation은 카운팅
  `GlobalAlloc`으로 프레임 1회 정확히 측정. 저장소 코드/테스트는 건드리지 않았다.
- **환경**: Apple M2 / 24GB / macOS 26.4.1 / release. `cargo test`·`clippy`는 공유 작업 트리가 다른
  에이전트의 미커밋 편집으로 일시적으로 컴파일되지 않는 상태여서 실행하지 않았다(대신 HEAD 스냅샷을
  실제로 빌드·실행해 검증했다 — 더 강한 증거다).
- 아래 **산술 모델과 실측치는 모든 셀에서 정확히 일치**한다(오차 0). 따라서 표의 수치는 추정이 아니라
  결정론적 계산값이다.

---

## 2. 질문 1 — 프레임당 shape 발행량

### 2.1 코드 경로 (누가 몇 개를 발행하는가)

| 발행 지점 | 파일:라인 | 개수 |
|---|---|---|
| pane 배경 rect | `renderer_egui.rs:148` `background_painter.rect_filled(rect, 0.0, default_bg)` | **1** (그리드 크기 무관) |
| 행 배경 run | `renderer_egui.rs:168-176` — 행 루프(`:151`) 안에서 `row_cache.bg_runs`마다 `painter.rect_filled` | 행당 `bg_runs.len()` |
| 선택 하이라이트 | `renderer_egui.rs:177` → `paint_selection_row` (`:410-452`) — **`for col in 0..cols`(`:429`) 안에서 셀 하나마다 `rect_filled`(`:446`)** | **선택된 셀 개수** (run 병합 없음) |
| 행 텍스트 run | `renderer_egui.rs:178-181` — `row_cache.text_runs`마다 `painter.galley` | 행당 `text_runs.len()` |
| 커서 | `renderer_egui.rs:201` `painter.rect_filled(cursor_rect, ...)` | `cursor.visible ? 1 : 0` |
| IME preedit | `renderer_egui.rs:207-225` — text ×2, rect ×1, line_segment ×1 | 조합 중일 때만 **4** |
| 검색 하이라이트 | `workspace.rs:241` `ui.painter().rect_filled(rect, 0.0, color)` (`render_terminal_search`, 호출 `workspace.rs:1423`) | **뷰포트에 보이는 매치 수** (매치당 rect 1개) |

`bg_runs` / `text_runs`는 `build_row_cache`(`renderer_egui.rs:272-329`)가 만든다.

- `bg_runs`: 기본 배경색(`#18181c`)인 셀은 **건너뛴다**(`:294-297`). 인접 동색 run은 `push_bg_run`
  (`:375-391`)이 병합한다. → 기본 배경 화면은 0개.
- `text_runs`: 공백(`' '`)과 `wide_spacer`는 run을 끊는다(`:305-308`). **fg 색이 바뀌거나 열이
  불연속이면 run을 끊는다**(`:320-323`, `PendingTextRun::needs_flush` `:340-342`).
  wide char(한글/emoji)는 **글자마다 독립 run 1개**(`:311-318`).

### 2.2 발행량 공식 (확정)

```
shapes = 1                       (pane 배경)
       + Σ_rows bg_runs          (색 배경 셀이 있는 만큼)
       + Σ_rows text_runs        (공백/색전환/wide로 쪼개진 만큼)
       + selected_cells          (선택 드래그 중에만, 셀당 1)
       + (cursor.visible ? 1 : 0)
       + (preedit ? 4 : 0)
       + visible_search_matches
```

- **행 수에 정비례**(행 루프가 전 행을 돈다 — `:151`, hidden row 컬링 없음).
- **열 수에는 "내용 의존적으로" 비례**: 단색 꽉 찬 행은 열 수와 무관하게 text_run 1개, 8칸마다 색이
  바뀌는 TUI 화면은 `cols/8`개.
- **wide char(CJK)는 글자당 1 shape** — 최악. 한글 80칸 꽉 찬 행 = 40 shape/행.

### 2.3 실측표 (HEAD 코드 실행, font 13.0, ppp 2.0)

`shell` = 앞 절반만 텍스트(6자 단어+공백), `dense` = 전 셀 텍스트 단색, `colorful` = 전 셀 텍스트 +
8칸마다 fg 교체 + 8칸마다 bg 교체 (TUI / `ls --color` 근사).

| 그리드 | 시나리오 | shapes | (text / rect) | 글리프 | vertices | indices | mesh 바이트 |
|---|---|---:|---|---:|---:|---:|---:|
| 80×24 | shell | 146 | 144 / 2 | 840 | 3,376 | 5,100 | 86 KiB |
| 80×24 | dense | 26 | 24 / 2 | 1,920 | 7,696 | 11,580 | 196 KiB |
| 80×24 | colorful | 362 | 240 / 122 | 1,920 | 8,656 | 15,180 | 228 KiB |
| 120×40 | shell | 362 | 360 / 2 | 2,080 | 8,336 | 12,540 | 212 KiB |
| 120×40 | dense | 42 | 40 / 2 | 4,800 | 19,216 | 28,860 | 488 KiB |
| 120×40 | colorful | 902 | 600 / 302 | 4,800 | 21,616 | 37,860 | 570 KiB |
| 200×60 | shell | 902 | 900 / 2 | 5,160 | 20,656 | 31,020 | 525 KiB |
| 200×60 | dense | 62 | 60 / 2 | 12,000 | 48,016 | 72,060 | 1.19 MiB |
| 200×60 | colorful | 2,252 | 1,500 / 752 | 12,000 | 54,016 | 94,560 | 1.39 MiB |

> `dense`의 shape 수가 `shell`보다 **적은** 게 직관에 반하지만 정확하다: 공백이 run을 끊기 때문에
> "듬성듬성한 셸 화면"이 "빽빽한 단색 화면"보다 shape가 많다. 반대로 *정점 수*는 글리프 수를 따라간다.

### 2.4 선택 드래그 (셀당 rect — 최악 케이스)

전체 화면 선택 시:

| 그리드 | shapes | vertices | indices | mesh | publish | tessellate | **total** |
|---|---:|---:|---:|---:|---:|---:|---:|
| 80×24 | 1,946 | 23,056 | 69,180 | 0.70 MiB | 0.048ms | 0.233ms | **0.281ms** |
| 120×40 | 4,842 | 57,616 | 172,860 | 1.76 MiB | 0.116ms | 0.584ms | **0.700ms** |
| 200×60 | 12,062 | 144,016 | 432,060 | 4.40 MiB | 0.284ms | 1.943ms | **2.227ms** |

선택 없는 200×60 dense는 0.122ms → **선택 하나로 18배**. rect 1개가 정점 8·인덱스 30을 먹기 때문에
(§4.2), 셀당 rect는 글리프보다 **셀당 정점 2배·인덱스 5배**를 쓴다. `push_bg_run`(`:375-391`)이
이미 하는 run 병합을 `paint_selection_row`에도 적용하면 선택 행당 rect가 1~2개로 떨어져 이 비용은
사실상 사라진다.

**[IMPROVE, 우선순위 상] 선택 하이라이트 run 병합 — PR-05와 무관하게 단독 수정 가능.**

---

## 3. 질문 2 — selection/cursor만 바뀐 프레임에서 텍스트 shape가 다시 발행되는가?

## **답: 예. 전부 다시 발행된다. (확정 — 사전판정 조건 "참")**

### 코드 근거

`renderer_egui.rs:151-183`의 행 루프는 두 단계로 나뉜다.

```rust
for row in 0..snapshot.rows as usize {
    let needs_rebuild = row_is_dirty(...) || 캐시 미스;
    if needs_rebuild { ... build_row_cache(...) }          // ← dirty 행만 (:158-165)

    if let Some(row_cache) = cache.rows_cache.get(row)... {
        for bg in &row_cache.bg_runs { painter.rect_filled(...) }   // ← 매 프레임 전 행 (:168-176)
        paint_selection_row(...)                                     // ← 매 프레임 전 행 (:177)
        for run in &row_cache.text_runs {
            painter.galley(pos, Arc::clone(&run.galley), run.color); // ← 매 프레임 전 행 (:178-181)
        }
    }
}
```

`needs_rebuild` 게이트가 감싸는 것은 **`build_row_cache`(=갤리 shaping)뿐**이다. shape 발행 루프
(`:166-182`)는 게이트 **밖**에 있고, 캐시 히트 여부와 무관하게 항상 실행된다.
`painter.galley`는 `Shape::Text(TextShape)`를 PaintList에 **push**한다
(`egui-0.35.0/src/painter.rs:529-533` → `:213` `add` → `layers.rs:127-131` `PaintList::add`).
egui는 immediate mode라 `PaintList`는 매 프레임 drain되어 비워진다(`layers.rs:213-244`).

### 실측 근거

동일 스냅샷으로 연속 2프레임(f1=cold, f2=dirty 0행) + 3프레임(f3=selection만 추가):

| 그리드/시나리오 | 프레임 | rebuilt_rows | shapes | vertices | indices |
|---|---|---:|---:|---:|---:|
| 200×60 colorful | f1 (cold) | 60 | 2,252 | 54,016 | 94,560 |
| 200×60 colorful | **f2 (dirty 0)** | **0** | **2,252** | **54,016** | **94,560** |
| 200×60 colorful | f3 (selection 추가) | 0 | 2,657 | 57,256 | 106,710 |
| 120×40 dense | cold | 40 | 42 | 19,216 | — |
| 120×40 dense | **1행만 dirty** | **1** | **42** | **19,216** | — |

`rebuilt_rows_last_frame`은 0으로 떨어지는데(=갤리 재shaping 없음) **shape/vertex/index는 한 개도
안 줄어든다.** 1행만 dirty인 프레임도 마찬가지다 — 재shaping은 1행이지만 발행·테셀레이션은 40행 전부.

### 결론

- 갤리(레이아웃/shaping) 재사용: **달성** — `TerminalRenderCache`가 작동한다(감사 문서 판정 유효).
- shape 발행 회피: **미달성** — 커서 한 칸 이동, 선택 드래그 1픽셀, 심지어 **다른 UI 위젯이 유발한
  repaint**에도 전 visible row의 텍스트 shape가 재발행되고 재테셀레이션된다.
- 계획서 PR-05 사전판정 조건 **"selection/cursor만 변경해도 전체 visible terminal shape가 반복
  발행됨" = 참**.
- 다만 이 "참"이 곧 "PR-05 진행"을 뜻하지는 않는다 — §4의 비용이 작기 때문이다(§7 참조).

---

## 4. 질문 3 — Tessellation 비용, 그리고 egui는 캐시하는가?

## **답: egui 0.35는 테셀레이션을 캐시하지 않는다. 매 프레임 전부 다시 한다. (확정)**

### 4.1 egui 0.35 소스 근거

`~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/egui-0.35.0/src/context.rs:2757-2795`:

```rust
pub fn tessellate(&self, shapes: Vec<ClippedShape>, pixels_per_point: f32) -> Vec<ClippedPrimitive> {
    profiling::function_scope!();

    // A tempting optimization is to reuse the tessellation from last frame if the
    // shapes are the same, but just comparing the shapes takes about 50% of the time
    // it takes to tessellate them, so it is not a worth optimization.
    ...
        tessellator::Tessellator::new(...)          // ← 매 프레임 새 Tessellator
            .tessellate_shapes(shapes)              // ← 전량 재테셀레이션
```

**의도적 설계 결정**이며 우회 수단이 없다. 캐시가 없으므로 "shape가 같으니 건너뛰기"는 egui 레벨에서
불가능하다.

`epaint-0.35.0/src/tessellator.rs:1991-2110` `tessellate_text` — **캐시된 갤리라도 정점을 매 프레임
복사한다**:

```rust
out.vertices.reserve(galley.num_vertices);          // :2017
out.indices.reserve(galley.num_indices);            // :2018
for row in &galley.rows {                            // :2035
    out.indices.extend(row.visuals.mesh.indices.iter().map(|i| i + index_offset)); // :2056-2062
    out.vertices.extend(row.visuals.mesh.vertices.iter().enumerate().map(|(i, v)| {
        ... Vertex { pos: final_row_pos + offset, uv: uv * uv_normalizer, color }   // :2064-2100
    }));
}
```

즉 갤리는 **정점 템플릿**을 캐시할 뿐이고, 프레임마다 (위치 오프셋 + 색 치환 + uv 정규화)를 걸어
새 Mesh로 **전량 복사**한다. `Arc<Galley>` 재사용이 테셀레이션을 아껴주지 않는다.

`tessellate_shapes`(`:2218-2260`)는 단일 스레드다 — rayon 병렬 경로(`:2221-2224`)는
`#[cfg(feature = "rayon")]`인데 **우리 빌드에 rayon은 없다**(`Cargo.lock`에 `rayon` 0건;
epaint의 `rayon`은 opt-in feature, `epaint-0.35.0/Cargo.toml:62`). → **테셀레이션은 UI 스레드
100%.**

### 4.2 정점/인덱스 산술 (확정 — 실측과 오차 0)

| 프리미티브 | 경로 | vertices | indices |
|---|---|---:|---:|
| 글리프 1개 | `text_layout.rs:1155-1205` `tessellate_glyphs` → `mesh.rs:199-227` `add_rect_with_uv` | **4** | **6** |
| 글리프 (italic) | `text_layout.rs:1174-1201` | 4 | 6 |
| 채운 rect (cr=0, stroke 없음) | `tessellator.rs:1755+` `tessellate_rect` → `path::rounded_rectangle`(4점, `:549-554`) → `fill_closed_path`(feathering>0, `:765-818`) | **8** (=2n) | **30** (=3n−2 삼각형) |
| 공백/글리프 없는 문자 | `uv_rect.is_nothing()` → 스킵 | 0 | 0 |

- `Vertex` = `pos: Pos2(8B) + uv: Pos2(8B) + color: Color32(4B)` = **20 바이트**
  (`epaint-0.35.0/src/mesh.rs:12-24`, `#[repr(C)]`).
- index = `u32` = **4 바이트** (`mesh.rs:66`).
- feathering은 기본 ON(1.0 물리픽셀, `tessellator.rs:667-673, 728-729`), Retina에서
  `feathering = 1.0 / pixels_per_point` 포인트(`:1333-1336`) — **정점 수는 DPI와 무관**
  (실측: 1x와 2x의 vertex/index 수가 완전히 동일).

**공식**

```
vertices = 4 × 글리프수 + 8 × rect수
indices  = 6 × 글리프수 + 30 × rect수
bytes    = 20 × vertices + 4 × indices
```

### 4.3 프레임당 비용 (Apple M2, release, ppp=2.0, 300회 중앙값)

| 그리드 | 시나리오 | shape 발행 | **tessellation** | 합계 | 16.6ms 예산 대비 | mesh/frame |
|---|---|---:|---:|---:|---:|---:|
| 80×24 | dense | 0.003ms | 0.021ms | 0.025ms | 0.15% | 196 KiB |
| 80×24 | colorful | 0.011ms | 0.049ms | 0.060ms | 0.36% | 228 KiB |
| 120×40 | dense | 0.004ms | 0.051ms | 0.055ms | 0.33% | 488 KiB |
| 120×40 | colorful | 0.024ms | 0.110ms | 0.133ms | 0.80% | 570 KiB |
| 200×60 | dense | 0.005ms | 0.118ms | 0.122ms | 0.73% | 1.19 MiB |
| 200×60 | colorful | 0.058ms | 0.270ms | **0.327ms** | **1.97%** | 1.39 MiB |
| 200×60 | dense + **전체 선택** | 0.284ms | 1.943ms | **2.227ms** | **13.4%** | 4.40 MiB |

60fps 환산 정점 대역폭: 200×60 colorful = 1.39 MiB × 60 ≈ **83 MB/s**의 CPU측 정점 쓰기.

> **해석**: "매 프레임 재테셀레이션"은 사실이지만, 그 절대 비용은 (전체 선택을 빼면) 프레임 예산의
> **2% 미만**이다. PR-05가 이걸 0으로 만들어도 회수액이 0.33ms/frame이다.
> 반면 **선택 하이라이트 run 병합**은 같은 그리드에서 2.1ms를 회수한다 — 6배 큰 이득을, 1/100 비용으로.

---

## 5. 질문 4 — egui 0.35의 자체 최적화, 우리가 이미 받는 이득

| 메커니즘 | 존재? | 근거 | 우리가 이득을 보는가 |
|---|---|---|---|
| **테셀레이션 캐시** | **없음** | `context.rs:2764-2766` (의도적 미채택) | — |
| **갤리(레이아웃) 캐시** | 있음 | `epaint/text/fonts.rs:1068-1160` `GalleyCache`(job 해시 키), `:1290-1298` `flush_cache`(이번 프레임에 안 쓰인 것만 축출) | **예 (이중으로)**. 우리 `TerminalRenderCache`가 없어도 egui가 갤리를 캐시했을 것이다. 우리 행 캐시의 실제 이득은 *shaping 회피*가 아니라 **매 프레임 `String`+`LayoutJob` 생성과 해시 계산 회피**다(§6 참조) |
| **Shape → Primitive 병합** | 있음 | `tessellator.rs:1383-1401` — 직전 primitive와 `clip_rect`가 같고 `texture_id`가 같으면 **같은 Mesh에 이어붙인다**. 우리 rect(brush 없음)와 text는 모두 `TextureId::default()`(폰트 아틀라스) (`shapes/shape.rs:413-421`) | **예.** 실측 결과 터미널 pane 하나가 만드는 primitive는 **항상 2개**(배경 painter의 clip_rect 1개 + content clip_rect 1개). shape 2,252개여도 **draw call은 2개**. → "draw call 폭증"은 우리 문제가 **아니다** |
| **coarse culling** | 있음 | `tessellator.rs:1756-1760`(rect), `:2048-2052`(text row) — clip_rect 밖이면 스킵 | 예 (clip 밖 행은 정점 생성 전에 컷) |
| **Shape::Callback** | 있음 | `shapes/shape.rs:70` `Callback(PaintCallback)`, `tessellator.rs:1375-1381`(테셀레이터를 그냥 통과) | **아직 안 씀** — PR-05의 진입점 |
| **rayon 병렬 테셀레이션** | 코드엔 있음, **우리 빌드엔 없음** | `tessellator.rs:2221-2224` `#[cfg(feature="rayon")]`; `Cargo.lock`에 rayon 0건 | 아니오. **켜는 것만으로 테셀레이션 일부를 워커로 뺄 수 있다** (단, `should_parallelize`가 "큰 shape"만 고르므로 shape이 잘게 쪼개진 우리 케이스에는 효과가 제한적일 것 — *추측, 미검증*) |
| **폰트 아틀라스 / Fonts** | 공유 | egui `Context` 1개 = Fonts/Atlas 1개 (감사 문서 §렌더링 판정과 일치) | 예 |
| **`TessellationOptions`** | 튜너블 | `tessellator.rs:656-745` | `feathering=false`로 두면 rect가 8v/30i → **4v/6i**로 줄어든다(`:809-816`). 텍스트에는 영향 없음(`:664`). *배경/선택 rect가 픽셀 정렬된 격자라 feathering이 시각적으로 기여하는 바가 거의 없을 가능성 — 미검증 추측, 육안 확인 필요* |

---

## 6. 질문 5 — Steady-state allocation (§3-B 대비)

### 실측 (dirty 0행 프레임 1회, 카운팅 GlobalAlloc)

| 그리드/시나리오 | 총 alloc/frame | shape 발행 구간 | tessellation 구간 | 총 alloc 바이트/frame |
|---|---:|---:|---:|---:|
| (기준선) 빈 UI | 16 | 16 | 0 | — |
| 80×24 dense | 34 | **16** | 18 | 523 KiB |
| 120×40 dense | 36 | **16** | 20 | 1,560 KiB |
| 200×60 dense | 36 | **16** | 20 | 2,594 KiB |
| 200×60 colorful | 49 | **16** | 33 | 3,897 KiB |

### 해석 — 누가 할당하는가

1. **`renderer_egui::draw` 자체의 steady-state heap allocation = 0.** shape 발행 구간의 alloc 수(16)가
   **빈 UI 기준선(16)과 정확히 같다.** dirty 0행이면
   - `cache.prepare`(`:64-82`)는 shape 불변 시 아무것도 안 한다,
   - `painter.galley(pos, Arc::clone(&galley), color)`는 `Shape`(64바이트, `shapes/shape.rs:73-80`)를
     **PaintList Vec에 push**할 뿐이고 그 Vec은 프레임 간 capacity가 유지된다
     (`layers.rs:213-244`의 `drain`이 `Vec::append`로 옮기므로 원본 capacity가 남는다),
   - `FontId::monospace`는 `FontFamily::Monospace`(유닛 배리언트) → String alloc 없음.

   → **§3-B의 "매 프레임 Vec/String 새 생성 금지"를 우리 렌더러는 이미 지키고 있다.**

2. **§3-B 위반은 전부 egui/epaint 내부에서 발생한다.**
   - `GraphicLayers::drain`이 **매 프레임 새 `Vec<ClippedShape>`를 만든다**(`layers.rs:220`
     `let mut all_shapes: Vec<_> = Default::default();`). `ClippedShape` = 80바이트
     (Rect 16 + Shape 64) → 200×60 colorful에서 2,252 × 80 ≈ **176 KiB/frame**. (실측 발행 구간
     바이트 184 KiB와 일치.)
   - `Context::tessellate`가 매 프레임 `Mesh::default()`를 만들고 Vec을 성장시킨다
     (`tessellator.rs:1396-1401`). 200×60 colorful에서 **33 alloc / 3.7 MiB/frame**의 할당 트래픽.
     (최종 mesh는 1.39 MiB지만 Vec 성장 재할당 누적이 ~2.7배.)
   - `Vec<ClippedPrimitive>` 자체도 매 프레임 새로 만들어진다(`tessellator.rs:2226`).

3. **§3-B "steady-state zero allocation" 기준 대비 미달 지점 = 우리 코드가 아니라 epaint 경로 그 자체.**
   epaint를 쓰는 한 이 항목은 **구조적으로 달성 불가**다. 0으로 만들려면 터미널을 epaint 밖
   (=paint callback)으로 빼는 수밖에 없다. → **PR-05의 유일한 "구조적" 논거는 성능이 아니라 이것이다.**
   다만 이 할당들은 alloc/free 짝이 맞는 **churn**이지 누수가 아니고, 프레임당 20~33회 수준이라
   실질 위험은 낮다(**의견**).

### 부수 관찰 (dirty 행이 있을 때)

`build_row_cache`(`:272-329`)는 dirty 행마다 `Vec<RowBgRun>`, `Vec<RowTextRun>`, run마다 `String`
(`PendingTextRun::text` → `std::mem::take`, `:366`)과 `FontId::clone`을 새로 만든다. 유계이고
(행당 최대 cols개 run) §3-B의 허용 범위("비정상적으로 긴 행이나 대규모 resize" 외에도 실질적으로
dirty 행 재구성은 이벤트성)에 가깝지만, 엄밀히는 **대량 출력 시 매 프레임 수십 행 × run당 String
alloc**이 발생한다. → **[IMPROVE, 낮음] `RowRenderCache`의 Vec을 재사용(`clear()` 후 재채움)하고
`PendingTextRun::text`도 buffer를 재사용하면 이 alloc도 0으로 만들 수 있다.**

---

## 7. 질문 6 — PR-05(custom wgpu paint callback)의 비용과 이득

### 7.1 전제 정정: 진입 장벽이 계획서 가정보다 훨씬 낮다

계획서/감사 문서는 "PR-04에서 wgpu로 전환한 뒤 PR-05"를 가정했다. 그러나 **이미 wgpu다**:

- `eframe-0.35.0/Cargo.toml:60-68` — `default = ["accesskit","default_fonts","wayland","web_screen_reader","wgpu","winit/default","x11"]` (**glow 없음**)
- 실제 활성 feature: `cargo tree -p eframe --depth 0 -f "{p} | feats: {f}"` →
  `accesskit,default,default_fonts,wayland,web_screen_reader,wgpu,wgpu_no_default_features,x11`
- `cargo tree -e normal` — glow/glutin **0건**, wgpu **13건**. `wgpu v29.0.4`, `egui-wgpu v0.35.0`.
- `eframe-0.35.0/src/epi.rs:582-615` — `Renderer::Glow`는 `#[cfg(feature="glow")]`라 **우리 빌드엔
  타입 자체가 없고**, `Renderer::default()`는 `#[cfg(not(glow))] #[cfg(wgpu_no_default_features)] → Self::Wgpu`.
- HEAD의 `Cargo.toml:33`은 `eframe = "0.35"` — **feature 지정 없음** → default features 그대로.
- `crates/app/src/main.rs:59-85`의 `NativeOptions { ..Default::default() }` → `renderer: Renderer::Wgpu`.

→ **macOS Metal backend로 이미 돌고 있다.** (`docs/terminal-current-state-audit.md:55`의 반대 서술은
`Cargo.lock`에 glow가 *보이는* 것을 근거로 삼은 오독이다. Cargo.lock은 feature 독립이라 비활성
optional dep도 기재된다.)

> **교차 확인**: 본 분석 진행 중, 병렬 트랙(B1)이 독립적으로 같은 결론에 도달해 root `Cargo.toml`에
> `eframe = { version = "0.35", features = ["glow"] }` + `DEPPY_RENDERER=glow` 런타임 선택을 추가하는
> 것을 확인했다(분석 시점 미커밋). **그 변경 후에도 기본 실행 경로는 여전히 wgpu다** —
> `eframe-0.35.0/src/epi.rs:609-613`이 glow·wgpu가 **둘 다** 켜지면 `Renderer::default()`를
> `Wgpu`로 반환하기 때문이다. 즉 glow는 A/B 실측용 opt-in으로만 존재하게 된다.

### 7.2 PR-05로 가면 우리가 직접 관리해야 하는 것

| 항목 | 내용 | 난이도 |
|---|---|---|
| Callback 진입 | `egui_wgpu::CallbackTrait` (`egui-wgpu-0.35.0/src/renderer.rs:87-120`): `prepare` / `finish_prepare` / `paint`. egui는 터미널 사각형만 할당하고 `Shape::Callback` 1개를 발행 | 하 |
| 파이프라인 | 셀 인스턴싱용 `RenderPipeline` + `BindGroupLayout` + WGSL 셰이더. `CallbackResources`(type-map)에 앱 전역 1개 저장 | 중 |
| 정점/인스턴스 버퍼 | 셀당 인스턴스(예: `{cell_xy: u16×2, glyph_uv: u16×4, fg: u32, bg: u32}` ≈ 20~24B). **워크스페이스별 대형 버퍼 금지**(§3-A 원칙 15) → shared buffer pool의 slice + `queue.write_buffer` 부분 갱신 | 중상 |
| **글리프 아틀라스** | **여기가 진짜 비용.** egui 폰트 아틀라스(`epaint::TextureAtlas`)를 재사용할지, 별도 아틀라스를 만들지 결정해야 한다. 재사용하려면 문자→uv_rect 매핑을 `Fonts`에서 꺼내야 하는데 egui의 공개 API는 galley 경유가 자연스럽다(→ 결국 galley를 만들게 됨). 독립 아틀라스면 **폰트 폴백·CJK·emoji·combining·2:1 폭 정합을 전부 다시 구현**해야 한다(현재는 egui가 공짜로 해준다 — §8 참조) | **상** |
| 색/테마/커서/선택/IME | 레이어별 재구현. IME preedit 오버레이(`renderer_egui.rs:204-234`)와 `output.ime` 연동 유지 필요 | 중 |
| Retina | `ScreenDescriptor { size_in_pixels, pixels_per_point }`로 직접 스케일 처리 | 중 |
| 회귀 위험 | 현재 통과 중인 wide/CJK/emoji/선택 추출 테스트(`renderer_egui.rs:696-766`)가 커버하는 건 **스냅샷→텍스트**라 렌더 회귀를 못 잡는다. 시각 회귀 테스트가 새로 필요 | 중 |

### 7.3 이론적 이득 (본 분석의 산술 기준)

| 항목 | 현재 (200×60 colorful) | PR-05 이후 (이론값) | 회수액 |
|---|---|---|---|
| shape 발행 | 0.058ms / 2,252 shape | Shape::Callback **1개** | ~0.058ms |
| tessellation | 0.270ms / 54k vtx / 95k idx | **0** (테셀레이터 통과) | ~0.270ms |
| 프레임당 mesh 트래픽 | 1.39 MiB 생성 + 3.9 MiB 할당 트래픽 | dirty 행만 `write_buffer` (수 KiB) | ~3.9 MiB/frame alloc churn |
| **CPU 합계** | **0.327 ms/frame (예산 2.0%)** | ~0.02 ms/frame (추정) | **≈0.31 ms/frame** |
| 선택 드래그(전체) | 2.227 ms/frame | 커서/선택은 uniform/작은 버퍼 → ~0.02ms | ≈2.2 ms/frame |

**그러나 선택 드래그 2.2ms는 §2.4의 run 병합(약 15줄)으로도 ~0.1ms 수준까지 떨어진다.** 그 수정을 하고
나면 PR-05의 순 회수액은 **≈0.31 ms/frame (예산의 1.9%)** 뿐이다.

### 7.4 판정 (정적 분석 기준)

> **PR-05 = 보류 권고 (epaint KEEP).** 사전판정 4조건 중 ①전체 재구성 없음 ②hidden 미렌더는 충족,
> ③"selection/cursor만 바꿔도 전체 재발행" = **위반(참)** 이지만, 그 위반의 **절대 비용이 프레임 예산의
> 2% 미만**이라 REPLACE 기준("구조적으로 목표 달성이 어렵다")에 도달하지 못한다.
> 유일하게 남는 구조적 논거는 §3-B **"steady-state zero allocation"**이며, 이건 epaint를 쓰는 한
> 달성 불가다. → **§3-B 기준을 "우리 렌더 경로의 allocation 0"(이미 달성)으로 현실화하거나,
> PR-05를 "zero-alloc 목표 달성"만을 위해 감수할 가치가 있는지 사용자 결정이 필요하다.**
> 대신 얻는 것 대비 잃는 것(폰트 폴백/CJK/emoji/IME를 직접 떠안음, §8)이 크다 — **비추천.**

---

## 8. 질문 7 — PR-06 판정 입력 (텍스트 속성 / CJK / 폴백)

### 8.1 텍스트 속성 지원 격차 — **완전 부재 확정**

`crates/terminal/src/viewport_snapshot.rs:22-30`:

```rust
pub struct TerminalCell {
    pub c: char,
    pub fg: [u8; 3],
    pub bg: [u8; 3],
    pub wide: bool,
    pub wide_spacer: bool,
}
```

**bold / italic / underline / strikeout / dim / blink 필드가 아예 없다.**

`crates/terminal/src/alacritty_backend.rs:271-291` — alacritty가 준 `Flags`에서 **오직**
`INVERSE`(fg/bg 스왑), `HIDDEN`(공백 치환), `WIDE_CHAR`, `WIDE_CHAR_SPACER`만 읽고
**`BOLD` / `ITALIC` / `UNDERLINE` / `STRIKEOUT` / `DIM`은 읽지도 않고 버린다.**

렌더러도 대응 코드가 없다 — `build_row_cache`는 fg/bg만 보고 run을 만든다(`renderer_egui.rs:290-325`).

추가 제약: 폰트 등록이 `Monospace` 패밀리에 **단일 face(`term_mono`)** 를 넣는 구조라
(`crates/app/src/fonts.rs:97-105`), 설정에서 고른 굵기 1개만 존재한다. 셀별 bold를 그리려면
**두 번째 패밀리(예: `FontFamily::Name("term_mono_bold")`) 등록 + run별 FontId 분기**가 필요하다.
underline/strikeout은 epaint가 `TextShape.underline`(Stroke)을 지원하므로
(`tessellator.rs:2102-2108`) 비교적 싸게 붙는다.

> **판정: 이건 PR-06(glyph 렌더러 품질)이 아니라 별도의 "기능 격차"다.** glyphon으로 갈아타도
> 백엔드가 flag를 버리는 한 bold는 안 나온다. **스냅샷 스키마(`TerminalCell`) + backend + renderer
> 3곳을 건드리는 독립 PR**로 다루는 게 맞다.
> 참고: `TerminalCell`은 `serde`로 web-remote/mobile 전송에 쓰이므로(`viewport_snapshot.rs:5`)
> 필드 추가 시 **wire 호환성**(append-only)을 지켜야 한다.

### 8.2 CJK / emoji / wide 처리와 폰트 폴백 비용

| 항목 | 현재 | 근거 | 비용 |
|---|---|---|---|
| wide char 폭 | `wide`면 2셀 폭 배경, `wide_spacer`는 렌더 스킵 | `renderer_egui.rs:294-299`(bg), `:305-318`(text) | — |
| wide char 텍스트 | **글자마다 독립 run 1개**(run 병합 없음) | `renderer_egui.rs:311-318` | **한글 화면은 shape 수가 최악** — 80칸 한글 꽉 찬 행 = 40 shape. 200×60 전체 한글이면 6,000 text shape. *(추정: colorful 케이스와 비슷한 0.3~0.5ms/frame 대역. 실측 미실시 — 기본 egui 폰트에 한글 글리프가 없어 scratch 계측기로는 정확히 못 잼)* |
| 한글 폰트 | D2Coding 번들(한글 자체 커버, 2:1 폭 정합) | `crates/app/src/fonts.rs:15-21, 94-105` | 폴백 없이 1차 face에서 해결 → 빠름 |
| CJK 폴백 | AppleGothic 등 시스템 폰트를 Monospace/Proportional 패밀리 **끝에** 추가 | `fonts.rs:56-69, 107-124` | D2Coding 미보유 한자/일본어는 폴백 face로 — egui가 처리 |
| emoji | egui 기본 emoji 폰트(NotoEmoji/emoji-icon-font) | epaint `default_fonts` | 🚀 등은 wide → run 1개. 테스트 통과 중(`renderer_egui.rs:748-766`) |
| combining | `composed_char(indexed.c, indexed.zerowidth())` | `alacritty_backend.rs:284` | backend에서 합성 완료 |
| 글리프 치환 | `⏺`(U+23FA) → `●`(U+25CF) — 어떤 모노 폰트에도 없어 흑백 emoji 폴백으로 작게 그려지는 문제 회피 | `renderer_egui.rs:262-270` | 그리드 원본은 불변(복사/선택 무영향) |

> **폰트 폴백은 지금 egui/epaint가 전부 공짜로 해주고 있다.** PR-05/PR-06으로 렌더 경로를 빼면
> **폴백 체인·아틀라스·2:1 폭 정합·combining·emoji를 직접 구현해야 한다.** 이것이 §7.2의
> "글리프 아틀라스 = 난이도 상"의 실체이며, PR-05/06 비추천의 가장 큰 실무적 근거다.

> **PR-06 판정: 보류 유지.** 진행 조건은 §0 M3(육안 품질 비교) 하나뿐. 품질이 Ghostty 대비 열등하지
> 않으면 진행하지 않는다.

---

## 9. PR 진행 조건 — 이번 분석으로 확정된 것 / 실측이 필요한 것

### PR-05 (터미널 전용 렌더 경로)

| 계획서 §PR-05 사전판정 조건 | 상태 | 근거 |
|---|---|---|
| 전체 셀을 매 프레임 재구성하지 않음 | ✅ **참 (확정)** | 갤리 shaping은 dirty 행만 — `renderer_egui.rs:152-165`, 실측 `rebuilt_rows=0` |
| 숨겨진 workspace를 렌더링하지 않음 | ✅ **참 (확정)** | 감사 문서 §렌더링(active만 `.show()`) |
| ~~selection/cursor만 바꿔도 전체 재발행~~ | ❌ **위반 (확정)** | §3 — 전 visible row 재발행 + 재테셀레이션 |
| 대량 출력에서 목표 프레임 시간 충족 | ⚠️ **렌더 경로는 충족 (확정), 전체 파이프라인은 M2 실측 필요** | 렌더 0.33ms/frame ≪ 16.6ms. PTY/파서/스냅샷은 미측정 |
| 글자 품질·Retina 정렬 문제 없음 | ⚠️ **M3 실측 필요** | §10 절차 |

> **진행 조건**: 위 "위반" 1건이 성립하지만 **비용이 예산의 2%**라 REPLACE 기준 미달.
> **PR-05는 다음 중 하나가 참일 때만 진행한다.**
> (a) M2 실측에서 **렌더 경로가 p95의 주요 기여자**로 밝혀짐 (현재 산술상 가능성 낮음), 또는
> (b) §3-B "steady-state zero allocation"을 **문자 그대로** 달성해야 한다는 요구가 확정됨
>     (그렇다면 epaint로는 불가능하므로 PR-05가 유일한 길).
> **그 외에는 epaint KEEP.** 대신 §11의 저비용 IMPROVE 2건을 먼저 한다.

### PR-06 (Glyph 렌더러/텍스트 품질)

| 조건 | 상태 |
|---|---|
| monospace 셀 너비 안정성 | ✅ 참 (D2Coding 2:1, 테스트 존재) |
| 한글·CJK wide / combining / emoji fallback | ✅ 참 (egui 폴백 + 백엔드 합성, 테스트 통과) |
| **bold / italic / underline / strikeout** | ❌ **거짓 (확정)** — 단, glyphon 문제가 아니라 **스키마+백엔드 격차**(§8.1) |
| Retina scale 변경 시 atlas 갱신 | ✅ egui가 처리 (`ppp` 변경 시 galley 재생성 경고 경로 `tessellator.rs:2010-2015`) |
| atlas 공유 / 상한·회수 | ✅ Context 1개 = Atlas 1개 |
| **Ghostty/Termius 시각 비교** | ⚠️ **M3 실측 필요 (유일한 미결)** |

> **진행 조건**: M3에서 **육안 품질이 명확히 열등할 때만** 진행. bold/italic 격차는 PR-06과 분리해
> 별도 PR로 처리(스키마 append-only 주의).

### shared buffer pool (§3-A 원칙 15)

| 조건 | 상태 |
|---|---|
| 워크스페이스별 GPU 버퍼가 존재하는가 | ✅ **존재하지 않음 (확정)** — 현재 워크스페이스는 GPU 객체를 하나도 소유하지 않는다. epaint가 프레임마다 단일 Mesh로 병합해 올린다(§5 primitive 2개) |
| **진행 조건** | **PR-05를 하지 않으면 이 항목은 무의미(N/A).** PR-05를 할 때 비로소 "워크스페이스마다 대형 instance buffer" 위험이 생기고, 그때 shared pool이 필요하다. → **PR-05에 종속. 단독 진행 금지.** |

### PR-04 (wgpu/Metal 전환) — **정정**

> **이미 완료 상태다(§7.1).** 남은 일은 "전환"이 아니라 **검증·문서화**뿐:
> ① 런타임에 `wgpu` backend가 Metal인지 로그로 확인, ② Retina/멀티모니터/sleep-wake 회귀 확인,
> ③ 계획서·감사 문서의 "glow" 서술 정정, ④ (선택) glow fallback을 정말 유지할지 결정 —
> 현재는 glow가 **컴파일조차 안 되므로** "롤백 경로"가 실재하지 않는다.

---

## 10. 품질 캡처 절차 (재현 가능 — M3 / PR-06 입력)

> **주의(선행 조건)**: HEAD(`17019ae`) 빌드에는 **glow가 아예 컴파일되지 않는다**(§7.1) — 기본이
> wgpu이기 때문이다. 따라서 "Glow vs Wgpu" 비교는 **glow feature를 켠 빌드가 있어야만** 가능하다.
> 병렬 트랙(B1)이 정확히 그 작업(`features = ["glow"]` + `DEPPY_RENDERER=glow` 런타임 선택)을
> 진행 중이므로, 그것이 머지되면 아래 절차는 **환경변수 한 줄**로 두 백엔드를 띄울 수 있다:
> `DEPPY_RENDERER=glow` (glow) / 미지정 (wgpu, 기본).
> 그 전이라면 아래 절차를 **"wgpu 단독 품질 검증 + Ghostty 비교"** 로 축소해 쓴다.
> **이 문서는 코드를 바꾸지 않았다 — 백엔드 선택 코드는 B1 트랙의 몫이다.**

### 10.1 준비

1. 창 크기·폰트를 고정한다: 설정 → 터미널 폰트 D2Coding 13pt, UI 배율 1.0.
   (`config.font_size / self.ui_scale` — `workspace.rs:1205` — 이 값이 흔들리면 비교가 무의미하다.)
2. 비교 대상 창을 **같은 디스플레이**에 띄운다. 1x/2x 각각 별도 세션:
   - 2x(Retina): 내장 디스플레이 기본 배율
   - 1x: 시스템 설정 → 디스플레이 → 외장 1x 모니터, 또는 `displayplacer`로 scale 1 강제
3. 창 프레임을 고정 좌표에 둔다(`screencapture -R`로 같은 영역을 잘라야 한다).

### 10.2 화면에 출력할 고정 픽스처

터미널에 아래를 그대로 붙여넣어 **1화면**을 만든다. (파일을 만들지 말고 heredoc으로 출력만 한다.)

```sh
printf '%s\n' \
"ASCII  ABCDEFGHIJKLMNOPQRSTUVWXYZ abcdefghijklmnopqrstuvwxyz 0123456789" \
"기호   !\"#\$%&'()*+,-./:;<=>?@[\\]^_\`{|}~ ─│┌┐└┘├┤┬┴┼ ●○◆◇▲▼ ⏺" \
"한글   다람쥐 헌 쳇바퀴에 타고파 — 가나다라마바사아자차카타파하 12345" \
"日本語 いろはにほへと ちりぬるを 漢字混じり文 ｱｲｳｴｵ 全角１２３" \
"中文   中文测试 简体繁體 汉字混排 １２３４５" \
"emoji  🚀 ✅ ❌ ⚠️ 🔥 project/🚀-deploy/config.json" \
"얇은글 iiiillll11 .,;:'\`  IlO0 rn m vv w" \
"" ;
# 색상 (SGR) — 8색 + bright + 배경
for i in 30 31 32 33 34 35 36 37; do printf "\033[%dm%d \033[1;%dm%dB \033[0m" $i $i $i $i; done; echo
for i in 40 41 42 43 44 45 46 47; do printf "\033[%dm %d \033[0m" $i $i; done; echo
# 속성 (현재 미지원 확인용 — bold/italic/underline이 안 나오는 게 정상)
printf "\033[1mBOLD\033[0m \033[3mITALIC\033[0m \033[4mUNDERLINE\033[0m \033[9mSTRIKE\033[0m \033[2mDIM\033[0m \033[7mINVERSE\033[0m\n"
# 24bit
printf "\033[38;2;255;100;0mTRUECOLOR-FG\033[0m \033[48;2;0;80;160mTRUECOLOR-BG\033[0m\n"
```

그 다음 커서/선택 상태를 각각 만든다:

- **커서**: 프롬프트에서 아무 키도 안 누른 상태(블록 커서). `printf '\033[4 q'`로 underline,
  `\033[6 q`로 beam 커서도 각각 1장.
- **선택**: "한글" 행 시작부터 "中文" 행 끝까지 드래그해 하이라이트를 만든 상태로 캡처.

### 10.3 캡처

```sh
# 창 좌표 확인 (한 번만)
#   좌상단 x,y 와 width,height 를 얻는다
# 고정 영역 캡처 (지연 2초 — 드래그 선택 상태 유지용)
screencapture -R <x>,<y>,<w>,<h> -T 2 ~/Desktop/capture/<backend>-<scale>-<case>.png
```

- `<backend>` ∈ {`wgpu`, `glow`, `ghostty`, `termius`}
- `<scale>` ∈ {`1x`, `2x`}
- `<case>` ∈ {`text`, `color`, `cursor-block`, `cursor-underline`, `cursor-beam`, `selection`}
- `screencapture`는 물리 픽셀로 저장하므로 2x는 자동으로 2배 해상도가 된다. **리사이즈 금지** —
  100%·200%·400% 확대해서 육안 비교한다.

### 10.4 비교 체크리스트

각 쌍(wgpu vs glow, wgpu vs Ghostty)에 대해:

- [ ] **ASCII 스템 두께**가 균일한가 (일부 글자만 굵거나 흐릿하지 않은가)
- [ ] **얇은 글자**(`i l 1 . , ; : '` `) 가 뭉개지지 않는가 — 1x에서 특히
- [ ] **한글 2:1 폭 정합** — 한글 2셀이 ASCII 2셀과 정확히 같은 폭인가 (열이 어긋나면 실패)
- [ ] **일본어/중국어**가 폴백 폰트로 나오면서 baseline이 튀지 않는가
- [ ] **emoji**가 컬러로 나오는가, 셀 2칸을 정확히 차지하는가, baseline이 맞는가
- [ ] **박스 드로잉**(`─│┌┐└┘├┤┬┴┼`)이 인접 셀과 **틈 없이 이어지는가** (feathering/rounding 아티팩트 체크)
- [ ] **`⏺` 치환**(→ `●`)이 크기가 맞는가 (`renderer_egui.rs:262-270`)
- [ ] **8색/bright/24bit 색**이 동일한 RGB로 나오는가 (스크린샷에서 픽셀 색 샘플링)
- [ ] **배경 rect 경계**가 1픽셀 겹치거나 벌어지지 않는가 (feathering 확인)
- [ ] **커서**(block/underline/beam)의 위치·크기가 셀 격자와 정확히 일치하는가
- [ ] **선택 하이라이트**가 셀 경계에 정확히 맞는가, wide char 위에서 2셀을 덮는가
- [ ] **1x ↔ 2x**에서 위 항목이 모두 유지되는가 (2x만 예쁘고 1x가 깨지면 실패)
- [ ] **bold/italic/underline/strikeout이 "안 나온다"** — 예상된 격차(§8.1). Ghostty와의 가장 큰 육안 차이일 것

### 10.5 판정

- wgpu가 glow와 **동등 이상**이고 Ghostty 대비 **명확히 열등한 항목이 없으면** → PR-06 **미진행**.
- 열등 항목이 있으면 그 항목만 기록하고, 그것이 **glyph 래스터라이저 문제인지(→ PR-06)**
  **속성 미지원 문제인지(→ §8.1 별도 PR)** 구분한다. 후자면 PR-06은 여전히 미진행.

---

## 11. 권고 (우선순위)

1. **[상] 선택 하이라이트 run 병합** — `paint_selection_row`(`renderer_egui.rs:410-452`)에
   `push_bg_run`과 같은 병합을 적용. 200×60 전체 선택 시 **2.23ms → ~0.1ms 추정**, shape 12,062 → ~120.
   계획서 §7 "드래그 선택 60fps"의 실질 리스크를 제거한다. **PR-05와 무관, ~15줄.**
2. **[상] 문서 정정** — 감사 문서/계획서의 "현재 glow" 서술을 **"이미 wgpu(Metal)"** 로 고치고,
   PR-04를 "전환"에서 "검증·문서화"로 재기술.
3. **[중] `TerminalCell` 텍스트 속성**(bold/italic/underline/strikeout) — backend가 flag를 버리는
   지점(`alacritty_backend.rs:271-291`)부터. wire 호환(append-only) 주의.
4. **[하] `RowRenderCache` Vec/String 재사용** — dirty 행 재구성 시의 alloc도 0으로 (§6 부수 관찰).
5. **[하, 실험] `TessellationOptions::feathering = false`** 실험 — rect 정점 8→4, 인덱스 30→6.
   §10 절차로 육안 회귀만 확인하면 됨. *(효과·부작용 미검증 — 추측)*
6. **PR-05 / shared buffer pool: 보류.** §9의 진행 조건이 참이 되기 전에는 착수하지 않는다.

---

## 12. UNKNOWN / 한계 (사실과 추측의 구분)

**사실 (코드 근거 + 실측 일치, 오차 0)**
- §2 shape 발행 공식과 표, §3 selection-only 재발행, §4 매 프레임 테셀레이션과 정점/인덱스 산술,
  §5 egui 최적화 유무, §6 allocation 수치, §7.1 wgpu 확정, §8.1 속성 필드 부재.

**추측 / 미검증 (본문에 명시)**
- PR-05 이후 CPU가 "~0.02ms/frame"이 된다는 값 — 구현체가 없으므로 이론 추정치다.
- 선택 run 병합 후 "~0.1ms" — 실측 아님(산술 추정).
- rayon feature를 켰을 때의 실효 — `should_parallelize`가 큰 shape만 고르므로 효과가 제한적일
  것이라 봤으나 **미검증**.
- feathering off의 시각적 영향 — **미검증**.
- 한글 전면 화면의 프레임 비용 — scratch 계측기의 기본 egui 폰트에 한글 글리프가 없어 정확히 재지
  못했다. shape 수(글자당 1 run)는 코드로 확정, 시간은 미실측.

**UNKNOWN (실측 필요)**
- **M1** 실제 앱에서 "터미널이 안 바뀐 repaint"가 초당 몇 번 오는가 — 본 분석이 계산한 낭비의
  *실효 크기*를 결정하는 유일한 변수. `FrameStats`에 RepaintCause 기록이 없어 현재 측정 불가
  (감사 문서 PR-01 잔여 항목과 동일).
- **M2** 대량 출력 중 실측 frame p95 (렌더 외 구간 포함).
- **M3** Retina 1x/2x 육안 품질 (§10).
- 멀티 pane(split) 동시 표시 시의 배수 — 본 분석은 단일 pane 기준. pane 수에 선형 비례할 것으로
  보이나(각 pane이 독립 `draw` 호출) 실측 안 함.

**방법론적 한계**
- 시간·allocation 계측은 **scratch 크레이트에서 HEAD의 `terminal` 크레이트를 직접 호출**한 것이다.
  실제 앱은 여기에 eframe/wgpu 업로드·다른 UI 위젯·PTY 파이프라인이 얹힌다. 따라서 표의 수치는
  **렌더 경로의 하한**이며, 앱 전체 프레임 시간이 아니다.
- 공유 작업 트리에서 다른 에이전트가 `renderer_egui.rs`를 동시 편집 중이어서 `cargo test`/`clippy`는
  실행하지 않았다(작업 트리가 일시적으로 비컴파일 상태였다). 대신 HEAD(`17019ae`) 스냅샷을 별도로
  빌드·실행해 검증했다.
- Apple M2 단일 머신 수치다. Intel Mac / Windows에서는 테셀레이션 비용이 더 클 수 있다(**추측**).
