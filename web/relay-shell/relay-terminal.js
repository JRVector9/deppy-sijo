// Relay 셸의 터미널 화면 — **DOM 텍스트 행** 렌더러.
//
// deppy-mux 모바일 PWA와 같은 방식이다: 행마다 <div>, run마다 <span>. 글꼴 크기·폭 맞춤·
// 읽기 좋은 줄바꿈을 CSS로 바꿀 수 있고, 텍스트 선택·확대 접근성이 canvas보다 낫다. 보기 전용
// 셸이므로 입력·스크롤 명령은 없다 — 이 모듈이 내보내는 메시지는 `request_keyframe` 하나다.
//
// 프레임 계약(web-remote protocol.rs `ServerMsg::Viewport`):
//   { session, seq, keyframe, cols, rows, cursor{col,row,visible,shape}, alt, offset,
//     lines: [{ row, runs: [{ s, t, fg, bg, a, w }] }] }
// keyframe이면 전체 행, 아니면 바뀐 행만. 기준 화면 없이 delta가 오면 재동기화를 요청한다.
// 다른 세션의 잔여 프레임은 버린다.

export const TERMINAL_FONT_MIN_PX = 8;
export const TERMINAL_FONT_MAX_PX = 22;
export const TERMINAL_FONT_DEFAULT_PX = 12;
export const TERMINAL_LINE_HEIGHT_EM = 1.35;
/// 폭 맞춤 계산용 추정 셀 폭(em). 실제 폰트 계량을 재지 않고 deppy-mux와 같은 값을 쓴다.
const ESTIMATED_CELL_WIDTH_EM = 0.62;

const ATTR_BOLD = 1;
const ATTR_ITALIC = 2;
const ATTR_UNDERLINE = 4;
const ATTR_STRIKE = 8;
const ATTR_DIM = 16;

/// 기본 배경과 같은 run 배경은 투명으로 둔다 — 화면 배경이 그대로 비쳐야 행 경계가 안 보인다.
const DEFAULT_BACKGROUND = "#000000";
const DEFAULT_FOREGROUND = "#d4d4d4";

function element(tag, className) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  return node;
}

function clamp(value, low, high) {
  return Math.min(high, Math.max(low, value));
}

/// run 하나를 <span>으로. 색·굵기·기울임·밑줄·취소선·흐림을 계약의 비트 그대로 옮긴다.
function runSpan(run) {
  const span = element("span");
  const attrs = run.a | 0;
  const text = typeof run.t === "string" ? run.t : "";
  span.textContent = text;
  if (run.fg && run.fg !== DEFAULT_FOREGROUND) span.style.color = run.fg;
  if (run.bg && run.bg !== DEFAULT_BACKGROUND) span.style.backgroundColor = run.bg;
  if (attrs & ATTR_BOLD) span.style.fontWeight = "700";
  if (attrs & ATTR_ITALIC) span.style.fontStyle = "italic";
  if (attrs & ATTR_DIM) span.style.opacity = "0.65";
  const decorations = [];
  if (attrs & ATTR_UNDERLINE) decorations.push("underline");
  if (attrs & ATTR_STRIKE) decorations.push("line-through");
  if (decorations.length > 0) span.style.textDecorationLine = decorations.join(" ");
  return span;
}

/// 행 하나를 채운다. run 사이의 빈 열과 행 끝까지를 공백으로 메워 배경이 끊기지 않게 한다.
/// 읽기 좋은 줄바꿈 모드에서는 꼬리 공백을 줄여(최대 8) 화면 폭에 맞춰 접히게 둔다.
function fillRow(rowElement, runs, cols, readable) {
  rowElement.replaceChildren();
  let next = 0;
  for (const run of runs || []) {
    const start = run.s | 0;
    if (start > next) {
      rowElement.append(" ".repeat(readable ? Math.min(start - next, 8) : start - next));
    }
    rowElement.append(runSpan(run));
    const chars = Array.from(run.t || "").length;
    next = start + (run.w ? chars * 2 : chars);
  }
  if (!readable && next < cols) rowElement.append(" ".repeat(cols - next));
  if (rowElement.childNodes.length === 0) rowElement.append(readable ? " " : " ".repeat(cols));
}

/// `createTerminalView({ mount, requestKeyframe })`
///
/// - `applyViewport(frame)` → "applied" | "resync" | "ignored"
/// - `watch(sessionId)` / `unwatch()` — 어느 세션의 프레임을 받아들일지
/// - `setFontSize(px)`, `setFitWidth(bool)`, `setReadableWrap(bool)` — 표시 설정
/// - `settings()` — 현재 설정 스냅샷, `screen` — 현재 화면(테스트용)
export function createTerminalView({ mount, requestKeyframe }) {
  if (!mount) throw new TypeError("terminal view needs a mount element");
  const state = {
    session: null,
    screen: null,
    fontSizePx: TERMINAL_FONT_DEFAULT_PX,
    fitWidth: true,
    readableWrap: false,
    rowElements: [],
  };

  const grid = element("div", "m-term-grid");
  grid.setAttribute("role", "img");
  const cursor = element("span", "m-term-cursor");
  cursor.setAttribute("aria-hidden", "true");
  cursor.hidden = true;
  const empty = element("div", "m-term-empty");
  empty.textContent = "화면을 기다리는 중…";
  const scrollNote = element("div", "m-term-scroll-note");
  scrollNote.hidden = true;
  mount.replaceChildren(empty, scrollNote);

  function effectiveFontSize() {
    const screen = state.screen;
    if (!screen || !state.fitWidth) return state.fontSizePx;
    const width = mount.clientWidth;
    if (width <= 0) return state.fontSizePx;
    return Math.min(
      state.fontSizePx,
      Math.max(4, (width - 2) / Math.max(1, screen.cols * ESTIMATED_CELL_WIDTH_EM)),
    );
  }

  function applyLayout() {
    const screen = state.screen;
    if (!screen) return;
    grid.classList.toggle("fit", state.fitWidth && !state.readableWrap);
    grid.classList.toggle("wrap", state.readableWrap);
    grid.style.fontSize = `${effectiveFontSize().toFixed(2)}px`;
    grid.style.width = state.fitWidth || state.readableWrap ? "" : `${screen.cols}ch`;
    grid.style.minHeight = state.readableWrap ? "" : `${screen.rows * TERMINAL_LINE_HEIGHT_EM}em`;
    grid.setAttribute("aria-label", `터미널 화면 ${screen.cols}×${screen.rows}`);
  }

  function renderAllRows() {
    const screen = state.screen;
    grid.replaceChildren();
    state.rowElements = [];
    for (let row = 0; row < screen.rows; row += 1) {
      const rowElement = element("div", "m-term-row");
      fillRow(rowElement, screen.lines[row], screen.cols, state.readableWrap);
      state.rowElements.push(rowElement);
      grid.append(rowElement);
    }
    grid.append(cursor);
    if (!mount.contains(grid)) mount.replaceChildren(grid, scrollNote);
  }

  function renderCursor() {
    const screen = state.screen;
    const at = screen?.cursor;
    if (!screen || !at || !at.visible || state.readableWrap) {
      cursor.hidden = true;
      return;
    }
    cursor.hidden = false;
    cursor.className = "m-term-cursor";
    if (at.shape === "beam") cursor.classList.add("beam");
    if (at.shape === "underline") cursor.classList.add("underline");
    cursor.style.left = `${at.col | 0}ch`;
    const rowTop = (at.row | 0) * TERMINAL_LINE_HEIGHT_EM;
    cursor.style.top =
      at.shape === "underline"
        ? `${(rowTop + TERMINAL_LINE_HEIGHT_EM - 0.18).toFixed(3)}em`
        : `${rowTop.toFixed(3)}em`;
  }

  function renderScrollNote() {
    const offset = state.screen?.offset | 0;
    scrollNote.hidden = offset <= 0;
    if (offset > 0) scrollNote.textContent = `↑ ${offset}줄 위 (Mac이 과거를 열람 중)`;
  }

  function applyViewport(frame) {
    if (!frame || frame.session !== state.session) return "ignored";
    const screen = state.screen;
    if (!frame.keyframe && !screen) {
      requestKeyframe?.();
      return "resync";
    }
    const resized = !screen || screen.cols !== frame.cols || screen.rows !== frame.rows;
    let changedRows = [];
    if (frame.keyframe || resized) {
      state.screen = {
        cols: frame.cols | 0,
        rows: frame.rows | 0,
        lines: new Array(frame.rows | 0).fill(null),
        cursor: null,
        alt: false,
        offset: 0,
      };
    }
    const next = state.screen;
    for (const line of frame.lines || []) {
      const row = line.row | 0;
      if (row < next.rows) {
        next.lines[row] = line.runs || [];
        changedRows.push(row);
      }
    }
    next.cursor = frame.cursor || null;
    next.alt = !!frame.alt;
    next.offset = frame.offset | 0;
    if (frame.keyframe || resized) {
      applyLayout();
      renderAllRows();
    } else {
      for (const row of changedRows) {
        const rowElement = state.rowElements[row];
        if (rowElement) fillRow(rowElement, next.lines[row], next.cols, state.readableWrap);
      }
    }
    renderCursor();
    renderScrollNote();
    return "applied";
  }

  function rerender() {
    if (!state.screen) return;
    applyLayout();
    renderAllRows();
    renderCursor();
  }

  const resizeObserver =
    "ResizeObserver" in globalThis
      ? new ResizeObserver(() => {
          if (state.screen && state.fitWidth) applyLayout();
        })
      : null;
  resizeObserver?.observe(mount);

  return {
    get screen() {
      return state.screen;
    },
    watch(sessionId) {
      state.session = sessionId;
      state.screen = null;
      mount.replaceChildren(empty, scrollNote);
      cursor.hidden = true;
      scrollNote.hidden = true;
    },
    unwatch() {
      state.session = null;
      state.screen = null;
      state.rowElements = [];
      mount.replaceChildren(empty, scrollNote);
    },
    applyViewport,
    setFontSize(px) {
      state.fontSizePx = clamp(px | 0, TERMINAL_FONT_MIN_PX, TERMINAL_FONT_MAX_PX);
      applyLayout();
    },
    setFitWidth(enabled) {
      state.fitWidth = !!enabled;
      rerender();
    },
    setReadableWrap(enabled) {
      state.readableWrap = !!enabled;
      rerender();
    },
    settings() {
      return {
        fontSizePx: state.fontSizePx,
        fitWidth: state.fitWidth,
        readableWrap: state.readableWrap,
      };
    },
    destroy() {
      resizeObserver?.disconnect();
      state.session = null;
      state.screen = null;
      state.rowElements = [];
      mount.replaceChildren();
    },
  };
}
