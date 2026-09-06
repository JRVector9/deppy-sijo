# 이력(work history) 카드 목록 프레임당 낭비 제거 (2026-08-19)

작성일: 2026-08-19
브랜치: `perf/work-history-virtualize` (`main` 기준)
성격: **정적 감사(오케스트레이터가 사전 확인) + 실측(순수 함수 마이크로벤치, `Instant`)**.
GUI 앱은 빌드·실행하지 않았다(작업 지시 — 사용자 앱이 떠 있음). 렌더 카운트는 `egui_kittest`
헤드리스 하네스로 실측했다.

## 0. 요약 (TL;DR)

| Priority | Location | Finding | Impact | Next step |
|---|---|---|---|---|
| 상 | `crates/app/src/ui/work_history.rs` 옛 `show()` 내부 `presentations.iter().find(...)` | 카드 렌더 루프 안에서 매 행마다 `presentations`(최대 256개) 전체를 `WorkTurnIdentity::matches`(문자열 4개 비교)로 선형 탐색 — 256행×256presentation×4비교 = 프레임당 최대 65,536쌍 비교(추정, 문제 확인 시점 코드 읽기 기준) | 이력 탭이 열려 있는 동안 매 프레임 O(N²) 문자열 비교 | **수정 완료**: `WorkHistoryGroup::row_indices`로 행↔presentation을 인덱스 대응시켜 O(1) 조회로 대체. 실측 256×256 비교 2,000회 반복: 선형 736.5ms → 인덱스 3.8ms(약 194배) |
| 상 | `crates/app/src/app.rs` 옛 `App::work_history_presentations()` | `history_tab_active` 게이트 하나만으로 매 프레임 **행마다** `WorkTurnIdentity::from(row)`(String 4개 할당)를 호출 — 256행 시 프레임당 1,024개 String 할당(추정) | GC 없는 Rust라도 힙 할당·해제 비용이 매 프레임 누적 | **수정 완료**: `WorkHistoryActionPresentation`에서 `identity` 필드 자체를 없앴다 — 펼쳐진 카드 하나의 클릭 핸들러에서만(프레임당 최대 1회) `WorkTurnIdentity::from(row)`를 즉석으로 만든다. 실측 256개 `WorkTurnIdentity::from` 2,000회 반복: 151.7ms → 이 경로가 통째로 없어졌으므로 **0** |
| 중 | `crates/app/src/ui/work_history.rs` 옛 `show()`의 `egui::ScrollArea::vertical().show()` | 이력 탭의 모든 그룹·모든 카드를 매 프레임 렌더(가상화 없음) — 이 저장소의 다른 목록(`file_tree.rs` `show_rows`, `transcript_viewer.rs` `show_viewport`)은 전부 가상화돼 있는데 이 목록만 예외였다 | 스크롤 프레임 비용이 뷰포트가 아니라 전체 행 수(최대 256)에 비례 | **수정 완료**: transcript_viewer.rs의 측정-높이 기반 `show_viewport` 패턴을 그대로 따랐다(그룹 헤더+카드를 "슬롯"으로 평탄화). 실측(256행/32그룹, 900×700 뷰포트): 첫 프레임(부트스트랩) 288슬롯 전부 → 안정 상태(가상화) 9슬롯 |
| 중 | `crates/app/src/ui/work_history.rs` 옛 `visible_rows`/`grouped_rows` | `query`/`filter`/`providers`/`sort_mode`/행 목록이 그대로여도 매 `show()`마다 필터+`sort_by`+`O(n·그룹수)` 그룹핑을 다시 계산 | 이력 탭이 떠 있는 동안 조작이 없어도 매 프레임 정렬·그룹핑 재계산 | **수정 완료**: `WorkHistoryUi::cached_grouped_rows` — App이 넘기는 `rows_revision`(행 목록이 실제로 바뀔 때만 App이 올림)과 query/filter/providers/sort_mode를 키로 캐시. 실측 256행 2,000회 반복: MISS(재계산) 475.1ms vs HIT(캐시 재사용) 190.8ms(약 2.5배) |
| 낮음 | `crates/app/src/ui/work_history.rs` `provider_badge` | `kind.trim().to_ascii_lowercase()`를 배지 텍스트용·색용으로 각각 새로 할당(카드당 힙 alloc 2회) | 카드 렌더당 String 할당 1개 초과 | **수정 완료**: 한 번만 계산해 재사용 — 카드당 2회 → 1회 |

## 1. 측정 방법

전부 `crates/app/src/ui/work_history.rs`의 `#[cfg(test)] mod tests` 안에 **임시로** 추가한
`#[ignore]` 벤치 함수로 쟀다(`Instant::now()`, `cargo test -- --ignored --nocapture`로 실행).
**커밋 전에 전부 제거했다** — 이 문서가 그 대체 기록이다. 렌더 카운트(§4)는 `render_slot`에
임시 `#[cfg(test)] static AtomicUsize` 카운터를 심어 `egui_kittest::Harness::run()` 전후로
읽었고, 마찬가지로 커밋 전 제거했다.

- §2·§3의 시간값은 **실측**(내 머신, unoptimized `dev` 빌드, 2,000회 반복 평균이 아니라 총합 —
  1회당 값은 총합/2,000으로 환산해 표기).
- §0 표의 "65,536쌍"·"1,024개"는 **추정**(문제를 처음 확인할 때 코드를 정적으로 읽어 계산한
  값, 오케스트레이터가 작업 지시에 이미 명시) — §2·§3에서 실측으로 재확인했다.
- §4의 "288"은 `build_slots(...).len()`을 직접 호출해 **구조적으로 확인**한 값(실측이라기보다
  코드가 실제로 그렇게 동작함을 유닛 테스트로 고정한 것)이고, "9"는 `egui_kittest` 헤드리스
  렌더로 **실측**했다.

## 2. Before/After — presentation 조회 (스펙 이슈 #2)

256행을 32그룹에 고르게 흩어 만들고, 옛 방식(`identities.iter().position(|id| id.matches(row))`,
`WorkTurnIdentity::matches`와 동일한 문자열 4개 비교)과 새 방식(인덱스로 직접 슬라이싱)을 각각
256×256번(행마다 전체를 훑는 최악 경로) × 2,000회 반복해 쟀다.

| | 총합(2,000회) | 1회(추정 프레임 1회) |
|---|---|---|
| OLD (선형 탐색, `WorkTurnIdentity::matches`) | 736.5ms | **368.2µs** |
| NEW (인덱스 슬라이싱) | 3.8ms | **1.9µs** |

**약 194배.** 카드가 몇 백 개 안 되는 규모에서도 O(N²) 문자열 비교는 명확히 관측 가능한 비용
이었다.

## 3. Before/After — presentation 할당 + 캐시된 그룹핑 (스펙 이슈 #3·#4)

### 3-1. `WorkTurnIdentity::from(row)` 256회 (구 `App::work_history_presentations()` 경로)

| | 총합(2,000회) | 1회 |
|---|---|---|
| OLD (256개 전부 할당) | 151.7ms | **75.9µs** |
| NEW | 이 경로 자체가 없어짐 | **0** |

`WorkHistoryActionPresentation`에서 `identity` 필드를 없애서, 이 256회 할당이 프레임마다
일어나던 구조를 통째로 지웠다. 남은 유일한 `WorkTurnIdentity::from(row)` 호출은 카드를
펼친 상태에서 「이동」/「재개」/「Git 변경 보기」 버튼을 **클릭했을 때만**(프레임당 최대 1회)
`render_card` 안에서 그 자리의 `row`로 즉석에서 만든다 — 이미 있던 「원문 보기」 버튼과 같은
패턴이다.

### 3-2. `cached_grouped_rows` MISS vs HIT (256행/32그룹)

| | 총합(2,000회) | 1회 |
|---|---|---|
| MISS (매번 재계산 강제) | 475.1ms | **237.5µs** |
| HIT (캐시 재사용) | 190.8ms | **95.4µs** |

**약 2.5배.** HIT도 0에 가깝지 않은 이유는 캐시가 건너뛰는 건 필터+`sort_by`+그룹핑(`O(n log n
+ n·그룹수)`)뿐이고, 그룹 재조립(`materialize_group`, 인덱스로 `WorkHistoryRow`를 다시 모으고
`latest_with_value`로 model/effort/branch/변경 수를 다시 뽑는 일)은 캐시 히트여도 매번 하기
때문이다 — 이건 표시되는 행 수에 비례하는 `O(shown)`이라 정렬·그룹핑의 `O(n log n + n·그룹수)`
보다 훨씬 싸지만 공짜는 아니다. 캐시 범위는 **입력이 바뀌지 않으면 반드시 재사용**하는 것(=
매 프레임 무조건 재계산하던 것을 없애는 것)이지, 렌더 자체를 없애는 게 아니다.

## 4. Before/After — 목록 가상화 (스펙 이슈 #1)

256행을 32그룹(그룹당 8행)에 나눠 담고 900×700 뷰포트로 `egui_kittest::Harness`를 띄웠다.

- `build_slots(...)`가 만드는 슬롯 수(헤더 32 + 카드 256) = **288** — 유닛 테스트
  (`펼친_그룹은_헤더_다음에_카드_슬롯이_행_수만큼_있고_마지막만_그룹_끝이다` 등)로 구조를
  고정했다.
- 부트스트랩 프레임(슬롯 구성이 바뀐 뒤 첫 프레임)은 이 288개를 **전부** 배치해 높이를 잰다
  (착지 정확성을 위해 타협하지 않음 — transcript_viewer.rs와 같은 계약).
- 안정 상태(그 다음 프레임부터, 스크롤이나 조작이 없는 한 계속): `render_slot` 호출 횟수를
  임시 카운터로 재면 **9**(뷰포트에 걸치는 슬롯 + overscan 4개 앞뒤).

**288 → 9, 약 32배 감소.** 이 저장소의 다른 큰 목록(`file_tree.rs`, `transcript_viewer.rs`)이
이미 가지고 있던 성질을 이력 목록도 갖게 됐다 — 스크롤 프레임 비용이 총 행 수가 아니라 뷰포트
크기로 유계가 된다.

## 5. 각 항목이 실제로 무엇을 고쳤는지

1. **O(N²) presentation 선형 탐색 제거** (`crates/app/src/ui/work_history.rs`)
   `WorkHistoryGroup`이 `row_indices: Vec<usize>`를 새로 들고 다닌다 — 그룹을 만드는
   `grouped_row_indices`/`materialize_group`이 필터+정렬을 거친 뒤에도 원본 `rows` 슬라이스에서의
   위치를 잃지 않는다. `render_slot`은 카드를 그릴 때 `presentations.get(group.row_indices[k])`로
   O(1) 조회한다 — 문자열 비교가 전혀 없다.

2. **프레임당 String 1,024개 할당 제거** (`crates/app/src/app.rs`, `crates/app/src/ui/work_history.rs`)
   `WorkHistoryActionPresentation`에서 `identity: WorkTurnIdentity` 필드를 없앴다. App은
   `render_work_history_tab_body` 한 곳에서 `rows`와 `presentations`를 **같은 순회**로 함께 만들어
   (`self.work_history_rows.iter().map(|row| { presentations.push(...); WorkHistoryRow::from(row) })`)
   인덱스 정합을 구조적으로 보장한다 — 두 슬라이스를 따로 만들면서 순서가 어긋날 걱정을 아예
   없앴다. 클릭 시점 identity 생성은 §3-1 참고.

3. **목록 가상화** (`crates/app/src/ui/work_history.rs`)
   `egui::ScrollArea::vertical().show()` → `.show_viewport()`로 바꿨다. 그룹 헤더 한 줄·카드 한
   장을 각각 "슬롯"(`WorkHistorySlot`)으로 평탄화하고(접힌 그룹은 헤더 슬롯 하나만), 슬롯별
   실측 높이(`list_heights: Vec<Option<f32>>`)를 캐시해 누적합으로 오프셋을 구한다
   (transcript_viewer.rs `slot_offsets`/`visible_range`와 같은 공식, 이 파일 안에 별도로 복제 —
   두 파일이 서로 참조하지 않게 하기 위해 의도적으로 중복했다). 슬롯 구성이 바뀔 수 있는
   입력(리비전·질의·필터·provider·정렬 모드·보조 검색·접기 상태)이 하나라도 바뀌면
   `WorkHistoryListShape` 서명 비교로 감지해 그 프레임만 부트스트랩(전부 배치)한다. 카드 선택
   (펼침)은 슬롯 개수를 안 바꾸므로 별도 처리 없이 가상화 렌더 루프의 자기보정
   (`list_heights[index] != Some(measured)`이면 갱신 + `request_repaint`)이 알아서 따라잡는다.

4. **필터·정렬·그룹핑 메모이즈** (`crates/app/src/ui/work_history.rs`)
   `WorkHistoryUi::cached_grouped_rows`가 `WorkHistoryGroupedCacheKey`(rows_revision, query,
   filter, providers, sort_mode)로 이전 프레임과 같은지 비교해, 같으면 저장해 둔 인덱스 계획을
   그대로 재사용한다. `rows_revision`은 App이 새로 추가한 `work_history_rows_revision: u64`
   필드로, `self.work_history_rows`를 실제로 고치는 **세 지점**(git in-place 갱신
   `poll_work_history_git`, 스냅샷 통째 교체 `apply_agent_state_projection_result`의
   `WorkHistory` 분기, 워크스페이스 이탈 시 `clear()`)에서 빠짐없이 올린다.

5. **provider_badge 중복 할당 제거** (`crates/app/src/ui/work_history.rs`)
   `kind.trim().to_ascii_lowercase()`를 한 번만 계산해 배지 텍스트·색 판정 양쪽에서 재사용.

## 6. 추가한 테스트

- **캐시(이슈 #4)**: `캐시된_그룹핑은_캐시_없는_그룹핑과_같은_결과를_돌려준다`(정확성),
  `캐시_키가_같으면_저장된_계획을_다시_계산하지_않고_그대로_쓴다`(캐시된 계획을 일부러 틀리게
  조작한 뒤 같은 키로 다시 불러 조작값이 그대로 나오는지 확인 — 재계산을 건너뛴다는 걸
  양성으로 증명), `캐시_키가_하나라도_바뀌면_저장된_계획을_버리고_다시_계산한다`(rows_revision·
  query·filter·providers·sort_mode 5개 각각을 바꿔 가며 캐시 키가 갱신되는지 확인 — 재계산이
  실제로 일어났다는 뜻).
- **슬롯 구성(이슈 #1)**: `접힌_그룹은_헤더_슬롯_하나만_만든다`,
  `펼친_그룹은_헤더_다음에_카드_슬롯이_행_수만큼_있고_마지막만_그룹_끝이다`.
- **가상화 동작(이슈 #1)**: `kittest_뷰포트_밖_카드는_배치되지_않고_최근_카드는_보인다`
  (transcript_viewer.rs 테스트와 같은 계약 — 부트스트랩 프레임 vs 가상화 프레임).
- **선택 보존(절대 지켜야 할 것)**: `kittest_화면_밖으로_스크롤해도_선택한_카드_상태는_유지된다`
  — 화면 밖 카드를 선택한 채 부트스트랩+가상화 프레임을 지나도 `WorkHistoryUi::selected`가
  그대로인지 확인.
- **presentation 인덱스 정합(이슈 #2·#3)**:
  `kittest_정렬로_순서가_바뀌어도_각_카드는_자기_프레젠테이션을_보여준다` — 입력 배열 순서와
  정렬 후 화면 순서를 일부러 뒤집어, 인덱스 대응이 깨지면 카드가 이웃의 버튼을 보여주는 사고를
  잡아낸다.

기존 회귀 테스트(48개, 전부 `visible_rows`/`grouped_rows`의 필터·정렬·그룹핑 동작과 카드
상호작용을 검증)는 시그니처를 유지한 채(`#[cfg(test)]`로 표시해 프로덕션 경로에서는 더 이상
쓰이지 않음을 명시) 전부 그대로 통과한다 — 새 인덱스 기반 구현이 기존 동작과 동일한 결과를
낸다는 근거다.

## 7. 게이트 결과

- `cargo test -p deppy-sijo`: **1,793 passed, 0 failed, 11 ignored**(기존에도 ignored였던
  macOS phys_footprint 실측 8개 + pty/기타 3개, 이번 작업과 무관).
- `cargo clippy --workspace --all-targets -- -D warnings`: **0 경고**.
- `cargo run -q -p xtask -- check-boundary`: **통과**("UI leaf boundary guard passed; zero
  allowlist capability").
- `cargo fmt --all -- --check`: 건드린 두 파일(`crates/app/src/app.rs`,
  `crates/app/src/ui/work_history.rs`) 모두 **새 어긋남 없음** — 정규화된 diff 블록 비교로
  `main` 대비 새로 생긴 fmt 차이가 0개임을 확인했다(app.rs는 기존 37개 무관 diff가 그대로 남아
  있고, 하나도 새로 늘지 않았다; work_history.rs는 대량 편집이라 기존 5개 무관 diff까지 함께
  정리됐다).

## 8. 화면으로 검증하지 못한 항목

작업 지시에 따라 `scripts/dev-run.sh`로 앱을 실행하지 않았다(사용자 앱이 떠 있음). 따라서
아래는 코드 읽기·유닛/kittest 테스트·정적 감사로만 검증했고 **실제 화면에서는 보지 못했다**:

- 스크롤 위치가 실제 마우스/트랙패드 조작에서도 프레임 사이 튀지 않는지(transcript_viewer.rs가
  2026-08-18에 겪은 "깜빡임" 회귀와 같은 종류의 문제가 이 목록에도 없는지) — 부트스트랩+가상화
  2프레임 전환은 테스트로 확인했지만, 연속 스크롤 중 여러 프레임에 걸친 체감은 미검증.
  실측 높이가 프레임마다 살짝 흔들리는 콘텐츠(예: 카드 펼침 애니메이션 같은 게 생기면)에서의
  시각적 안정성도 미검증.
  - **후속 완화**: 이 리스크를 완전히 없애려면 `scripts/dev-run.sh`로 실제 실행해 스크롤을
    조작해 봐야 한다 — 사용자 앱이 비는 대로 진행 권장.
- 좁은 pane split 폭에서 가상화된 카드의 실제 렌더 결과가 시각적으로 기존과 동일한지(좁은 폭
  테스트는 1개 그룹·1개 카드짜리 소규모라 부트스트랩 경로만 지나갔다 — 가상화 경로에서의 좁은
  폭은 별도로 못 봤다).
- 실제 macOS 폰트/DPI 환경에서 카드 높이 실측값이 기대한 범위(대략 70~100px)인지, 그에 따라
  overscan 4개가 체감상 충분한지(빠르게 스크롤할 때 빈 프레임이 한순간 보이는지) — overscan
  값은 transcript_viewer.rs와 같은 값을 그대로 가져왔을 뿐 이력 카드 전용으로 튜닝하지 않았다.
