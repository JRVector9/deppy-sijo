// Deppy Sijo 공용 뷰어 코어 — 루프백 셸(crates/web-remote/assets/app.js)과 Relay 셸
// (web/relay-shell)이 **같은 읽기 전용 전체화면 뷰어**를 공유하기 위한 ES 모듈.
//
// 이 모듈이 아는 것은 읽기 전용 뷰어 수명주기뿐이다:
//   - viewport 프레임(keyframe = 전체 행, delta = 바뀐 행) → 화면 모델 → canvas 렌더
//   - watch / unwatch / request_keyframe 전송
//   - visual viewport 추적, 핀치 확대 중 네이티브 팬 양보, 세로 팬 제스처
//   - Back(history) 처리와 닫는 동안의 조작 잠금
//   - 프라이버시 커튼, 연결/재연결 상태 표시
//
// 이 모듈이 절대 알지 않는 것(호스트 셸이 소유한다): 페어링 자격증명, 작성기·특수키 같은
// 쓰기 경로, 승인 화면, 첨부, 서비스 워커. 그런 동작은 전부 `hooks`로 위임한다 —
// 시청 전용 기기가 이 모듈만으로 완전히 동작해야 하기 때문이다.
// (이 계약은 Rust 쪽 소스 법칙 테스트가 강제한다 — 금지어가 들어오면 빌드 게이트가 깨진다.)
//
// ── API ──────────────────────────────────────────────────────────────────────
// createViewer({ send, elements, backdrop, hooks }) → viewer
//
//   send(message)   필수. `{ type: 'watch', session }`, `{ type: 'unwatch' }`,
//                   `{ type: 'request_keyframe' }`만 보낸다. 반환값은 쓰지 않는다.
//   elements        선택. 표준 id를 쓰지 않는 셸이 DOM 참조를 직접 넘길 때만.
//   backdrop        선택. 뷰어가 떠 있는 동안 inert 처리할 배경 요소
//                   (기본값 `#dashboard-shell`).
//   hooks           선택. 전부 없어도 동작한다(시청 전용 셸의 기본값).
//     closingChanged(closing)             닫힘 잠금이 켜지고 꺼질 때
//     connectionChanged(state, connected) 연결 상태가 바뀐 뒤
//     viewportSynced(width, height)       visual viewport 크기를 반영한 뒤
//     pan(lines)                          세로 팬 제스처(줄 단위, 양수 = 과거로)
//     resetPan()                          팬 누적 상태를 버려야 할 때
//     beforeOpen(sessionId)               열기 직전
//     sessionChanged(sessionId)           시청 세션이 바뀐 직후(아직 감춰져 있다)
//     afterOpen(sessionId)                화면을 띄운 뒤
//     beforeRequestClose()                닫기 요청을 받아들이기 직전
//     beforeClose(options, returnSession) 닫기 시작(잠금 직후)
//     beforeHide(options, returnSession)  상태를 비운 뒤, 감추기 직전
//     afterHide(options, returnSession)   감춘 뒤, 잠금 해제 직전
//     afterClose(options, returnSession)  잠금 해제 뒤
//
// 반환값은 **상태와 조작을 겸하는 하나의 객체**다. `viewer.watching`, `viewer.screen`,
// `viewer.connection`, `viewer.closing`, `viewer.el`, `viewer.canvas` … 를 그대로 읽고
// `viewer.openViewer(id, title)` 로 조작한다. 호스트 셸이 자기 필드를 얹어도 된다.
//
// 조작 메서드: openViewer, requestCloseViewer, finishCloseViewer, setViewerClosing,
// setViewerConnection, setPrivacyCurtain, handleViewport, rewatch, resetPan,
// updateScrollNote, clearViewerCanvas, clearStaleViewerHistory, activateViewerShell,
// scheduleViewerRender, scheduleViewportSettle, cancelScheduledViewerRender,
// scheduleViewerRenderForLayoutChange.

/// 호스트 문서에 뷰어 마크업이 없을 때(시청 전용 셸) 코어가 직접 만든다. 루프백 셸의
/// `index.html`에 있는 것과 같은 구조에서 쓰기 컨트롤(특수키·작성기)만 뺀 것이다 — 같은
/// id·class를 쓰므로 `viewer-core.css`와 아래 핸들이 그대로 맞는다.
function buildViewerDom(mount) {
  const el = (tag, attrs = {}, ...children) => {
    const node = document.createElement(tag);
    for (const [key, value] of Object.entries(attrs)) {
      if (key === 'text') node.textContent = value;
      else if (key === 'hidden') node.hidden = value;
      else node.setAttribute(key, value);
    }
    node.append(...children);
    return node;
  };
  const section = el(
    'section',
    { id: 'viewer', class: 'viewer-shell', role: 'dialog', 'aria-modal': 'true',
      'aria-labelledby': 'viewer-title viewer-session', hidden: true },
    el(
      'header',
      { class: 'viewer-header' },
      el('button', { id: 'viewer-back', class: 'viewer-back', type: 'button',
        'aria-label': '세션 목록으로 돌아가기', text: '‹' }),
      el(
        'div',
        { class: 'viewer-heading' },
        el('h2', { id: 'viewer-title', text: '터미널' }),
        el('p', { id: 'viewer-session', class: 'viewer-session', text: '세션' }),
      ),
      el(
        'p',
        { id: 'viewer-connection', class: 'viewer-connection', role: 'status',
          'aria-live': 'polite', 'aria-atomic': 'true' },
        el('span', { id: 'viewer-connection-label', text: '연결 중' }),
        el('span', { id: 'viewer-connection-detail', class: 'sr-only',
          text: '터미널 화면을 준비하고 있습니다.' }),
      ),
    ),
    el(
      'div',
      { id: 'viewer-stage', class: 'viewer-stage' },
      el('div', { class: 'viewer-wrap' },
        el('canvas', { id: 'viewer-canvas', 'aria-label': '원격 터미널 화면' })),
      el('div', { id: 'viewer-scroll-note', class: 'viewer-scroll-note', hidden: true },
        el('span', { id: 'viewer-offset-text' })),
      el(
        'div',
        { id: 'viewer-connection-overlay', class: 'viewer-overlay', 'aria-hidden': 'true',
          hidden: true },
        el('strong', { id: 'viewer-overlay-title', text: '연결 중' }),
        el('span', { id: 'viewer-overlay-detail', text: '터미널 화면을 준비하고 있습니다.' }),
      ),
      el('div', { id: 'viewer-privacy-curtain', class: 'viewer-privacy-curtain',
        'aria-hidden': 'true', hidden: true }, el('span', { text: '화면이 보호되었습니다' })),
    ),
  );
  mount.append(section);
  return section;
}

export function createViewer(options = {}) {
  const send = options.send || (() => false);
  const hooks = options.hooks || {};
  // 마크업이 없는 호스트(시청 전용 셸·계약 러너)에서는 코어가 스스로 선다.
  if (!options.elements && !document.getElementById('viewer')) {
    buildViewerDom(options.mount || document.body);
  }
  // 뒤에 깔린 대시보드는 있을 때만 잠근다 — 시청 전용 셸에는 없다.
  const dashboardShell = options.backdrop || document.getElementById('dashboard-shell');

  const viewer = {
    el: document.getElementById('viewer'),
    label: document.getElementById('viewer-session'),
    canvas: document.getElementById('viewer-canvas'),
    wrap: document.querySelector('#viewer .viewer-wrap'),
    back: document.getElementById('viewer-back'),
    connectionStatus: document.getElementById('viewer-connection'),
    connectionLabel: document.getElementById('viewer-connection-label'),
    connectionDetail: document.getElementById('viewer-connection-detail'),
    overlay: document.getElementById('viewer-connection-overlay'),
    overlayTitle: document.getElementById('viewer-overlay-title'),
    overlayDetail: document.getElementById('viewer-overlay-detail'),
    privacy: document.getElementById('viewer-privacy-curtain'),
    scrollNote: document.getElementById('viewer-scroll-note'),
    offsetText: document.getElementById('viewer-offset-text'),
    ...(options.elements || {}),
    watching: null,
    returnSession: null,
    screen: null,
    closing: false,
    pendingClose: null,
    connection: 'connecting',
  };

  const VIEWER_CONNECTION_COPY = {
    connecting: ['연결 중', '터미널 화면을 준비하고 있습니다.'],
    reconnecting: ['재연결 중', '마지막 화면을 유지합니다. 연결되기 전에는 입력할 수 없습니다.'],
    paused: ['일시정지', '앱으로 돌아오면 다시 연결합니다.'],
  };

  function setViewerClosing(closing) {
    viewer.closing = closing;
    viewer.back.disabled = closing;
    if (hooks.closingChanged) hooks.closingChanged(closing);
  }

  function setViewerConnection(state) {
    viewer.connection = state;
    const connected = state === 'connected';
    const copy = connected ? ['연결됨', ''] : VIEWER_CONNECTION_COPY[state];
    viewer.connectionLabel.textContent = copy[0];
    viewer.connectionDetail.textContent = copy[1];
    viewer.connectionStatus.className = 'viewer-connection ' + state;
    viewer.overlay.hidden = connected;
    if (!connected) {
      viewer.overlayTitle.textContent = copy[0];
      viewer.overlayDetail.textContent = copy[1];
      resetPan();
    }
    if (hooks.connectionChanged) hooks.connectionChanged(state, connected);
  }

  // 프라이버시 커튼 — 백그라운드로 나갈 때만 덮는다(시청 중일 때). 돌아오면 항상 걷는다.
  function setPrivacyCurtain(covered) {
    if (covered) {
      if (viewer.watching) viewer.privacy.hidden = false;
    } else {
      viewer.privacy.hidden = true;
    }
  }

  function clearViewerCanvas() {
    const canvas = viewer.canvas;
    const context = canvas.getContext('2d');
    context.setTransform(1, 0, 0, 1, 0, 0);
    context.fillStyle = '#000000';
    context.fillRect(0, 0, canvas.width, canvas.height);
  }

  function activateViewerShell() {
    if (dashboardShell) {
      dashboardShell.inert = true;
      dashboardShell.setAttribute('aria-hidden', 'true');
    }
    document.body.classList.add('viewer-open');
    viewer.el.hidden = false;
  }

  function clearStaleViewerHistory() {
    if (!(history.state && history.state.deppyViewer)) return;
    const cleanState = { ...history.state };
    delete cleanState.deppyViewer;
    history.replaceState(Object.keys(cleanState).length ? cleanState : null, '', location.href);
  }

  function finishCloseViewer(options = {}) {
    if (!viewer.watching) return;
    setViewerClosing(true);
    const returnSession = viewer.returnSession;
    if (hooks.beforeClose) hooks.beforeClose(options, returnSession);
    cancelScheduledViewerRender(); // central viewer close
    send({ type: 'unwatch' });
    viewer.watching = null;
    viewer.returnSession = null;
    viewer.screen = null;
    viewer.pendingClose = null;
    if (hooks.beforeHide) hooks.beforeHide(options, returnSession);
    resetPan();
    updateScrollNote();
    viewer.el.hidden = true;
    document.body.classList.remove('viewer-open');
    if (dashboardShell) {
      dashboardShell.inert = false;
      dashboardShell.removeAttribute('aria-hidden');
    }
    if (hooks.afterHide) hooks.afterHide(options, returnSession);
    setViewerClosing(false);
    if (hooks.afterClose) hooks.afterClose(options, returnSession);
  }

  function mergeViewerCloseOptions(current = {}, incoming = {}) {
    return {
      rerender: current.rerender !== false && incoming.rerender !== false,
      notice: [current.notice, incoming.notice].filter(Boolean).join(' '),
      discardDraft: !!(current.discardDraft || incoming.discardDraft),
    };
  }

  function requestCloseViewer(options = {}) {
    if (!viewer.watching) return;
    if (viewer.closing) {
      viewer.pendingClose = mergeViewerCloseOptions(viewer.pendingClose, options); // merge while awaiting popstate
      return;
    }
    if (hooks.beforeRequestClose) hooks.beforeRequestClose();
    const ownsHistory = !!(history.state && history.state.deppyViewer);
    if (ownsHistory) {
      setViewerClosing(true);
      viewer.pendingClose = options;
      resetPan();
      history.back();
      return;
    }
    finishCloseViewer(options);
  }

  /// `sessionId`는 영속 UUID 문자열이다 (I1).
  function openViewer(sessionId, title) {
    if (!sessionId || viewer.closing || viewer.watching === sessionId) return false;
    if (hooks.beforeOpen) hooks.beforeOpen(sessionId);
    setViewerClosing(false);
    viewer.watching = sessionId;
    viewer.returnSession = sessionId;
    viewer.screen = null;
    clearViewerCanvas();
    resetPan();
    updateScrollNote();
    if (hooks.sessionChanged) hooks.sessionChanged(sessionId);
    viewer.label.textContent = title || '세션';
    activateViewerShell();
    if (!(history.state && history.state.deppyViewer)) {
      history.pushState({ ...(history.state || {}), deppyViewer: true }, '', location.href);
    }
    if (hooks.afterOpen) hooks.afterOpen(sessionId);
    viewer.back.focus();
    scheduleViewerRender(); // first full-screen frame
    if (viewer.connection === 'connected') {
      send({ type: 'watch', session: sessionId });
    }
    return true;
  }

  // 재연결 — 서버 접속 상태(시청)가 초기화됐으므로 보던 세션을 다시 watch한다.
  // 식별자가 영속 UUID라 재시작 뒤에도 같은 세션이 잡힌다 (I1).
  function rewatch() {
    if (!viewer.watching) return;
    viewer.screen = null;
    send({ type: 'watch', session: viewer.watching });
  }

  viewer.back.addEventListener('click', () => requestCloseViewer());
  window.addEventListener('popstate', () => {
    if (viewer.watching && !(history.state && history.state.deppyViewer)) {
      finishCloseViewer(viewer.pendingClose || {});
    } else if (!viewer.watching && history.state && history.state.deppyViewer) {
      clearStaleViewerHistory();
    }
  });

  function handleViewport(msg) {
    if (msg.session !== viewer.watching) return; // 전환 직후 이전 세션의 잔여 프레임
    if (!msg.keyframe && !viewer.screen) {
      // delta인데 기준 화면이 없다 — 재동기화 요청 (P5c RequestKeyframe)
      send({ type: 'request_keyframe' });
      return;
    }
    if (msg.keyframe || viewer.screen.cols !== msg.cols || viewer.screen.rows !== msg.rows) {
      viewer.screen = { cols: msg.cols, rows: msg.rows, lines: new Array(msg.rows).fill(null) };
    }
    for (const line of msg.lines || []) {
      if (line.row < viewer.screen.rows) viewer.screen.lines[line.row] = line.runs || [];
    }
    viewer.screen.cursor = msg.cursor || null;
    viewer.screen.alt = !!msg.alt;
    viewer.screen.offset = msg.offset | 0;
    updateScrollNote();
    viewerScreenRevision += 1;
    scheduleViewerRender();
  }

  // ── 세로 팬 제스처 — 터치/휠을 줄 단위 delta로 바꿔 호스트에 넘긴다(양수 = 과거로).
  // 실제 스크롤백 명령은 쓰기 권한이 있는 셸만 보낸다 — 코어는 제스처만 해석한다.
  let lastTouchY = null;

  function pan(lines) {
    if (hooks.pan) hooks.pan(lines);
  }

  function resetPan() {
    lastTouchY = null;
    if (hooks.resetPan) hooks.resetPan();
  }

  function updateScrollNote() {
    const note = viewer.scrollNote;
    const text = viewer.offsetText;
    if (!note) return;
    const offset = (viewer.screen && viewer.screen.offset) || 0;
    note.hidden = offset <= 0;
    if (offset > 0) text.textContent = '↑ ' + offset + '줄 위 (과거 열람 중)';
  }

  viewer.canvas.addEventListener('touchstart', (e) => {
    if (e.touches.length === 1) lastTouchY = e.touches[0].clientY;
  }, { passive: true });
  viewer.canvas.addEventListener('touchmove', (e) => {
    if (window.visualViewport && window.visualViewport.scale > 1.01) {
      lastTouchY = null;
      return; // native pan while zoomed
    }
    if (lastTouchY == null || e.touches.length !== 1) return;
    e.preventDefault(); // 페이지 스크롤 대신 터미널 스크롤백
    const y = e.touches[0].clientY;
    const dy = y - lastTouchY;
    lastTouchY = y;
    // 손가락을 아래로 끌면(dy>0) 과거로 — 콘텐츠가 손가락을 따라온다.
    pan(dy / (viewer.cellH || 16));
  }, { passive: false });
  viewer.canvas.addEventListener('touchend', () => { lastTouchY = null; }, { passive: true });
  viewer.canvas.addEventListener('wheel', (e) => {
    if (e.ctrlKey) return; // preserve browser pinch zoom
    e.preventDefault();
    // 휠 위(deltaY<0) = 과거로(양수 delta).
    pan(-e.deltaY / (viewer.cellH || 16));
  }, { passive: false });

  const CELL_ASPECT_RATIO = 2;
  const MAX_CANVAS_PIXELS = 8 * 1024 * 1024;
  let viewerRenderFrame = 0;
  let viewerViewportSettleTimer = 0;
  let viewerScreenRevision = 0;
  let lastViewerRenderKey = '';

  function syncViewerViewport() {
    const visualViewport = window.visualViewport;
    const pinched = !!(visualViewport && visualViewport.scale > 1.01);
    // Pinch zoom is accessibility magnification. Use current layout geometry instead of
    // stale inline vars or the narrower visual viewport, then let native zoom/pan own it.
    const top = pinched ? 0 : (visualViewport ? visualViewport.offsetTop : 0);
    const left = pinched ? 0 : (visualViewport ? visualViewport.offsetLeft : 0);
    const width = pinched
      ? document.documentElement.clientWidth
      : (visualViewport ? visualViewport.width : window.innerWidth);
    const height = pinched
      ? document.documentElement.clientHeight
      : (visualViewport ? visualViewport.height : window.innerHeight);
    const topPx = Math.max(0, Math.round(top)) + 'px';
    const leftPx = Math.max(0, Math.round(left)) + 'px';
    const widthPx = Math.max(1, Math.round(width)) + 'px';
    const heightPx = Math.max(1, Math.round(height)) + 'px';
    if (viewer.el.style.getPropertyValue('--viewer-top') !== topPx) {
      viewer.el.style.setProperty('--viewer-top', topPx);
    }
    if (viewer.el.style.getPropertyValue('--viewer-height') !== heightPx) {
      viewer.el.style.setProperty('--viewer-height', heightPx);
    }
    if (viewer.el.style.getPropertyValue('--viewer-left') !== leftPx) {
      viewer.el.style.setProperty('--viewer-left', leftPx);
    }
    if (viewer.el.style.getPropertyValue('--viewer-width') !== widthPx) {
      viewer.el.style.setProperty('--viewer-width', widthPx);
    }
    // 컨트롤 영역 높이는 셸마다 다르다(시청 전용 셸은 아예 없다) — 호스트가 정한다.
    if (hooks.viewportSynced) hooks.viewportSynced(width, height);
  }

  function scheduleViewerRender() {
    if (!viewer.watching || viewerRenderFrame) return;
    viewerRenderFrame = requestAnimationFrame(() => {
      viewerRenderFrame = 0;
      syncViewerViewport();
      drawScreenNow();
    });
  }

  function scheduleViewportSettle() {
    if (!viewer.watching) return; // do not arm settle after close
    scheduleViewerRender();
    if (viewerViewportSettleTimer) clearTimeout(viewerViewportSettleTimer);
    viewerViewportSettleTimer = setTimeout(() => {
      viewerViewportSettleTimer = 0;
      scheduleViewerRender();
    }, 64);
  }

  function cancelScheduledViewerRender() {
    if (viewerRenderFrame) cancelAnimationFrame(viewerRenderFrame);
    if (viewerViewportSettleTimer) clearTimeout(viewerViewportSettleTimer);
    viewerRenderFrame = 0;
    viewerViewportSettleTimer = 0;
  }

  function drawScreenNow() {
    const screen = viewer.screen;
    if (!screen || viewer.el.hidden) return;
    const canvas = viewer.canvas;
    const availableWidth = viewer.wrap.clientWidth;
    const availableHeight = viewer.wrap.clientHeight;
    if (availableWidth <= 0 || availableHeight <= 0 || screen.cols <= 0 || screen.rows <= 0) return;
    const cellW = Math.min(
      availableWidth / screen.cols,
      availableHeight / (screen.rows * CELL_ASPECT_RATIO),
    );
    const cellH = cellW * CELL_ASPECT_RATIO;
    viewer.cellH = cellH;
    const cssWidth = cellW * screen.cols;
    const cssHeight = cellH * screen.rows;
    const visualScale = window.visualViewport ? window.visualViewport.scale : 1;
    const requestedDpr = (window.devicePixelRatio || 1) * Math.max(1, visualScale || 1);
    const pixelBudgetDpr = Math.sqrt(MAX_CANVAS_PIXELS / Math.max(1, cssWidth * cssHeight));
    const dpr = Math.min(requestedDpr, pixelBudgetDpr);
    const renderKey = [
      viewerScreenRevision,
      availableWidth.toFixed(2),
      availableHeight.toFixed(2),
      dpr.toFixed(3),
    ].join(':');
    if (renderKey === lastViewerRenderKey) return;
    lastViewerRenderKey = renderKey;
    const pixelWidth = Math.max(1, Math.round(cssWidth * dpr));
    const pixelHeight = Math.max(1, Math.round(cssHeight * dpr));
    if (canvas.width !== pixelWidth) canvas.width = pixelWidth;
    if (canvas.height !== pixelHeight) canvas.height = pixelHeight;
    canvas.style.width = cssWidth + 'px';
    canvas.style.height = cssHeight + 'px';
    const ctx = canvas.getContext('2d');
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.fillStyle = '#000000';
    ctx.fillRect(0, 0, cssWidth, cssHeight);
    const A_BOLD = 1;
    const A_ITALIC = 2;
    const A_UNDERLINE = 4;
    const A_STRIKE = 8;
    const A_DIM = 16;
    const fontPx = (cellH * 0.82).toFixed(2);
    const fontFor = (attrs) => {
      const style = attrs & A_ITALIC ? 'italic ' : '';
      const weight = attrs & A_BOLD ? '700 ' : '';
      return style + weight + fontPx + 'px ui-monospace, Menlo, monospace';
    };
    const dimmed = (hex) => {
      const match = /^#?([0-9a-f]{6})$/i.exec(hex || '');
      if (!match) return hex;
      const value = parseInt(match[1], 16);
      const fade = (channel) => Math.round(channel * 0.6);
      return `rgb(${fade((value >> 16) & 255)},${fade((value >> 8) & 255)},${fade(value & 255)})`;
    };
    ctx.font = fontFor(0);
    ctx.textBaseline = 'middle';
    for (let row = 0; row < screen.rows; row++) {
      const runs = screen.lines[row];
      if (!runs) continue;
      const y = row * cellH;
      for (const run of runs) {
        const advance = run.w ? cellW * 2 : cellW;
        const chars = Array.from(run.t || '');
        const attrs = run.a || 0;
        ctx.fillStyle = run.bg || '#000000';
        ctx.fillRect(run.s * cellW, y, chars.length * advance, cellH);
        const foreground = attrs & A_DIM
          ? dimmed(run.fg || '#d4d4d4')
          : (run.fg || '#d4d4d4');
        ctx.fillStyle = foreground;
        ctx.font = fontFor(attrs);
        for (let index = 0; index < chars.length; index++) {
          if (chars[index] === ' ') continue;
          ctx.fillText(chars[index], run.s * cellW + index * advance, y + cellH / 2, advance);
        }
        if (attrs & (A_UNDERLINE | A_STRIKE)) {
          const x = run.s * cellW;
          const width = chars.length * advance;
          ctx.fillStyle = foreground;
          if (attrs & A_UNDERLINE) ctx.fillRect(x, y + cellH - 1.5, width, 1);
          if (attrs & A_STRIKE) ctx.fillRect(x, y + cellH / 2, width, 1);
        }
      }
    }
    ctx.font = fontFor(0);
    const cursor = screen.cursor;
    if (cursor && cursor.visible) {
      ctx.fillStyle = 'rgba(212, 212, 212, 0.45)';
      ctx.fillRect(cursor.col * cellW, cursor.row * cellH, cellW, cellH);
    }
  }

  window.addEventListener('resize', scheduleViewerRender);
  if (window.visualViewport) {
    window.visualViewport.addEventListener('resize', scheduleViewportSettle);
    window.visualViewport.addEventListener('scroll', scheduleViewerRender);
  }
  const hasViewerResizeObserver = 'ResizeObserver' in window;
  if (hasViewerResizeObserver) {
    new ResizeObserver(scheduleViewerRender).observe(viewer.wrap);
  }

  function scheduleViewerRenderForLayoutChange(previousWrapHeight) {
    if (viewer.wrap.clientHeight !== previousWrapHeight) scheduleViewerRender();
  }

  clearStaleViewerHistory();

  return Object.assign(viewer, {
    openViewer,
    requestCloseViewer,
    finishCloseViewer,
    mergeViewerCloseOptions,
    setViewerClosing,
    setViewerConnection,
    setPrivacyCurtain,
    handleViewport,
    rewatch,
    resetPan,
    updateScrollNote,
    clearViewerCanvas,
    clearStaleViewerHistory,
    activateViewerShell,
    scheduleViewerRender,
    scheduleViewportSettle,
    cancelScheduledViewerRender,
    scheduleViewerRenderForLayoutChange,
  });
}
