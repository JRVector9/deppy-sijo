# Document editor development plan

Status: reviewed against `main` on 2026-07-27; final pre-spike architecture and
editor-engine shortlist defined, implementation not started

Immediate delivery track: implement
`docs/document-editor-lightweight-core-plan.md` first. That track uses native
`TextEdit` behind an adapter to deliver local source editing quickly, while
building the native Markdown Viewer, page theme, image broker, cache, and I/O as
production components that will not be replaced if the source editor changes.
The broader engine bakeoff in this document resumes after the lightweight core
is usable and measured.

## 1. Goal

Add a lightweight source-first document editor to the existing Deppy Sijo
shell. Deppy already has the file tree, native terminal workspace, agent panel,
composer, and bounded diff review surface. Selecting a supported document in
the existing file tree opens a document surface beside the existing terminal;
the project must not duplicate those existing shells or replace or weaken the
terminal mux.

The editor should eventually support:

- Read, create, edit, save, rename, and close text documents.
- Markdown source editing and preview.
- `[[document links]]` and completion.
- Slash commands.
- Collapsible `<details>` blocks.
- Safe local image loading.
- Code syntax highlighting.
- Checklists and tables.
- Inline and block math.
- Markdown text-range annotations.
- Sending an explicit annotation or selection to an agent.
- Table of contents and in-document search.
- Source-preserving Markdown round trips.
- A consistent user-facing save flow for local and remote documents.

## 2. Main-worktree findings

### 2.1 The current pane model is terminal-only

`WorkspaceUi` owns mux snapshots and terminal session views. Splitting a pane
sends `RuntimeCommand::SplitPane` with a runtime pane ID. The runtime command
protocol has no generic `Terminal | Document` pane representation.

Relevant code:

- `crates/app/src/ui/workspace.rs:587`
- `crates/app/src/ui/workspace.rs:3894`
- `crates/runtime/src/command.rs:753`
- `crates/runtime/src/command.rs:802`

Decision: do not insert a document leaf into the runtime mux for the MVP.
Instead, let the App own an outer split containing the existing terminal
workspace and a document surface. A generic mixed-pane tree may be considered
later if users need arbitrary terminal/document nesting.

### 2.2 File-tree I/O has no document-content lane

The file tree already follows a bounded-intent architecture, but its actions
and I/O requests cover selection, directory listing, mutation, OS path opening,
and session operations. It does not expose document load/save operations.

Relevant code:

- `crates/app/src/ui/file_tree.rs:112`
- `crates/app/src/ui/file_tree.rs:282`
- `crates/app/src/ui/file_tree.rs:342`
- `crates/app/src/app.rs:16574`

Decision: add `SidebarAction::OpenDocument` and a separate bounded
`DocumentIoIntent`/result lane. Do not read or write document contents from a
render function and do not overload file-tree mutation requests with editor
state.

### 2.3 No editor engine is selected yet

The workspace uses egui/eframe 0.35. The current published
`egui_code_editor` line targets egui 0.34, so directly embedding its widget can
introduce incompatible `egui::Ui` and response types. Monaco provides the most
complete VS Code-like behavior out of the box, but it is not the only WebView
editor and its breadth can be unnecessary for a Markdown-first product.

Decision: do not add the incompatible `egui_code_editor` dependency and do not
preselect Monaco. Milestone 0 will compare egui 0.35 multiline `TextEdit` as the
native baseline, CodeMirror 6 as the preferred modular WebView candidate, and
Monaco as the VS Code-quality control. Scintilla and a custom Ropey/Crop-based
editor remain reserve paths, not active prototypes, because both add a much
larger native-window or editor-engine integration commitment.

### 2.4 Remote runtime is not a remote file system

`crates/runtime/src/remote.rs` transports Deppy runtime commands and terminal
events. It does not provide SSH/SFTP file read, stat, write, or rename APIs.

Decision: make the document storage interface backend-neutral but implement
only the local backend in the MVP. Implement remote storage as an independent
milestone after choosing one of these contracts:

1. Add bounded file RPCs to the paired Deppy remote runtime.
2. Add an SFTP client, including host-key validation, authentication, and
   remote atomic-save semantics.

The first option is preferred for Deppy-managed remote workspaces. The second
is required only for arbitrary SSH hosts.

## 3. Architecture

```text
App
|-- FileTreeUi (existing)
|   `-- SidebarAction::OpenDocument(DocumentTarget)
|-- Central workspace surface
|   |-- WorkspaceUi (existing native terminal mux, unchanged)
|   `-- DocumentSurface (new, lazy)
|       |-- DocumentTabs
|       |-- SourceEditorAdapter
|       |-- MarkdownAuthoring / Preview
|       `-- Search / TOC / annotations
|-- AgentSessionsUi / ComposerUi (existing explicit agent destinations)
|-- DiffPanelUi (existing bounded review pattern)
`-- DocumentIoController
    |-- LocalDocumentBackend
    `-- RemoteDocumentBackend (future)
```

The central surface should render as:

```text
+--------------------------+--------------------------+
| existing terminal mux    | document editor/preview  |
| runtime-owned panes      | App-owned surface        |
+--------------------------+--------------------------+
```

The outer split must have an independent persisted ratio and must not send
runtime resize/split commands except for the terminal rectangle's normal size
update. Opening a document must not create a second file explorer, terminal,
agent panel, or global navigation system. Agent handoff and change review extend
the existing `AgentSessionsUi`/composer and bounded diff-review boundaries.

### 3.1 Warp strengths to adopt

Use Warp as the reference for the native terminal and bounded editor-core
properties:

- Keep terminal input, output, pane state, and rendering in the native Rust
  application instead of moving the terminal into a browser runtime.
- Separate content, selection, render, and persistence models so a view does not
  become the owner of terminal or document state.
- Represent edits as deltas with stable anchors and bounded action-based undo,
  rather than copying a complete document for each user action.
- Seek by summarized position and create paint objects only for content in the
  active viewport. Incrementally lay out changed content instead of rebuilding
  an entire long document after each edit.
- Parse and highlight incrementally or off-thread, cache by content version and
  visible range, and degrade expensive language features above an explicit
  bound while keeping plain editing available.
- Share one buffer for the same file across tabs and panes so views cannot drift
  into conflicting in-memory copies.

These are architecture properties, not a requirement to port Warp's custom
`SumTree`, CRDT, UI framework, or editor during the MVP.

### 3.2 Orca strengths to adopt

Use Orca as the reference for familiar editing and agent-workspace workflow,
not as an application-shell template:

- Offer a mature source-editor experience: line numbers, familiar selection and
  shortcuts, undo/redo, multi-selection where supported, find/replace, syntax
  highlighting, bracket matching, large-text behavior, and explicit source
  mode. Monaco is the quality benchmark, not the predetermined dependency.
- Connect file opening, quick open, worktrees, diffs, comments, and explicit
  send-to-agent actions without making document contents implicitly available
  to an agent.
- Preserve the same user-facing editor and save flow for local and SSH-backed
  workspaces while isolating backend-specific transport and conflict errors.
- Apply feature-specific bounds and fallbacks instead of one global failure
  mode: rich rendering, syntax analysis, search, paste, preview assets, and raw
  source editing may have different safe limits.
- Lazily mount expensive editor/browser surfaces, suspend or dispose inactive
  resources, bound hidden-output queues, and expose enough process/resource
  diagnostics to attribute memory growth to a document, terminal, or child
  process.
- Keep editor models shared across views so opening the same path in multiple
  tabs remains synchronized with filesystem and remote updates.

These are product and lifecycle properties, not a requirement to adopt
Electron, React, xterm.js, or Orca's complete application shell.

### 3.3 Combined Deppy direction

The intended synthesis is a Warp-like native terminal core with an Orca-like
document and agent workflow:

```text
App-owned outer split
|-- NativeTerminalSurface
|   `-- existing WorkspaceUi/runtime mux, unchanged
`-- DocumentSurface
    |-- DocumentController (Rust persistence/security/revisions)
    |-- SharedDocumentRegistry (one model per stable document ID)
    |-- SourceEditorAdapter
    |   |-- Native TextEdit candidate
    |   |-- Embedded CodeMirror 6 candidate
    |   `-- Embedded Monaco quality-control candidate
    `-- Preview / search / TOC / annotations / agent action
```

Combined rules:

1. The terminal must start and remain usable without creating a webview.
2. The selected editor backend owns interactive cursor, selection, composition,
   and undo state; Rust does not mirror the full source on every keystroke.
3. Rust always owns path authorization, file revisions, conflict detection,
   atomic persistence, remote transport, and sensitive-content policy.
4. Source Markdown remains authoritative. Preview and direct-rendered controls
   are derived views that emit range-local source edits; they do not normalize
   and reserialize the whole document.
5. Expensive editor resources are created lazily and have measurable idle,
   active, hidden, and disposed lifecycle states.
6. Local and remote backends reuse the same document state machine and UI, with
   bounded backend-specific requests and explicit stale-revision errors.
7. A custom native rope/tree editor remains a post-spike option only if native
   `TextEdit` misses the accepted workload and both WebView candidates miss the
   integration, fidelity, or resource budgets.

### 3.4 Editor-engine shortlist

| Candidate | Strength for Deppy | Main cost | Milestone 0 disposition |
| --- | --- | --- | --- |
| egui `TextEdit` | Smallest integration, native focus/theme/accessibility path, no WebView | Not a complete code editor; line layout, multi-cursor, gutters, viewport work, and large-file behavior may require substantial custom code | Implement as the native baseline |
| CodeMirror 6 | Modular source editor, transaction/range model, viewport DOM, incremental Lezer parser, decorations/widgets, completion, and split/shared state fit Markdown authoring | Requires a sandboxed child WebView, typed IPC, JS asset build, and platform IME/focus validation | Preferred production candidate if it passes the spike |
| Monaco | Strongest ready-made VS Code-like source UX and mature model/provider lifecycle | Broader bundle/runtime surface and less Markdown-specific customization leverage than Deppy needs | Implement only as the quality and resource control |
| Scintilla | Mature native source-control component with multi-view documents, folding, completion, markers, and IME support | C++/platform child-control integration does not naturally compose with egui/wgpu and creates three platform seams | Paper-evaluate; prototype only if WebView is prohibited |
| Ropey/Crop plus custom view | Maximum ownership of buffer, deltas, viewport, and resource policy | Rope is only the buffer; selection, IME, shaping, bidi, undo, accessibility, gutters, rendering, and widgets still have to be built | Post-Milestone-0 escalation only |
| Zed/GPUI, Floem/Lapce, Makepad, or Warp editor code | Useful architectural references for native GPU editors | They are application/UI-stack commitments rather than stable egui editor widgets; Zed is primarily GPL and GPUI is pre-1.0 | Reference only |
| ProseMirror/TipTap | Strong direct rich-text editing model | Markdown import/export tends to normalize source and conflicts with byte-preserving round trips | Separate future experiment only if WYSIWYM is insufficient |

### 3.5 Provisional production direction

The preferred direction before measurements is **CodeMirror 6 inside one
lazy, sandboxed editor WebView per app window**, with one editor state per open
document and only one visible editor view. This is a hypothesis, not a frozen
dependency choice. CodeMirror's transactions, document offsets, viewport
rendering, completion sources, decorations, and widgets map directly to
wikilinks, slash commands, checklists, headings, annotations, and source-local
Markdown edits without requiring the full VS Code editor surface.

Monaco stays in Milestone 0 because it defines the UX quality bar and can still
win if its measured cost is acceptable and reproducing the required editing
behavior in CodeMirror is materially more expensive. Native `TextEdit` can win
if it meets the accepted workload and editor-feature bar with substantially
less lifecycle complexity. No production code should support all three engines;
the losing prototypes are removed after the decision record.

For a WebView outcome:

- Use a child WebView host such as `wry` only behind `SourceEditorAdapter`; do
  not leak WebView types into document persistence or terminal code.
- Package editor HTML, CSS, JavaScript, fonts, and grammars as local immutable
  assets. Deny arbitrary navigation, new windows, downloads, direct network,
  Node APIs, and direct filesystem access with both host handlers and CSP.
- Use a typed, versioned, bounded IPC protocol. Coalesce edit metadata and never
  send a full document on every keypress. Request a bounded source snapshot only
  at explicit save, preview synchronization, recovery, or conflict operations.
- Create the WebView only when the first supported document opens. Hide or
  suspend it when the document surface is inactive, dispose inactive document
  states under a measured bound, and destroy it when the last document closes.
- Keep terminal startup, input, rendering, splitting, and shutdown completely
  functional when the WebView cannot be created or crashes.

### 3.6 Image #1 decision mapping

| Criterion | Final Deppy direction |
| --- | --- |
| Base RAM | Preserve the existing Rust/egui/wgpu shell; add no Electron or React shell and lazily create at most one editor WebView |
| Terminal CPU | Preserve the existing native terminal data structures and GPU renderer; document work never enters the runtime mux |
| Code-editing UX | Meet the Monaco/VS Code behavior bar using the winning adapter, provisionally CodeMirror 6 |
| Terminal UX | Keep the existing Deppy terminal and selectively adopt Warp's bounded delta/viewport principles rather than replacing it with xterm.js |
| Multi-agent | Extend the existing agent panel, composer, worktree, and diff flows with explicit selection/annotation handoff |
| Remote/server | Reuse the same document state machine and UI; add a bounded remote document backend without changing terminal transport semantics |
| Implementation complexity | Reuse all existing Deppy surfaces, customize a modular editor, and avoid building a native editor core until evidence requires it |

### 3.7 Frozen Milestone 0 budgets

These budgets are fixed before prototype results are collected. A candidate may
degrade an expensive feature at the 10 MiB stress size, but it may not hide a
miss at the 1 MiB acceptance size.

| Measurement | Pass budget on target release hardware |
| --- | --- |
| Terminal-only startup | No WebView process or editor heap before the first document opens; existing terminal startup path remains unchanged |
| Terminal regression | With an editor open but idle/hidden, active-terminal frame p95 remains at or below 16 ms and no more than 10% above its same-build baseline |
| First editor open | Cold first interactive paint at or below 1,000 ms; same-process reopen at or below 300 ms |
| 1 MiB document load | Editable and interactive at or below 500 ms |
| Typing latency | Key-to-painted-glyph p95 at or below 50 ms and p99 at or below 100 ms during ordinary Markdown editing |
| Scroll | Continuous visible scroll paint/RAF p95 at or below 20 ms on the 1 MiB fixture |
| Paste/undo | 100 KiB paste and its undo each complete at or below 500 ms without blocking terminal input |
| Idle CPU | Editor-visible idle total app/editor CPU at or below 2%; hidden editor adds no periodic egui repaint and no more than 0.5 percentage point over terminal-only idle |
| Memory | One active 1 MiB document adds at most 150 MiB total app plus editor-process RSS; 20 open/close cycles show no positive retained-RSS slope after warm-up |
| IPC | Ordinary typing emits at most one coalesced edit batch per rendered frame; no full-source message occurs before explicit save/preview/recovery/conflict synchronization |
| 10 MiB stress | No crash, data loss, input deadlock, terminal regression, or UI stall above 2 seconds; feature degradation is explicit and source/view fallback remains available |

## 4. Core data model

```rust
enum DocumentLocation {
    Local(PathBuf),
    Remote(RemoteDocumentLocation), // reserved until remote milestone
}

struct DocumentRevision {
    modified: SystemTime,
    len: u64,
    content_hash: Option<[u8; 32]>,
}

enum DocumentContentState {
    Native {
        source: String,
    },
    ExternalEditor {
        model: EditorModelId,
        generation: DocumentGeneration,
        byte_len: usize,
        current_source_hash: [u8; 32],
    },
}

struct DocumentState {
    location: DocumentLocation,
    content: DocumentContentState,
    saved_source_hash: [u8; 32],
    loaded_revision: DocumentRevision,
    dirty: bool,
    read_only: bool,
    load_state: DocumentLoadState,
    cursor: Option<TextCursor>,
    scroll_offset: f32,
}
```

The native spike should use `String`, not `Rope`. Native `TextEdit` already
edits a `String`, and the current MVP proposal rejects oversized files before
allocation. Do not add a rope only to make the spike more complex. If a WebView
editor is selected, its native model (`EditorState` for CodeMirror or
`ITextModel` for Monaco) should own interactive edit and undo state while Rust
owns file identity, revision checks, permissions, and storage policy. The
`ExternalEditor` state stores only the stable model identity and bounded
metadata. Exchange range edits and bounded snapshots at explicit load, save,
preview synchronization, recovery, and conflict points instead of mirroring the
full document over IPC on every keystroke.

## 5. I/O contract

```rust
enum DocumentIoRequest {
    Load {
        operation: DocumentOperationId,
        target: DocumentTarget,
    },
    Save {
        operation: DocumentOperationId,
        target: DocumentTarget,
        expected_revision: DocumentRevision,
        contents: Arc<str>,
    },
    Create {
        operation: DocumentOperationId,
        target: DocumentTarget,
        contents: Arc<str>,
    },
}
```

Requirements:

- Keep request and result channels bounded.
- Reject NUL paths, oversized paths, symlinks where policy requires, special
  files, invalid UTF-8, and files beyond the configured absolute load limit.
- Treat 1 MiB as the guaranteed **full-authoring acceptance tier**, not as a
  save-file-size cap. Freeze separate full-authoring, source-edit, read-only
  view, search, preview, syntax, image, and absolute-load limits from Milestone
  0 results. A document accepted into an editable tier can be saved at the same
  bounded size; saving must not impose a surprising smaller limit.
- Above a feature's measured limit, disable only that feature and show the
  active tier. Do not reject plain source viewing merely because rich preview,
  highlighting, math, or workspace search reached a lower bound.
- Load with an opened-handle identity/length recheck where applicable.
- Save to a same-directory temporary file, preserve required permissions, and
  atomically rename when the platform supports it.
- Compare the expected revision before replacement. Never silently overwrite
  an externally modified document.
- Return static/sanitized diagnostics; never log document contents.
- Treat `.env`, credential, key, and token-like files as sensitive for agent
  actions even though they remain editable.

## 6. Markdown round-trip rule

The raw Markdown text is the only authoritative document representation. It
may reside in the native `String` or the selected external editor model.
Markdown parsing is used for preview, navigation, indexes, and source ranges
only.

Do not serialize a parsed Markdown AST back into the source document. That
would normalize whitespace, list markers, code fences, HTML, and other author
formatting.

UI-originated edits such as toggling a checklist item must replace only the
smallest known byte range in the source. Unsupported or ambiguous transformations
must fall back to source editing rather than rewriting a whole block.

## 7. Feature design

### 7.1 File-tree opening

- Selecting a supported text file opens it in a preview-style document tab.
- Editing pins the tab.
- Opening another file may replace an unmodified preview tab.
- Double-click can pin immediately.
- The existing OS `OpenPath` action remains available as a separate command.
- Binary files and files above the absolute load limit show a non-editable
  explanation instead of being loaded into the selected source editor.

### 7.2 Markdown parsing and preview

- Use a parser that exposes source byte ranges.
- Render preview directly in egui for the native candidate. A CodeMirror,
  Monaco, or future rich-editor candidate may share the constrained editor
  WebView, but must not gain direct filesystem or network access.
- Enable tables and task-list events explicitly.
- Render fenced code through a bounded syntax-highlighting cache.
- Render `<details>` as an egui collapsing section without executing raw HTML.
- Cache parsed output by source generation and never parse unchanged text every
  frame.

The exact parser/renderer dependency must pass an egui 0.35 compatibility and
retained-memory spike before selection. Parser crates without egui types are
preferred over a renderer that pins a conflicting egui version.

#### 7.2.1 Source-preserving Markdown authoring mode

The user-facing target is WYSIWYM rather than a separate rich-text document
model: the user edits the Markdown source while the editor styles or augments
recognized ranges. Raw source remains immediately available and no feature may
require a whole-document Markdown reserialization.

- Style heading lines by level while preserving the exact `#` sequence and
  spacing; reveal punctuation unconditionally when the caret enters the range.
- Render task markers as clickable checkboxes that replace only the marker
  byte range (`[ ]`/`[x]`/`[X]`).
- Provide list indentation, continuation, table-row, link, image, code-fence,
  math, and `<details>` commands as explicit range transactions.
- Use inline or block widgets for bounded image, math, code, and link previews
  only when the underlying source range remains addressable and recoverable.
- Keep syntax visible for malformed, unsupported, ambiguous, or currently
  edited constructs rather than guessing and normalizing the source.
- Offer source-only, preview-only, and split source/preview modes. Authoring
  decorations are an enhancement to source mode, not a second document model.

CodeMirror 6 is provisionally favored for this mode because its official
decoration model supports marks, widgets, range replacements, line styling,
atomic ranges, and viewport-derived decorations. The Milestone 0 fidelity spike
must prove Korean IME, cursor movement, selection, copy/paste, undo, and source
offset behavior across those widgets before this becomes a production choice.

### 7.3 Local images

Add a bounded Rust-owned `WorkspaceImageBroker`; never let an editor or preview
resolve arbitrary local paths itself. A native preview receives a validated
texture, while a WebView receives only a short-lived opaque asset ID served by
a custom local protocol or bounded byte response. Neither path receives an
arbitrary `file://` URL.

- Resolve relative paths against the document directory.
- Canonicalize the document root and image path.
- Reject paths outside the permitted workspace root unless the user explicitly
  approves them.
- Reject symlinks or revalidate the opened handle according to the existing
  filesystem policy.
- Allow only supported image formats and bounded dimensions/encoded bytes.
- Decode off the UI thread.
- Cache textures by canonical path plus revision.
- Bind WebView asset IDs to document ID, canonical revision, and an expiry or
  lifecycle generation so one document cannot request another document's
  images.
- Reject WebView requests that are unknown, stale, oversized, or outside the
  current document's permitted workspace root.

### 7.4 Wikilinks and slash commands

Create a bounded `WorkspaceDocumentIndex` on a maintenance worker. It should
index normalized relative paths, Markdown titles, and headings. File watching
may mark entries dirty, but a full workspace scan must not run every frame.

`[[` opens a completion popup at the editor cursor. `/` opens a command popup
only at a valid source position. Both features insert or replace an explicit
source range and leave the rest of the document unchanged.

### 7.5 Search and table of contents

- Document search operates on the source string and stores byte ranges.
- The table of contents is derived from parsed heading ranges.
- Selecting a result updates the source cursor and scroll target.
- Preview and source navigation share stable source offsets where available.

### 7.6 Annotations

Annotations must not be embedded into Markdown unless the user explicitly
requests a textual comment. Store them in an app-owned sidecar database keyed
by document location.

Each annotation should retain:

- Document location.
- Start and end byte offsets.
- Hash of selected text.
- Short prefix and suffix context.
- Comment text and timestamps.
- Last known document revision.

After an edit, re-anchor by exact selected text and context. If the match is
ambiguous, mark the annotation unresolved rather than attaching it silently to
the wrong range.

The first annotation UI may use a side panel or gutter marker. True inline
highlight overlays should be deferred until the source editor exposes reliable
glyph and scroll geometry.

### 7.7 Sending annotations to an agent

- Send only the explicitly selected range, annotation, document-relative path,
  and necessary surrounding lines.
- Require an explicit action; never stream the active document automatically.
- Warn before sending content from `.env`, secret, credential, key, or token
  files.
- Route the payload through the existing structured agent action and audit
  boundary, not through shell-string interpolation.
- Keep document contents out of diagnostic logs and persisted command history
  unless the product contract explicitly requires them.

## 8. Delivery milestones

### Milestone 0: architecture and editor-engine spike, 3-5 days

Run exactly three decision spikes:

1. **Existing-shell integration boundary.** Prove the App-owned outer split
   inside the current central workspace without changing runtime pane identity
   or commands and without creating a second file tree, terminal, agent panel,
   or diff shell. Exercise resize, DPI, native terminal/editor focus transfer,
   Korean IME, shortcuts, clipboard, drag/drop, hide/show, and close/reopen with
   both a native widget and one sandboxed child-WebView placeholder.
2. **Source-editor engine bakeoff.** Run the same minimal feature contract
   through egui 0.35 multiline `TextEdit`, CodeMirror 6, and Monaco. The contract
   includes open/edit/save snapshot, line numbers where supported, selection,
   undo/redo, find/replace, syntax styling, multi-selection capability, and
   theme/focus integration. Use 1 MiB as the acceptance workload and 10 MiB as
   an architectural stress workload. Record bundle delta, WebView/process count,
   load time, key-to-paint latency, scroll/frame p95, paste and undo behavior,
   idle/loaded/hidden/disposed RSS delta, CPU while idle and typing, startup
   delta, IPC messages/bytes, and retained document bytes on target hardware.
   Freeze pass budgets before collecting results; do not relax them after seeing
   an implementation.
3. **Markdown authoring and fidelity.** Implement the same small WYSIWYM feature
   set in the leading native and WebView candidates: styled headings, clickable
   checklists, one table-row action, wikilink completion, one slash command,
   fenced-code styling, math/image placeholders, link interaction, and
   `<details>` folding. Require byte-identical no-op round trips and range-local
   diffs for supported edits. Record unsupported constructs, Korean IME and
   selection behavior around widgets, source-range mapping quality, and whether
   a separate ProseMirror/TipTap experiment is truly necessary.

After the three spikes, select exactly one production source engine. Prefer
CodeMirror 6 if it meets the VS Code-like behavior bar and frozen resource
budgets; select Monaco only if its measured ready-made UX advantage materially
outweighs its lifecycle/package cost; select native `TextEdit` only if it meets
the required editor and Markdown-authoring behavior without recreating a code
editor. If none pass, stop before Milestone 1 and write a separate Scintilla or
custom native editor proposal. Then select the parser/highlighter and freeze
IDs, byte bounds, error codes, IPC contracts, and document-state ownership
around the chosen direction.

The decision record must score each direction against the combined reference
profile above: native-terminal independence, editor familiarity, target-hardware
resource budgets, Markdown source fidelity, lazy lifecycle behavior, shared
buffer ownership, local/remote save-flow compatibility, implementation effort,
license, platform packaging, accessibility, and Korean IME behavior.

Exit condition: a reviewed decision record containing the common benchmark
table, captured target-hardware results, rejected alternatives and reasons, and
a compile-only prototype with no runtime protocol change.

### Milestone 1: local source editor, 5-8 days

- File-tree open action.
- Bounded asynchronous load.
- Winning source editor, basic editing commands, dirty state, save, save as,
  create, and close confirmation.
- Atomic replacement and external-change conflict UI.
- Basic document tab and persisted outer split ratio.
- Lazy editor lifecycle, typed bounded IPC if selected, and one shared document
  state per canonical file identity.

Exit condition: `.md`, `.env`, `.json`, and common UTF-8 text files can be
created, edited, saved, reopened, and conflict-tested without blocking render.

### Milestone 2: Markdown authoring and preview, 7-10 days

- Source-only, preview-only, and split modes.
- Source-preserving styled headings, list helpers, clickable task lists, table
  commands, links, images, code blocks, math, and `<details>`.
- Bounded code syntax highlighting and source/preview navigation.
- Safe local images.
- Table of contents and in-document search.

Exit condition: no-op view changes are byte-identical, every direct authoring
action produces the smallest expected source diff, and all parser/decoder work
is bounded, incremental, viewport-limited, or off-thread.

### Milestone 3: knowledge and agent features, 6-9 days

- Workspace document index.
- Wikilinks and completion.
- Slash commands.
- Range annotations and re-anchoring.
- Explicit selection/annotation-to-agent action through the existing agent
  panel/composer boundary.
- Agent-proposed changes reviewed through the existing bounded diff-review
  pattern before document replacement.

Exit condition: links and annotations survive ordinary edits, and ambiguous
annotation relocation fails visibly instead of silently moving.

### Milestone 4: remote documents, 1-3 weeks

- Choose Deppy remote file RPC or arbitrary-host SFTP.
- Implement bounded read/stat/write/rename.
- Add reconnect, stale revision, partial failure, and atomic-save behavior.
- Reuse the same `DocumentState` and save UI as the local backend.

Exit condition: equivalent local/remote user flow with backend-specific errors,
conflict tests, reconnect behavior, and no separate remote editor UI.

## 9. Estimate

- First usable local editor MVP: 2-3 weeks.
- Complete local feature set listed above: 5-7 weeks.
- Local plus production remote storage: 7-10 weeks.
- Arbitrary-host SFTP, rich inline annotation painting, or a custom large-file
  editor may extend the schedule beyond this range.

These estimates assume one engineer working primarily on this feature and the
source-first outcome. If Milestone 0 selects direct-rendered rich editing, update
the architecture and estimate before Milestone 1 rather than treating it as a
free addition.

## 10. Reference projects

- `Warp`: https://github.com/warpdotdev/warp
  Use as the reference for a native Rust terminal core, summarized/delta-based
  text models, viewport rendering, incremental syntax work, and shared buffers.
  Do not treat its custom UI/editor stack as an MVP-sized dependency.
- `Orca`: https://github.com/stablyai/orca
  Use as the reference for Monaco-quality source editing, terminal/editor tabs,
  multi-agent worktrees, local/SSH parity, lazy expensive-surface lifecycle, and
  feature-specific resource limits. Do not adopt its full Electron shell.
- `CodeMirror 6`: https://codemirror.net/docs/guide/
  Preferred modular WebView candidate. Its transaction model, viewport-limited
  drawing, shared state, completion, and decoration/widget APIs align with
  source-preserving Markdown authoring and Deppy-specific extensions.
- `Lezer`: https://lezer.codemirror.net/docs/guide/
  CodeMirror's compact incremental parser reference. Validate Markdown dialect,
  mixed fenced languages, math, wikilinks, and source offsets in the spike.
- `Monaco Editor`: https://github.com/microsoft/monaco-editor
  Keep as the VS Code-like UX and resource control. Monaco models and providers
  are useful references even if CodeMirror wins; VS Code extensions do not run
  in Monaco automatically.
- `wry`: https://github.com/tauri-apps/wry
  Candidate child-WebView host with IPC and custom-protocol support. Keep it
  behind an adapter and validate winit/egui bounds, IME, focus, packaging, and
  lifecycle before selection.
- `Scintilla`: https://www.scintilla.org/ScintillaDoc.html
  Native reserve candidate with mature document/view, folding, markers,
  completion, and IME behavior. Do not prototype unless the WebView path is
  rejected because platform control integration is a separate architecture.
- `Zed/GPUI`: https://github.com/zed-industries/zed
  Use only as a native editor architecture reference. GPUI is pre-1.0, the Zed
  application is primarily GPL, and neither is an egui editor widget.
- `md-echo`: https://github.com/fibnas/md-echo
  Use as a reference for a minimal egui document lifecycle and dual-pane UI.
- `egui_code_editor`: https://github.com/p4ymak/egui_code_editor
  Use as a reference for editor layout and highlighting; do not directly add an
  incompatible egui version.
- `Ferrite`: https://github.com/OlaProeis/Ferrite
  Use as a reference for buffer, save-conflict, and mature editor architecture;
  do not embed the application wholesale.

## 11. Explicit non-goals for the MVP

- Committing to production WYSIWYG or rich-text Markdown editing before the
  Milestone 0 fidelity spike proves the required source preservation.
- Shipping multiple production source-editor engines or retaining losing spike
  bundles after Milestone 0.
- Replacing the existing file explorer, native terminal workspace, agent panel,
  composer, or diff shell with an Electron/React/VS Code workbench.
- Arbitrary terminal/document nesting inside the runtime mux.
- Binary or hex editing.
- Unbounded or multi-gigabyte files.
- Raw HTML or script execution in preview.
- Automatic transmission of document contents to an agent.
- Arbitrary SSH host support before a backend and authentication decision.

## 12. Required verification before each delivery commit

- Focused document model and I/O tests.
- File-size, invalid UTF-8, symlink/special-file, stale revision, and atomic-save
  regressions.
- Markdown round-trip fixtures that assert byte-for-byte preservation when no
  source edit is requested.
- Image traversal, oversized image, decode failure, and cache invalidation tests.
- Annotation re-anchor success, ambiguity, and deletion tests.
- Agent-send secret warning and log-redaction tests.
- Existing project gate required by `CLAUDE.md`, including
  `cargo test -p deppy-sijo --bins file_tree`, immediately before commit.
