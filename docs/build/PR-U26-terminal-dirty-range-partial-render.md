# PR-U26 — Terminal Dirty-Range Partial Render

작성: 2026-07-05. 상태: 설계(구현 전). 근거: `docs/review/PR-R08-resource-performance-findings.md` Finding 4.
배치: Phase D(성능, PR-U12~U20). **PR-U20 Final Performance Gate 이전**에 착지해야 한다
(게이트의 "active pane frame time p95" 기준이 이 PR로 검증된다). PR-U14(scrollback 바이트 예산)와 독립.

## 0. 배경 / 근거

PR-R08 Finding 4는 터미널 렌더 비용을 지적한다: backend는 dirty rows를 계산하지만
session layer는 `dirty: bool`만 보존하고, `viewport_snapshot()`은 매번 `cols * rows`
`Vec<TerminalCell>`을 새로 만들며 `dirty_ranges`는 비어 있고, renderer는 모든 visible cell을
매 프레임 개별 순회한다. 즉 **dirty 추적 인프라는 이미 존재하나 파이프라인 중간에서 버려진다.**

`crates/terminal/src/alacritty_backend.rs`의 `viewport_snapshot()`에는 이미 다음 주석이 있다:
> `dirty_ranges: Vec::new()` — 항상 빈 값. 아직 소비자(부분 렌더러)가 없다. 채우려면
> 스크롤/리플로우 좌표계와 함께 설계해야 하므로 부분 렌더 도입 시 같이 간다.

이 PR이 그 "부분 렌더 도입"이다.

## 1. 현황 진단 (실제 코드 흐름)

| 지점 | 파일 | 현재 상태 |
|---|---|---|
| damage 계산 | `crates/terminal/src/alacritty_backend.rs::feed()` | alacritty `damage()`로 `dirty_rows` **이미 계산** → `TerminalChangeSet.dirty_rows` |
| damage 폐기 | `crates/session/src/session.rs::pump()` | `changes.dirty_rows`를 **버리고** `self.dirty = true` 하나로 뭉갬 |
| 전체 재할당 | `crates/terminal/src/alacritty_backend.rs::viewport_snapshot()` | 매번 `cols*rows` Vec 전체 재할당, `dirty_ranges: Vec::new()` |
| 전체 재구성 | `crates/terminal/src/renderer_egui.rs::draw()` | **완전 stateless** — 매 프레임 모든 셀을 `painter.text()`로 개별 재구성 |

## 2. 비용 분리 (혼동 금지)

- **비용 A — dirty tick마다 스냅샷 전체 재할당**: 출력이 잦은 pane에서 빈번한 `cols*rows` 할당.
- **비용 B — egui 프레임마다 전체 재-shape/재-paint**: egui 즉시모드의 지배적 CPU 비용.
  현재 셀당 `painter.text()`를 `cols*rows`회 호출하며 text shaping을 반복한다.

**설계 판단**: egui는 즉시모드라 retained canvas식 "dirty-rectangle 부분 repaint"는 프레임워크와
싸우는 길이다. 진짜 이기는 지점은 **비용 B의 텍스트 레이아웃 재사용**이다 — 안 바뀐 행의
galley를 재사용하고 바뀐 행만 재구성한다. 비용 A(부분 스냅샷 diff)는 스크롤/리플로우 좌표계
설계가 얽혀(위 코드 주석이 경고한 부분) **remote delta 경로와 함께 별도 PR로 미룬다.**

참고: 이미 적용된 최적화(B08a idle repaint 제거 + Viewport wake)로 앱은 출력/상호작용이
있을 때만 repaint한다. 따라서 비용 B는 **고출력 active pane**에서 집중적으로 발생한다
(idle에는 프레임 자체가 안 돈다).

## 3. 스코프 3부

### Part 1 — dirty_rows 파이프라인 복원 (저위험)

- `Session`에 `pending_dirty` (행 인덱스 집합 또는 "전체 dirty" 플래그) 추가.
- `pump()`이 각 `feed()`의 `changes.dirty_rows`를 **union 누적**.
- `take_snapshot()`이 누적분을 `snapshot.dirty_ranges`(행 범위)로 이관하고 clear.
- **전체 무효화 트리거** (feed를 안 거쳐 damage가 안 잡히는 경로):
  - alt-screen 토글
  - `scroll` (display_offset 변경 — 모든 행이 시프트)
  - `resize` (cols/rows 변경)
  이들은 "전 행 dirty"로 표시한다.

### Part 2 — UI 측 per-row galley 캐시 (핵심 CPU 이득)

- `crates/terminal/src/renderer_egui.rs`에 `TerminalRenderCache { rows: Vec<Option<Arc<Galley>>>, cols, rows, alt, scroll }` 도입.
- `WorkspaceUi`의 `SessionView`가 pane별로 캐시를 소유.
- `draw()` 시그니처에 `&mut cache` 추가. 각 행 처리:
  - `snapshot.dirty_ranges`에 포함 **or** 캐시 미스 → 그 행의 텍스트런을 재구성
  - 아니면 캐시된 galley 재사용
- `cols`/`rows`/`alt`/`scroll` 변화 시 캐시 전체 무효화.
- 커서는 기존대로 오버레이 rect — **cursor-only 이동은 행 재구성 불필요**.

### Part 3 — 셀→행 배치 렌더링

- 현재 셀당 `painter.text()` (`cols*rows`회)를 **행당 텍스트런**으로 전환:
  연속된 동일 style(fg/bg) 셀을 한 galley로 배치.
- dirty 여부와 무관하게 per-frame 오버헤드 자체가 감소(부수 이득).
- wide/wide_spacer 셀 경계 처리 주의(§6 회귀 리스크).

## 4. 파일별 변경

```text
crates/terminal/src/change_set.rs         dirty_rows 이미 있음 (변경 없음)
crates/session/src/session.rs             pending_dirty 누적 + take_snapshot 이관
                                          + scroll/resize/alt-screen 전체 무효화
crates/terminal/src/alacritty_backend.rs  viewport_snapshot이 dirty_ranges를 채우도록
                                          (session이 누적분 주입 또는 별도 경로)
crates/terminal/src/renderer_egui.rs      TerminalRenderCache + draw(&mut cache) + 행 배치 렌더
crates/app/src/ui/workspace.rs            SessionView에 render_cache 필드 추가, draw 호출부 수정
```

## 5. 완료 기준

- 한 행만 바뀌는 출력에서 `dirty_ranges`가 **해당 행만** 포함 (단위 테스트).
- 대량 출력 active pane에서 프레임당 galley 재구성 수가 **dirty 행 수에 비례**(전체 아님).
- `PR-U20 Scenario C`(hidden 10 + 대량 출력 3)에서 **active pane frame time p95 개선** — 게이트로 측정.
- 렌더 출력 픽셀 동일 (스냅샷 회귀 없음).
- `cargo test -p terminal -p session -p deppy-sijo` pass.

## 6. 회귀 리스크 (PR-R08 명시 — 반드시 테스트)

1. **CJK/wide char**: wide/wide_spacer 셀이 행 텍스트런 경계에서 깨지지 않게.
2. **cursor-only 변화**: 커서만 이동 시 행 재구성 안 함 (오버레이만).
3. **alt-screen 전환**: 전체 무효화.
4. **scrollback 스크롤**: display_offset 변하면 전 행 무효화 (모든 행 시프트).
5. **resize**: cols/rows 변하면 캐시 폐기.
6. **IME preedit**: 조합 중 텍스트는 커서 위치 오버레이라 행 캐시와 독립 유지.

## 7. 구현 순서 (/ak 분할)

1. **Part 1** (파이프라인) → 검증: 한 행 변경 시 dirty_ranges 단위 테스트.
2. **Part 2 + Part 3** (렌더 캐시 + 행 배치) → 검증: 픽셀 동일 + 재구성 수 계측 + 회귀 6종 테스트.
3. 통합 → PR-U20 Scenario C 프레임타임 A/B (전/후 p95 비교).

## 8. 명시적 비목표 (이 PR 밖)

- 부분 **스냅샷 diff**(비용 A) — 스크롤/리플로우 좌표계 설계 필요, remote delta 경로와 함께 별도 PR.
- scrollback 바이트 예산 — PR-U14 별도.
- output backpressure — PR-U15 별도.
