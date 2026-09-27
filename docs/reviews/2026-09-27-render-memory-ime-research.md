# 화면 렌더링·메모리·한글 입력 비교 조사

| Priority | Location | Finding | Impact | Next step |
| --- | --- | --- | --- | --- |
| medium | terminal/alacritty_backend.rs:29 | 단일 char로 합성되지 않는 결합 문자를 버림 | 옛한글·일부 결합 문자가 표시·복사에서 유실 | 희소 grapheme 정보 보존 |
| medium | app/ui/workspace.rs:7518 | IME active_range_chars를 저장하지 않음 | 조합 내부 선택·커서와 후보창 위치를 정확히 표현할 수 없음 | 조합 문자열과 범위를 함께 관리 |
| medium | terminal/alacritty_backend.rs:402 | dirty 행과 무관하게 전체 grid를 새로 할당·변환 | 출력 중 반복 할당·복사 | 행 공유 snapshot 또는 재사용 버퍼 |
| medium | vendor/grid/storage.rs:400 | 압축 행마다 새 Row를 만들어 scratch를 교체 | 과거 화면 읽기에서 할당 증가 | decode_into로 scratch 재사용 |
| medium | terminal/renderer_egui.rs:863 | wide 문자마다 별도 갤리·shape | 한글 화면에서 painter 호출 급증 | 동일 스타일의 CJK run 병합 |
| medium | terminal/viewport_snapshot.rs:55 | 셀 크기가 주석의12B가 아닌16B | snapshot 셀 배열의25% 축소 여지 | wide/spacer와 attrs의 flag 통합 |
| low | vendor/grid/storage.rs:408 | footprint 조회마다 압축 슬롯 전체를 순회 | 히스토리·세션 수에 따라 worker CPU 증가 | 일관성 있는 증분 합계 |
| low | terminal/renderer_egui.rs:580 | 캐시 적중이어도 전체 visible row shape를 다시 발행 | 무변화 repaint에서도 테셀레이션 비용 | 계측 후 GPU retained path 여부 결정 |
| low | session/session.rs:558 | scroll offset만 읽기 위해 전체 snapshot 생성 | 프롬프트 점프마다 불필요한 할당 | backend의 경량 메타데이터 API |

위 우선순위는 이번 조사에서의 개선 순서다. 확정된 문자 유실, 확인된 구현 제약, 성능 개선 후보를 아래에서 구분한다. 모든 항목이 현재 사용자에게 발생하는 장애라는 의미는 아니다.

## 조사 범위와 방법

- 날짜: 2026-09-27. 제품 기준은 직전 수정이 완료된 `/Users/jr/Desktop/projects/deppy-sijo-cloud-agent-mcp`, `feat/cloud-agent-mcp`, `78f054b9`.
- 원래 cwd `/Users/jr/Desktop/projects/deppy-sijo`, `331b78b3`에서도 전체 snapshot 할당, 압축 행 decode 교체, 전 행 paint, preedit 문자열만 보관하는 경로가 남아 있음을 확인했다. 아래 라인과 측정은 최신 feature worktree 기준이다. 두 branch 전체가 동일하다고 주장하지 않는다.
- 사용자 요청의 Orca는 Rust·GPUI 기반 **OrcaShell**(`bhensley5/orcashell`)로 해석했다. 동명의 Electron/Rust-engine 프로젝트 Orca ALab, CLI coding agent 등과 구분했다.
- GitHub/공식 문서를 웹 검색하고, OrcaShell·Warp·Alacritty를 저장소 밖 `/tmp/deppy-terminal-research-*-20260927`에 shallow clone하여 실제 Rust 소스를 읽었다. 다른 프로젝트의 소개 문구를 Deppy의 벤치 결과로 취급하지 않았다.
- 현재 Deppy의 release 렌더 벤치, 실제 backend/renderer를 부르는 저장소 밖 release probe, 기존 한글/IME 테스트를 실행했다.
- 제품 소스 변경, 앱 실행/재실행, 공개 터널, push는 하지 않았다. 실제 macOS 입력기 타이핑, GPU present, 앱 전체 RSS·프레임 p95는 측정하지 않았다.

외부 소스 고정 revision:

| 프로젝트 | 확인한 revision | 확인한 내용 |
| --- | --- | --- |
| OrcaShell | `f19907b9b59b9dd5731ad91751c04307ae84609c` | damage 행만 snapshot에 복사; 모든 visible 행은 캐시에서 paint replay |
| Warp | `5af88f49f84e70025f9c19e13f6b9ae64b624627` | visible 범위 제한, glyph cache, 배경 batching, flat scrollback, IME 문자열+선택 범위 |
| Alacritty | `d692748d3f61253ebe9f5094320120d22f6a046f` | damage tracking, preedit cursor/선택/표시 범위와 후보창 위치 |

## 비교에서 확인한 차이

### OrcaShell: 변경된 행만 snapshot에 넣는다

`snapshot_frame`은 damage·geometry·palette cache를 검사하고, 다시 계산할 행의 셀만 flat buffer로 복사한다. Deppy는 변경된 행만 **shaping**하지만 snapshot 생성은 전체 셀을 순회한다. 따라서 가져올 아이디어는 snapshot 단계의 변경 행 처리다. OrcaShell도 paint replay는 모든 visible 행을 돈다. “Orca는 변경된 행만 GPU에 그리므로 Deppy도 전체 paint를 없애면 된다”는 결론은 소스에서 나오지 않는다. [OrcaShell renderer:688](https://github.com/bhensley5/orcashell/blob/f19907b9b59b9dd5731ad91751c04307ae84609c/crates/orcashell-terminal-view/src/renderer.rs#L688), [paint replay:1076](https://github.com/bhensley5/orcashell/blob/f19907b9b59b9dd5731ad91751c04307ae84609c/crates/orcashell-terminal-view/src/renderer.rs#L1076).

OrcaShell 터미널 뷰 소스에서 `EntityInputHandler`/marked-text/조합 범위를 직접 처리하는 구현은 확인하지 못했다. 키 이벤트 중심 경로만으로 OrcaShell의 한글 IME가 더 우수하다고 결론내리지 않았다. [TerminalView](https://github.com/bhensley5/orcashell/blob/f19907b9b59b9dd5731ad91751c04307ae84609c/crates/orcashell-terminal-view/src/terminal_view.rs).

### Warp: visible 범위·glyph cache·flat history·IME range

Warp는 renderer에서 visible row 범위를 제한하고, 문자/문자열+font 기준으로 glyph를 캐시하며 같은 배경을 묶는다. Deppy도 visible snapshot, 행 갤리 캐시, 같은 배경/선택 영역 run 병합을 이미 한다. Warp가 GPU를 사용한다는 이유로 Deppy에 GPU 도입이 필요하다는 결론은 부정확하다. 현재 Deppy의 기본 eframe feature는 이미 wgpu다. [Warp grid renderer:326](https://github.com/warpdotdev/warp/blob/5af88f49f84e70025f9c19e13f6b9ae64b624627/app/src/terminal/grid_renderer.rs#L326), [glyph cache](https://github.com/warpdotdev/warp/blob/5af88f49f84e70025f9c19e13f6b9ae64b624627/app/src/terminal/grid_renderer/cell_glyph_cache.rs).

Warp의 scrollback은 grid와 분리한 `FlatStorage`에 text content·색/스타일 interval map을 보관한다. 내용은 chunk 단위라 앞쪽 삭제도 chunk를 제거한다. Deppy의 텍스트+속성 run 압축은 같은 방향의 최적화를 이미 가진다. 전체 저장 모델 교체보다 먼저 현재 압축 행의 읽기 할당과 bookkeeping을 줄이는 것이 타당하다. 이것은 코드 비교에 따른 판단이며 두 제품의 RSS 비교 실측은 아니다. [Warp FlatStorage](https://github.com/warpdotdev/warp/blob/5af88f49f84e70025f9c19e13f6b9ae64b624627/crates/warp_terminal/src/model/grid/flat_storage/mod.rs), [chunk content](https://github.com/warpdotdev/warp/blob/5af88f49f84e70025f9c19e13f6b9ae64b624627/crates/warp_terminal/src/model/grid/flat_storage/content.rs).

Warp의 `SetMarkedText`는 문자열과 `selected_range`를 함께 전달한다. macOS callback과 winit 경로 모두 범위를 받는다. Deppy도 egui에서 `active_range_chars`를 받지만 UI가 `text`만 저장한다. 따라서 범위를 UI 모델과 renderer까지 보존하는 개선은 현재 스택에서도 가능하다. Warp의 편집기/블록 입력 구조를 전부 가져올 필요는 없다. [Warp event:207](https://github.com/warpdotdev/warp/blob/5af88f49f84e70025f9c19e13f6b9ae64b624627/crates/warpui_core/src/event.rs#L207), [macOS callback:1573](https://github.com/warpdotdev/warp/blob/5af88f49f84e70025f9c19e13f6b9ae64b624627/crates/warpui/src/platform/mac/window.rs#L1573).

### Alacritty/winit: 한글 종료 키 패치는 이미 Deppy에 있다

winit PR4478은 macOS Korean IME의 중복 Space와 조합 확정 뒤 ASCII 키 유실을 다룬다. 웹 확인 시 PR은 Open으로 표시된다. Deppy는 이미 이 수정의 backport와 AppKit/egui 물리 키 조정을 보유한다. 따라서 “winit을 최신으로 올리면 해결된다”거나 기존 패치를 바로 제거하라는 권고는 하지 않는다. [winit PR4478](https://github.com/rust-windowing/winit/pull/4478), [Alacritty issue8079](https://github.com/alacritty/alacritty/issues/8079), 로컬 `third_party/winit-0.30.13/DEPPY_BACKPORT.md`.

Alacritty는 preedit cursor offset/선택 길이를 사용하고 긴 조합 문자열을 화면 안으로 제한하며 후보창 위치를 계산한다. Deppy는 preedit 전체에 같은 밑줄을 그리고 후보창은 터미널 커서 셀에 둔다. 조합 내부 위치에 따른 처리 차이가 있다. [Alacritty draw_ime_preview:1137](https://github.com/alacritty/alacritty/blob/d692748d3f61253ebe9f5094320120d22f6a046f/alacritty/src/display/mod.rs#L1137).

## Details: 현재 코드의 개선 후보

### 1. 결합 문자 유실 — 실제 재현한 표시·복사 결함

- `crates/terminal/src/alacritty_backend.rs:29`의 `composed_char`는 NFC 결과가 정확히 한 char일 때만 쓴다. 여러 scalar가 남으면 원래 base만 반환한다.
- `TerminalCell.c` 자체가 char라 전체 grapheme를 보관할 수 없다. `selection_text`도 이 flattened snapshot을 복사한다.
- 실제 backend→snapshot→selection_text probe 결과: `한`은 `한`으로 유지되지만, 옛한글 `가ᇹ`은 `ᄀ`, `a + U+0301 + U+0308`은 `a`로 표시/복사된다.
- 입력기에서 확정된 보통 현대 한글이 깨졌다는 재현은 아니다. **조합 문자를 flatten하는 출력/복사 경로**의 결함이다.
- 방향: 일반 셀은 현재의 작은 고정 데이터로 유지하고, 여러 scalar grapheme만 희소 side table로 보존한다. shaping·복사·검색·선택 폭·원격 직렬화도 함께 연결해야 한다. 모든 셀을 String으로 바꾸면 오히려 메모리가 증가한다.
- Warp 셀은 char 또는 base+zerowidth 문자열을 렌더 경로에 전달한다. [Warp cell:168](https://github.com/warpdotdev/warp/blob/5af88f49f84e70025f9c19e13f6b9ae64b624627/crates/warp_terminal/src/model/grid/cell.rs#L168).

### 2. 조합 상태가 String 하나 — 확인된 기능 제약

- `workspace.rs:1829`는 `preedit: String`; `:7518`의 `Preedit { text, .. }`는 선택 범위를 버린다.
- `renderer_egui.rs:704`는 전체 preedit에 배경/밑줄, `:733`은 기본 terminal cursor rect를 후보창 위치로 제공한다. 조합 내부 커서/선택 범위와 행 끝 여유 폭을 반영하지 않는다.
- 방향: `Composition { text, active_range_chars, owner }` 형태로 세션/pane 소유권과 범위를 저장하고, 조합 caret 기준 후보 위치와 좁은 pane의 표시 범위를 계산한다. egui 값은 **char 인덱스**이므로 macOS UTF-16·winit UTF-8 byte offset과 혼동하면 안 된다.
- 현재 문자열은 draw(`workspace.rs:7062`) 이후 입력 처리(`:7518`)에서 갱신된다. 조합 표시가 한 pass 이전 상태일 수 있는 순서는 확인했지만 실제 입력기 화면 지연은 측정하지 않았다. 변경 시 같은 pass에서 표시 상태를 확정하고 실제 PTY 입력은 한 번만 보내는 설계가 필요하다.
- 작은 추가 비용: 현재 preedit는 크기 확인을 겸해 text를 두 번 발행한다(`renderer_egui.rs:706`, `:715`). 한 번 layout한 galley의 크기로 배경을 그리고 같은 galley를 한 번 paint하면 된다. 일반 한 음절에서 큰 성능 효과를 기대하지 않는다.
- macOS의 surrounding-text/replacement-range까지 지원하려면 winit event 모델을 바꿔야 한다(`view.rs:394` TODO). 이것은 위 범위 표시 개선과 별개의 큰 작업이다.

### 3. 전체 snapshot 변환·Vec→Arc — 확인된 반복 할당

- `alacritty_backend.rs:402`에서 scratch와 `cols*rows` Vec를 생성하고 모든 셀의 색/속성/문자를 변환한다. `:469`에서 Arc slice로 옮긴다. `Session::take_snapshot`은 그 후 dirty 범위만 붙인다.
- 300×80 snapshot의 셀 배열 자체는 384,000B(375KiB). probe에서 새 snapshot 한 번의 요청 할당 총량은775,217B,3회였다. Vec→Arc 변환을 포함한 일시적 할당량이며375KiB가 두 번 영구 남는다는 뜻은 아니다.
- 8ms pacing의125회/초를 전부 사용한다면 fixture 기준 약96.9MB/초의 요청 할당 트래픽이라는 산술 상한이 나온다. 실제 앱에서 그 빈도·크기가 항상 발생한다는 측정은 아니다.
- 단계적 개선: 메타데이터만 변경되면 cells Arc 재사용; 변경 행은 Arc row를 교체하고 나머지는 공유. 또는 full wire DTO는 그대로 두고 로컬 내부 모델만 행 단위로 유지한다.
- 각 행을 공유해도 UI의 최종 mesh 비용은 남는다. skipped generation·event coalescing·palette/scroll/resize/alt-screen 변경 때 full resync를 유지해야 한다. 현재 `runtime/event.rs:298`의 unconsumed dirty 합치기를 보존해야 한다.

### 4. 압축 행 읽기에 scratch 재사용이 없다 — 실제 할당 증가 확인

- `storage.rs:400`은 `*scratch = compressed.decode(columns)`; `compressed.rs:140`은 매번 새 셀 Vec를 만든다.
- 같은300×80에서 live 화면은3회/775,217B,400행 위 압축 화면은83회/1,351,216B를 요청 할당했다. 차이는80행 각각의 새 Row다. fixture CPU 시간은 약84µs→112µs.
- 방향: decode_into가 기존 scratch Row의 capacity를 재사용한다. 이전 행의 attrs·zerowidth·hyperlink를 완전히 리셋해야 한다. viewer/search/복사 경로에도 같은 read_line을 쓰므로 공통 지점 하나로 개선한다.
- 이는 메모리의 **할당/해제 churn** 감소다. 현재83회가 메모리 누수이거나 동일 비율로 RSS가 줄어든다는 결론은 아니다.

### 5. 한글 wide 문자마다 독립 run — 실제 shape 수 확인

- `renderer_egui.rs:863`의 wide branch는 pending run을 flush하고 char 하나에 galley 하나를 만든다. 뒤 spacer도 run을 끊는다. 같은 속성의 연속 한글을 합치지 않는다.
- D2Coding를 로드한 실제 renderer fixture의 cached300×80: ASCII는81 shapes/약0.15ms, 한글은11,852 shapes/약0.51ms. 각각80행을 paint하고 rebuild는0이다. 동일 glyph 개수 비교가 아니라 동일 grid 크기 비교다. 출력 생성의 마지막 CRLF 때문에 한글 fixture는 대부분79행이 채워졌다.
- 방향: 같은 색·attrs·셀 폭2의 연속 CJK를 묶되 spacer는 소유 글자의 일부로 처리하고, `fit_galley_to_cells`를 문자 폭에 맞게 확장한다. ASCII/CJK 혼합·문장부호·emoji·NFD·fallback 폰트가 바뀌는 경계는 보존한다.
- run 수는 줄어도 글리프 정점 수가 같은 비율로 줄지는 않는다. shape 수는 painter 호출 수이며 GPU draw-call 수가 아니다. fixture에서 한글의 총 요청 할당 bytes는 ASCII보다 작았으므로 “한글 화면은 메모리도 반드시 더 쓴다”는 결론은 하지 않는다.

### 6. 셀 배열16B→12B — 제한된 범위의25% 절감 가능

- `viewport_snapshot.rs:55`의 char4B + RGB6B + wide/spacer bool2B + attrs1B를 실제 size_of로 확인하면 alignment를 포함해16B다. `:65`의 “기존12B→12B” 주석은 현재 코드와 다르다.
- wide/spacer와 attrs를 한 u8 flags로 합친, packed/unsafe를 사용하지 않는 별도 레이아웃 probe는12B였다.
- 같은300×80의 snapshot cell payload는384,000B→288,000B,96,000B(93.75KiB)/snapshot·25% 축소 가능하다. 이것은 **셀 배열만**의 산술이며 앱 전체 메모리25% 감소가 아니다.
- bool field를 지우면 source API와 derived serde 형태가 바뀐다. 읽기 helper와 wire DTO/custom serde를 이용해 field 계약을 유지하거나 protocol version을 맞춰야 한다. 앞의 희소 grapheme 확장과 함께 설계하는 것이 좋다.

### 7. footprint 조회가 히스토리 길이에 비례 — CPU 후보

- `storage.rs:408`은 압축 bytes를 sum, `:425`는 논리 슬롯의 압축 개수를 순회한다. `AlacrittyBackend::cache_footprint`에서 active/inactive 두 grid 모두 조회한다.
- `runtime/in_process.rs:3889`의 pump가 archive_over_cap을 부르고, 그 안에서 전체 세션 footprint를 합산한다. 압박 시 추가 합산도 한다.
- probe1,000회 평균:1,000행1.21µs,5,000행6.76µs,20,000행28.32µs. 이 함수 자체의 할당은0이다.
- 방향: 저장소가 압축 heap 합계와 logical compressed count를 증분 유지한다. stale cached 슬롯의 실제 heap·rotation/truncate/inflate/resize까지 포함해야 하므로 단순 cache 하나 추가하면 예산이 틀릴 수 있다. 수정 전후 전체 슬롯 합계와 비교하는 검증이 필요하다.

### 8. 캐시된 화면도 전체 paint/tessellation — 현재는 후순위

- `renderer_egui.rs:580`의 rebuild 판단과 별개로 모든 visible row의 bg/text/underline/selection/cursor shape를 다시 발행한다. cached galley는 mesh 생성에 재사용되지만 frame mesh 제출은 필요하다.
- 최신 release 벤치에서300×80 cached0.25ms였다. 단일 pane 기준만으로 renderer 전면 교체의 필요성을 입증하지 못했다.
- 방향: 기존 `app/bench.rs:580`의 repaint causes/rows_rebuilt/rows_painted/shapes 계측을 사용해 여러 분할의 실제 frame p95·무변화 repaint 비율을 먼저 확인한다. 텍스트를 단순히 draw하지 않으면 egui의 다음 frame에서 사라진다. retained wgpu texture/mesh를 도입할 때 cursor/selection/IME/scale/atlas의 합성·무효화 계약이 필요하다.
- Deppy는 이미 wgpu이며 OrcaShell도 all-visible paint replay를 한다. 프레임워크를 GPUI로 교체하는 것을1차 개선으로 권고하지 않는다.

### 9. 경량 값 읽기에 전체 snapshot — 작고 분리 가능한 수정

- `Session::scroll_to_prompt`, `session.rs:558`는 scroll_offset만 필요하지만 full viewport_snapshot을 부른다. restore의 alt-screen 판정(`:445`)도 같은 방식이다.
- 방향: backend에 scroll_offset/is_alt_screen 같은 메타데이터 조회를 추가하고 full snapshot은 실제 화면 전달 때만 만든다. 정상 타이핑마다 호출되는 경로는 아니므로 효과를 과장하지 않는다.

## 이미 구현되어 있는 최적화

- 같은 snapshot generation에서 dirty를 재소비하지 않는 row galley cache(`renderer_egui.rs:179`, `:216`).
- 같은 배경·underline/strikeout run 병합, 선택 행의 연속 영역 병합(`:1038`, `:1133`). 옛 조사 문서의 셀당 선택 rect 문제는 현재 이미 해결되어 있다.
- 숨겨진 tab의 snapshot과 render cache 제거(`workspace.rs:4910`), warm event는 viewport를 UI cache로 수신하지 않음(`:5495`).
- runtime의8ms viewport pacing, latest-wins 이벤트와 unconsumed dirty 병합.
- cold history의 text+attrs 압축, cache budget, exited archive 및 메모리 반환 신호.
- IME cursor visibility와 조합 활성 상태 분리, 조합 중 request_focus 억제, 물리 key count 기준 Space/Comma 중복 제거, winit4478 backport.

## 실행한 검증과 수치

환경: 현재 Mac `Mac17,7`,128GiB RAM. release 벤치는 CPU의 headless egui shape 생성+테셀레이션이며 GPU present·AppKit 실제 IME를 실행하지 않았다. scratch allocator는 **누적 요청 bytes**와 호출 수를 센다(realloc 포함). RSS, peak live heap, GPU memory 수치가 아니다.

| 현재 release 렌더 벤치 | 전 행 rebuild | cache reuse |
| --- | --- | --- |
| 80×24 |0.06ms/frame|0.02ms/frame|
| 200×50 |0.29ms/frame|0.11ms/frame|
| 300×80 |0.68ms/frame|0.25ms/frame|

위 벤치는 기존 mixed ASCII/color fixture30프레임 평균. D2Coding ASCII/CJK fixture100프레임 결과와 문자열/색/폰트가 달라 절대값을 직접 비교하면 안 된다. CPU 시간은 단일 실행의 탐색 측정이며 성능 SLA/회귀 임계값은 아니다.

실행·완료한 명령:

```bash
cd /Users/jr/Desktop/projects/deppy-sijo-cloud-agent-mcp
cargo test -p terminal --release render_tessellation_bench -- --ignored --nocapture
# 1 passed; /tmp/deppy-render-research-bench-20260927.log
cargo test -p deppy-sijo --bin deppy-sijo 한글 -- --nocapture
# 13 passed; /tmp/deppy-ime-research-hangul-20260927.log
cargo test -p terminal ime -- --nocapture
# 8 passed; /tmp/deppy-ime-research-renderer-20260927.log
CARGO_TARGET_DIR=/Users/jr/Desktop/projects/deppy-sijo-cloud-agent-mcp/target \
  cargo run --release --manifest-path /tmp/deppy-terminal-research-probe-20260927/Cargo.toml
# exit0; /tmp/deppy-terminal-research-probe-20260927.log
```

Probe main/manifest는 저장소 밖 `/tmp/deppy-terminal-research-probe-20260927`에 있다. 실제 terminal crate에 path 의존하고, 제품과 같은 vendored alacritty patch와 egui0.36.1을 사용했다. 기본 셀16B/모델12B·결합 문자 유실·footprint·live/압축 snapshot·D2Coding cached renderer를 측정한다. 마지막 로그의 값이 위 본문에 반영되어 있다. 테스트 filter 간 중복이 있을 수 있어 고유 테스트 총계로 더하지 않는다.

실패/제약: 처음에 존재하지 않는 `ui/pane.rs`, `DEPPY_VENDOR.md`, Warp flat_storage 단일 파일을 찾거나 zsh glob이 매칭되지 않았다. rg로 실제 `ui/workspace.rs`, `DEPPY_BACKPORT.md`, flat_storage 디렉터리를 확인했다. web의 특정 docs.rs 버전/블로그 open은 internal error였으므로 해당 본문을 근거로 사용하지 않고 cloned 실제 Rust 소스로 확인했다. 제품 컴파일/검증 실패는 없었다. 제품 코드를 임시로 수정해 재현한 것은 아니다.

## 권고 진행 순서

1. **작은 메모리/읽기 PR:** compressed decode_into + 경량 메타데이터 조회. snapshot 수·할당 회수 확인, 데이터 보존/resize/search/복사 회귀 검증.
2. **문자 모델 PR:** compact flags12B와 희소 grapheme 보존을 함께 설계. copy/search/remote serde 호환까지 확인. 옛한글/결합 문자 유실을 RED로 먼저 고정.
3. **snapshot PR:** 변경된 행 공유와 metadata-only 재사용. coalescing/generation/resync 계약이 핵심.
4. **CJK 렌더 PR:** 같은 스타일 wide run 병합. plain/mixed Hangul·emoji·fallback/zoom 시각 검증과 현재 벤치 비교.
5. **IME 표시 PR:** composition text/range/owner, 같은 pass 표시, 행 끝·분할/배율에 맞는 caret/후보 위치. 실제 macOS 두벌식/세벌식·빠른 연타·Space/Enter/Comma·한영 전환·한자 후보·검색창/세션 전환 QA를 포함한다. 기존 winit/원장 패치는 증거 없이 제거하지 않는다.
6. footprint 증분 집계와 retained GPU path는 세션 수·frame 원인 실측에서 필요가 확인된 경우 진행한다.

예상 절감치는 셀 payload·요청 할당·shape 제출처럼 확인된 범위에만 적용된다. 지금 보고만 완료했으며 이 개선안을 제품에 구현하거나 새 릴리스로 빌드한 것은 아니다.
