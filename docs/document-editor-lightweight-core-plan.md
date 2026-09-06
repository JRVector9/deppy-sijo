# 경량 문서 편집기 핵심 구현 계획

상태: 2026-07-27 계획 확정 — **§4 아키텍처는 2026-08-21에 대체됨**

> **먼저 읽을 것**: `docs/superpowers/specs/2026-08-21-document-tab-design.md`
>
> 이 문서의 방향과 계약(§5~§8)은 그대로 유효하고 2026-08-21에 재확인했다.
> 그러나 **§4가 제안한 중앙 outer split은 채택하지 않는다.** 이 문서가 쓰인 뒤
> 저장소가 보조 pane 탭(`PaneAuxTab`, 이력·Git)을 확립했고, 문서도 그 기구를 쓴다.
> 한계 초과 시 동작·i18n·`check-boundary`·버전 고정 근거도 그 문서가 채운다.

## 1. 목적

Deppy Sijo의 기존 파일 탐색기와 네이티브 터미널을 유지하면서 가장
빠르고 가벼운 방법으로 로컬 Markdown 문서의 열기, 보기, 수정, 저장을
먼저 제공한다.

이번 경로에서는 편집기 엔진 경쟁을 먼저 해결하지 않는다. 소스 편집은
egui 0.35의 multiline `TextEdit`로 시작하고, 교체 가능한 얇은 어댑터
뒤에 둔다. 반면 Markdown Viewer는 임시 화면을 만들지 않고 이후에도
계속 사용할 수 있는 네이티브 렌더러와 페이지 디자인으로 구현한다.

핵심 원칙은 다음과 같다.

> 편집기는 단순하게 시작할 수 있지만 Viewer는 두 번 만들지 않는다.

## 2. 이번 경로에서 고정하는 결정

1. 기존 `FileTreeUi`, `WorkspaceUi`, `AgentSessionsUi`, composer, diff
   surface를 재사용한다. 동일한 기능의 패널을 새로 만들지 않는다.
2. 문서 영역은 runtime mux가 아니라 App이 소유한 중앙 outer split에
   배치한다. `RuntimeCommand`와 터미널 pane identity는 변경하지 않는다.
3. 초기 소스 편집기는 egui multiline `TextEdit`와 bounded `String`을
   사용한다.
4. `TextEdit`는 `SourceEditorAdapter` 뒤에 두어 향후 CodeMirror 6 또는
   Deppy 네이티브 커스텀 편집기로 교체할 수 있게 한다.
5. Markdown Viewer는 egui 0.35와 직접 호환되는
   `egui_commonmark 0.24`를 기반으로 한다.
6. Viewer는 Deppy가 소유하는 `MarkdownViewer` facade, page theme, 안전한
   이미지 broker, link policy, render cache로 감싼다. 앱의 다른 코드가
   `egui_commonmark` 타입에 직접 의존하지 않게 한다.
7. Viewer는 WebView를 사용하지 않는다. 따라서 문서를 열지 않았을 때뿐
   아니라 preview를 열었을 때도 별도 브라우저 프로세스와 IPC가 없다.
8. raw Markdown source만 문서의 authoritative state다. Viewer는 source를
   다시 직렬화하거나 저장하지 않는다.
9. 로컬 UTF-8 Markdown만 먼저 지원한다. 원격 파일, 범위 주석, LSP,
   multi-cursor, slash command, wikilink index는 후속 단계로 미룬다.

## 3. 이 방식이 가장 빠르고 가벼운 이유

### 3.1 편집기 기반 공사를 피한다

`TextEdit`가 이미 제공하는 네이티브 선택, clipboard, undo/redo, IME,
키보드 입력을 그대로 사용한다. Ropey/Crop, custom layout, glyph shaping,
multi-cursor, gutter를 핵심 기능보다 먼저 만들지 않는다.

### 3.2 Viewer를 WebView보다 먼저 네이티브로 완성한다

`egui_commonmark 0.24`는 현재 workspace의 egui 0.35와 버전이 맞고,
CommonMark와 tables, strikethrough, task lists, footnotes, images, headings,
links, code blocks를 렌더한다. `show_scrollable`은 긴 정적 문서에서 보이는
요소를 중심으로 렌더할 수 있다.

Viewer를 `MarkdownViewer` facade 뒤에 두므로 향후 편집기 엔진이
CodeMirror 또는 커스텀 네이티브 엔진으로 바뀌어도 Viewer, page theme,
이미지 보안, link routing, preview cache는 그대로 유지된다.

### 3.3 기존 Deppy 경계를 그대로 사용한다

파일 I/O는 기존 bounded-intent 관례를 따르는 별도 document lane에서
수행한다. render path에서 파일을 읽거나 저장하지 않고, terminal runtime에
document command를 추가하지 않는다.

## 4. 최소 아키텍처

```text
App
|-- FileTreeUi (existing)
|   `-- SidebarAction::OpenDocument(DocumentTarget)
|-- Central workspace outer split
|   |-- WorkspaceUi (existing terminal mux)
|   `-- DocumentSurface (new)
|       |-- DocumentToolbar
|       |   `-- Source | Preview | Split
|       |-- NativeSourceEditor
|       |   `-- egui TextEdit behind SourceEditorAdapter
|       `-- MarkdownViewer
|           |-- egui_commonmark
|           |-- DeppyMarkdownTheme
|           |-- WorkspaceImageBroker
|           `-- LinkPolicy / render cache
`-- DocumentIoController
    `-- LocalDocumentBackend
```

문서 기능은 새 crate로 나누지 않고 우선 app crate 안에 격리한다.

예상 파일 경계:

- `crates/app/src/ui/document.rs`: toolbar, split, source editor, document UX.
- `crates/app/src/ui/markdown_viewer.rs`: Viewer facade와 page theme.
- `crates/app/src/document_io.rs`: bounded local load/save worker와 revision.
- `crates/app/src/app.rs`: action routing과 outer split 조립만 담당.
- `crates/app/src/ui/mod.rs`: 새 UI module export.

`app.rs`가 document buffer, Markdown parsing, image decoding을 직접 수행하지
않게 한다.

## 5. 핵심 기능 범위

### 5.1 첫 사용 가능 버전에 포함

- 기존 파일 탐색기에서 `.md` 파일 열기.
- 현재 workspace 안의 UTF-8 regular file만 허용.
- source, preview, split 세 가지 표시 모드.
- source 수정, native undo/redo, selection, clipboard, Korean IME.
- `Cmd/Ctrl+S` 저장.
- dirty 표시와 닫기 전 확인.
- same-directory temporary file과 atomic replace를 이용한 저장.
- 외부 수정 revision 충돌 감지와 reload/cancel UI.
- preview tab 하나를 먼저 사용하고, 수정 시 pinned 상태로 전환.
- light/dark theme에 맞는 완성형 Markdown 페이지.
- heading, paragraph, emphasis, list, quote, horizontal rule.
- table, task list, footnote, link, fenced code block.
- bounded local PNG 이미지.
- 문서 내 heading anchor 이동.
- 읽기 실패, invalid UTF-8, oversized, conflict의 명확한 오류 화면.

### 5.2 같은 Viewer를 확장해 추가

다음 기능은 Viewer를 교체하지 않고 renderer callback 또는 facade 내부
extension으로 추가한다.

- inline/block math renderer와 generation별 SVG cache.
- `<details>/<summary>` 전용 안전한 collapsing renderer.
- 언어별 syntax highlighting cache.
- table of contents와 source/preview heading navigation.
- safe remote image opt-in 정책.
- GitHub alert block과 Deppy 전용 callout style.
- print/export HTML이 필요할 경우 동일 parse policy 재사용.

### 5.3 이번 경로에서 제외

- Monaco, CodeMirror 또는 완전한 커스텀 편집기 도입.
- line-number gutter와 minimap.
- multi-cursor와 rectangular selection.
- LSP, diagnostics, code completion.
- `[[wikilink]]` workspace index와 completion.
- slash command.
- range annotation과 agent handoff.
- arbitrary SSH/SFTP document backend.
- raw HTML 실행.
- binary/hex editor.

이 제외 항목은 폐기가 아니라 핵심 버전 뒤에 붙는 기능이다.

## 6. 두 번 만들지 않는 Markdown Viewer 계약

### 6.1 Viewer 소유권

앱은 `egui_commonmark::CommonMarkViewer`를 직접 호출하지 않는다. 다음과
같은 Deppy-owned facade만 사용한다.

```rust
struct MarkdownViewer {
    cache: MarkdownViewerCache,
    theme: DeppyMarkdownTheme,
    image_broker: WorkspaceImageBroker,
}

struct MarkdownViewRequest<'a> {
    document: DocumentId,
    generation: DocumentGeneration,
    source: &'a str,
    base_directory: &'a Path,
    mode: MarkdownViewMode,
}
```

실제 타입과 lifetime은 구현 시 조정할 수 있지만 다음 ownership은
변경하지 않는다.

- `DocumentState`가 source와 generation을 소유한다.
- `MarkdownViewer`는 파생 cache만 소유한다.
- `WorkspaceImageBroker`가 local path validation과 bytes/texture를 소유한다.
- Viewer widget은 파일 path, network client, save operation을 소유하지 않는다.

### 6.2 페이지 디자인 기준

첫 버전부터 preview를 단순 widget 나열이 아니라 읽기 좋은 문서 페이지로
보이게 한다.

- 본문 최대 폭 840-900px, 중앙 정렬.
- 좌우 최소 32px, 상하 40px 이상의 page padding.
- 본문 15-16px 상당 크기와 1.55-1.7 line height.
- H1-H4의 명확한 크기, weight, 위아래 spacing hierarchy.
- paragraph, list, quote 사이에 일관된 vertical rhythm.
- inline code와 code block의 별도 배경, border, radius, padding.
- 긴 code/table은 문서 전체가 아니라 해당 block만 수평 스크롤.
- table header 강조, row separator, dark/light 대비 보장.
- blockquote와 alert는 왼쪽 accent line과 약한 배경 사용.
- 링크 hover와 keyboard focus 상태 제공.
- 이미지는 본문 폭에 맞게 축소하고 alt text와 failure placeholder 제공.
- heading anchor를 눌렀을 때 위치가 toolbar 아래로 가려지지 않게 offset 적용.
- selectable text를 활성화해 Viewer에서도 복사가 가능하게 한다.

페이지 theme은 `DeppyMarkdownTheme` 하나에서 파생하고 각 renderer가 임의의
색과 spacing을 하드코딩하지 않는다.

### 6.3 지원 문법 기준

초기 production Viewer가 반드시 통과해야 하는 fixture:

- ATX/setext headings.
- emphasis, strong, strikethrough, inline code.
- ordered/unordered/nested lists.
- checked/unchecked task lists.
- blockquote와 GitHub alert 입력.
- GFM table과 정렬.
- fenced/indented code blocks와 언어 tag.
- relative/absolute links와 heading links.
- local relative PNG image와 실패 image.
- footnotes.
- escaped punctuation와 raw HTML 공격 입력.
- future placeholder fixture인 math와 `<details>`.

math와 `<details>`가 첫 delivery에서 fallback으로 표시되더라도 source가
사라지거나 깨진 HTML이 나오면 안 된다. 이후 같은 callback seam에서 실제
렌더링을 추가한다.

### 6.4 캐시와 성능

- cache key는 `(DocumentId, DocumentGeneration, ThemeRevision, WidthBucket)`다.
- source가 변하지 않으면 매 frame 재파싱하지 않는다.
- source generation이 바뀌면 해당 문서의 scrollable cache만 무효화한다.
- inactive document cache는 item 수와 retained bytes를 모두 제한한다.
- 이미지 decode는 UI thread 밖에서 수행하고 canonical path와 revision으로
  cache한다.
- syntax와 math cache도 source text 전체가 아니라 block hash로 식별한다.
- 1 MiB fixture에서 preview scroll frame p95 16ms 이하를 목표로 한다.

`egui_commonmark::show_scrollable`의 static-content cache 조건을 지키기 위해
source generation 변경 시에만 명시적으로 cache를 지운다. 편집 중 split
preview는 150-250ms debounce 후 새 generation을 반영하고 매 keypress마다
전체 preview를 다시 만들지 않는다.

## 7. Viewer 보안 정책

### 7.1 dependency feature 정책

`egui_commonmark`의 기본 file/http image loader를 그대로 사용하지 않는다.
초기 dependency는 원칙적으로 다음처럼 최소화한다.

```toml
egui_commonmark = { version = "0.24", default-features = false }
pulldown-cmark = { version = "0.13", default-features = false }
```

실제 lockfile 변경 전 Cargo feature tree를 확인한다. `fetch`,
`embedded_image`, 임의 `file://` loader는 활성화하지 않는다.

`better_syntax_highlighting`은 bundle/RSS 측정 뒤 같은 Viewer에 추가한다.
기본 code block rendering으로 먼저 출시해도 Viewer 구조나 page theme을
교체하지 않는다.

### 7.2 local image

- Markdown 상대 경로는 document directory 기준으로만 해석한다.
- workspace root와 image path를 canonicalize한다.
- workspace 밖 경로, special file, 허용되지 않은 symlink를 거부한다.
- encoded bytes, decoded dimensions, pixel count를 각각 제한한다.
- 첫 버전은 기존 workspace dependency와 맞는 PNG만 허용한다.
- Viewer에는 canonical path 대신 opaque `deppy-image://<id>`만 전달한다.
- ID는 document, generation, image revision에 묶고 stale request를 거부한다.
- decode 실패는 page 안 placeholder로 표시하고 render를 중단하지 않는다.

### 7.3 link와 HTML

- `http`/`https` 링크는 사용자가 클릭했을 때 기존 host action으로 연다.
- `#heading`은 Viewer 내부 navigation으로 처리한다.
- 상대 `.md` 링크는 후속 document-open action seam으로 전달한다.
- `javascript:`, `data:`, 임의 custom scheme은 거부한다.
- raw HTML은 실행하지 않고 기본적으로 text 또는 omitted placeholder로
  표시한다.
- `<details>/<summary>`만 별도 parser가 정확히 인식한 block에 한해 native
  collapsing widget으로 변환한다. script/style/event attribute는 해석하지
  않는다.

## 8. 최소 문서 상태와 I/O

```rust
struct LightweightDocumentState {
    id: DocumentId,
    path: PathBuf,
    source: String,
    generation: DocumentGeneration,
    loaded_revision: DocumentRevision,
    saved_hash: [u8; 32],
    dirty: bool,
    mode: DocumentViewMode,
    load_state: DocumentLoadState,
}

enum DocumentViewMode {
    Source,
    Preview,
    Split,
}
```

- 1 MiB는 모든 Viewer 기능과 source editing을 보장하는 acceptance tier다.
- 1 MiB는 저장 파일 한도가 아니다.
- 더 큰 파일의 source/view limit은 fixture 측정 뒤 별도로 고정한다.
- save는 loaded revision을 비교한 뒤 같은 directory의 temporary file을
  flush하고 atomic replace한다.
- 외부 변경은 자동 overwrite하지 않는다.
- document source와 path를 log에 기록하지 않는다.

## 9. 구현 순서

### L0. 구조 연결, 0.5-1일

- `SidebarAction::OpenDocument` 추가.
- App-owned document surface와 outer split 추가.
- terminal rectangle resize가 기존 runtime path만 타는지 검증.
- `DocumentState`, adapter trait, bounded I/O request/result 정의.

완료 조건: 빈 document surface를 열고 닫아도 terminal split/focus가 그대로
동작한다.

### L1. 로컬 source 편집, 1-2일

- local UTF-8 load.
- `TextEdit` source mode.
- dirty/save/close confirmation.
- revision conflict와 atomic save.
- Korean IME, clipboard, undo/redo 확인.

완료 조건: `.md` 파일을 열어 수정, 저장, 재오픈하고 외부 충돌을 확인할 수
있다.

### L2. production Markdown Viewer, 2-3일

- `MarkdownViewer` facade와 cache.
- source/preview/split toolbar.
- page width, typography, spacing, code, table, quote, link theme.
- scrollable cache와 split-preview debounce.
- safe local PNG image broker.
- heading navigation과 Viewer text selection.
- representative visual fixture와 screenshot checklist.

완료 조건: README 수준의 문서가 light/dark theme에서 제품 화면으로 사용할
수 있는 품질로 보이며 임시 preview라는 이유로 교체가 필요하지 않다.

### L3. 안정성 마감, 1-2일

- invalid UTF-8, oversized, special file, symlink, image traversal tests.
- stale revision와 atomic save tests.
- no-op Viewer가 source를 바꾸지 않는 regression.
- checkbox range-local mutation test를 추가할지 결정.
- cache bound와 open/close 반복 retained-resource 측정.

완료 조건: 핵심 기능과 보안 실패가 deterministic test로 고정되고 문서
surface를 닫으면 document/image cache가 회수된다.

예상 총 기간은 4.5-8일이다. 첫 사용 가능 source+preview는 L2 종료 시점인
3.5-6일을 목표로 한다.

## 10. 검증 계획

### 10.1 자동 검증

- document model dirty/generation/hash tests.
- bounded I/O admission과 stale completion tests.
- load size/UTF-8/special-file/path tests.
- atomic save와 stale revision tests.
- Markdown fixture render smoke.
- source generation별 Viewer cache invalidation test.
- image root traversal, symlink, byte/dimension limit tests.
- source/preview/split mode state tests.
- file-tree open action integration test.
- terminal runtime protocol에 document variant가 추가되지 않았다는 구조 검사.

### 10.2 수동 target-hardware 검증

- Korean IME 조합 중 source/preview focus 이동.
- 1 MiB source typing과 preview scroll.
- source/preview split resize.
- light/dark theme visual review.
- heading, nested list, table, code, quote, link, image fixture review.
- preview open 전후 app RSS/CPU/frame p95.
- document 20회 open/close 후 retained RSS slope.

### 10.3 품질 실패 기준

다음 중 하나라도 발생하면 Viewer를 출시하지 않는다.

- 기본 README가 잘린 글자, 겹친 block, 읽을 수 없는 contrast를 보인다.
- table/code가 전체 page width를 깨뜨린다.
- preview 전환만으로 source text가 변경된다.
- raw HTML 또는 image path로 workspace 밖 resource를 읽을 수 있다.
- source가 바뀌지 않아도 매 frame 전체 parse 또는 image decode가 발생한다.
- preview를 닫은 뒤 cache가 문서 수에 비례해 계속 증가한다.

## 11. 이후 확장 방향

경량 핵심 버전이 나온 뒤 실제 불편을 기준으로 source editor를 결정한다.

1. `TextEdit`가 충분하면 native adapter를 유지하고 line numbers/search 등
   필요한 기능만 추가한다.
2. VS Code형 source UX가 필요하면 `SourceEditorAdapter` 구현만 CodeMirror
   6로 교체한다. Native `MarkdownViewer`는 유지한다.
3. 장기적으로 Deppy custom editor가 필요하면 buffer/layout/IME spike를
   별도로 진행한다. Viewer와 document I/O는 유지한다.
4. math, `<details>`, syntax highlighting, TOC는 기존 `MarkdownViewer`
   callback과 cache를 확장한다.
5. wikilink, annotations, agent handoff는 source byte range를 기준으로
   `DocumentController`에 추가한다.
6. remote backend는 같은 `DocumentState`와 save UI 뒤에 추가한다.

따라서 이번 구현은 버리는 MVP가 아니다. 버릴 수 있는 부분은 얇은
`TextEdit` adapter뿐이고, document controller, I/O, Viewer, theme, image
broker, cache와 테스트 fixture는 최종 제품에 남는다.

## 12. 참고 자료

- `egui_commonmark 0.24`: https://docs.rs/egui_commonmark/latest/egui_commonmark/
- `egui_commonmark` source and feature contract:
  https://docs.rs/crate/egui_commonmark/latest/source/Cargo.toml
- `pulldown-cmark OffsetIter`:
  https://docs.rs/pulldown-cmark/latest/pulldown_cmark/struct.OffsetIter.html
- 장기 전체 계획: `docs/document-editor-development-plan.md`
