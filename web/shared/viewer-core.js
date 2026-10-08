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
//     layoutChanged(metrics)             셀·stage 크기를 반영한 뒤(첫 프레임 전에도 호출)
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
// settings() → { fontSize, overview, readableWrap }, setFontSize(px) (12–24px), setOverview(bool)
// getCellMetrics() → { cellWidth, cellHeight, fontSize } (개요에서는 실제 축소 크기).
// layoutChanged에는 stageWidth, stageHeight, configuredFontSize도 포함한다.
// resumeFollow() → 현재 커서/최신 출력 따라가기. 수동 이동 후에는 명시적으로 재개한다.
// setReadableWrap(bool) → 좌표 커서를 숨기는 읽기 줄바꿈(기기 저장).
// getSelectedText() → 터미널에 속한 선택 문자열. findText(query, direction = 1),
// clearSearch() → 현재 불러온 화면만 검색. { query, total, index, scope: 'loaded viewport' }.
// hooks.searchChanged(result) → 프레임 갱신으로 검색 결과가 바뀔 때.
// expectHistoryWindow(request) → 호스트가 보낸 nonzero-u32 기록 요청 응답만 설치한다.
// cancelHistoryWindow(request) → 현재 요청만 취소하고 불러온 창을 유지한다.
// getHistoryState(), hooks.readingChanged(state) → 불러온 창 보존/만료/요청 상태.
// hooks.resumeHistory() → 기록 열람에서 현재 화면 복귀를 호스트에 위임한다.

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
        el('span', { id: 'viewer-offset-text' }),
        el('button', { id: 'viewer-resume-follow', type: 'button', text: '현재 화면으로' })),
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
    resumeFollowButton: document.getElementById('viewer-resume-follow'),
    ...(options.elements || {}),
    watching: null,
    returnSession: null,
    screen: null,
    closing: false,
    pendingClose: null,
    connection: 'connecting',
    followingLive: true,
  };

  if (viewer.scrollNote) {
    viewer.el.insertBefore(viewer.scrollNote, viewer.wrap.parentElement);
    viewer.el.classList.add('viewer-has-scroll-note');
  }

  const FONT_FAMILY = 'ui-monospace, Menlo, monospace';
  const PREFS_KEY = 'deppy-viewer:prefs';
  const prefs = { fontSize: 15, overview: false, readableWrap: false };
  try {
    const saved = JSON.parse(localStorage.getItem(PREFS_KEY));
    if (saved && Number.isFinite(saved.fontSize)) prefs.fontSize = Math.max(12, Math.min(24, saved.fontSize));
    if (saved && typeof saved.overview === 'boolean') prefs.overview = saved.overview;
    if (saved && typeof saved.readableWrap === 'boolean') prefs.readableWrap = saved.readableWrap;
  } catch (_) { /* Private browsing or unavailable storage keeps readable defaults. */ }
  if (prefs.readableWrap) prefs.overview = false;
  viewer.wrap.classList.toggle('overview', prefs.overview);
  viewer.wrap.classList.toggle('readable', prefs.readableWrap);
  const textLayer = document.createElement('div');
  textLayer.className = 'viewer-text-layer';
  textLayer.setAttribute('role', 'document');
  textLayer.setAttribute('aria-label', '터미널 출력 — 현재 불러온 화면');
  const searchMarks = document.createElement('div');
  searchMarks.className = 'viewer-search-marks';
  searchMarks.setAttribute('aria-hidden', 'true');
  textLayer.append(searchMarks);
  viewer.wrap.append(textLayer);
  viewer.textLayer = textLayer;
  viewer.canvas.setAttribute('aria-hidden', 'true');
  const textRows = [];
  const textRowKeys = [];
  let searchQuery = '';
  let searchMatches = [];
  let searchIndex = -1;
  let wireScreen = null;
  let keyframeRequested = false;
  let retainedDisplay = false;
  let expiredHistory = false;
  let pendingHistoryRequest = null;
  let waitingForResume = false;
  let registeringResume = false;
  let lastReadingStateKey = '';
  const localPositions = new Map();
  let restoreLocalPosition = false;
  let automaticPosition = null;
  let measuredFontSize = 0;
  let baseMetrics = null;
  let cellMetrics = null;
  let lastLayoutKey = '';
  const measuringContext = document.createElement('canvas').getContext('2d');
  const positionKey = session => session + (prefs.readableWrap ? ':reading' : ':grid');

  function getHistoryState() {
    const metadata = viewer.screen && viewer.screen.history;
    return {
      retained: retainedDisplay,
      expired: expiredHistory,
      pendingRequest: pendingHistoryRequest,
      offset: viewer.screen ? viewer.screen.offset : 0,
      latestOffset: wireScreen ? wireScreen.offset : 0,
      totalLines: metadata ? metadata.total_lines : null,
      generation: metadata ? metadata.generation : null,
      firstLine: metadata ? metadata.first_line : null,
    };
  }

  function notifyReadingChanged() {
    const state = getHistoryState();
    const key = JSON.stringify(state);
    if (key === lastReadingStateKey) return;
    lastReadingStateKey = key;
    if (hooks.readingChanged) hooks.readingChanged(state);
  }

  function resetHistoryState() {
    wireScreen = null;
    keyframeRequested = false;
    retainedDisplay = false;
    expiredHistory = false;
    pendingHistoryRequest = null;
    waitingForResume = false;
    lastReadingStateKey = '';
  }

  function expectHistoryWindow(request) {
    if (!viewer.watching || !Number.isInteger(request) || request <= 0 || request > 0xffffffff) return false;
    pendingHistoryRequest = request;
    if (!registeringResume) {
      waitingForResume = false;
      viewer.followingLive = false;
    }
    retainedDisplay = true;
    rememberLocalPosition();
    updateScrollNote();
    notifyReadingChanged();
    return true;
  }

  function cancelHistoryWindow(request) {
    if (pendingHistoryRequest === null || request !== pendingHistoryRequest) return false;
    pendingHistoryRequest = null;
    waitingForResume = false;
    retainedDisplay = true;
    viewer.followingLive = false;
    rememberLocalPosition();
    updateScrollNote();
    notifyReadingChanged();
    return true;
  }

  function inferHistoryExpiration() {
    const loaded = viewer.screen && viewer.screen.history;
    const latest = wireScreen && wireScreen.history;
    if (!retainedDisplay || !loaded || !latest) return;
    if (loaded.generation !== latest.generation) { expiredHistory = true; return; }
    if (typeof loaded.first_line !== 'string' || typeof latest.first_line !== 'string'
        || !/^\d+$/.test(loaded.first_line) || !/^\d+$/.test(latest.first_line)
        || !Number.isSafeInteger(latest.total_lines)) return;
    const floor = BigInt(latest.first_line) + BigInt(wireScreen.offset)
      + BigInt(wireScreen.rows) - BigInt(latest.total_lines);
    if (BigInt(loaded.first_line) < floor) expiredHistory = true;
  }

  function publishScreen(screen, manual = false) {
    const previous = viewer.screen;
    const differentWindow = previous && (previous.history && screen.history
      ? previous.history.generation !== screen.history.generation || previous.history.first_line !== screen.history.first_line
      : previous.offset !== screen.offset || previous.cols !== screen.cols || previous.rows !== screen.rows);
    if (manual && differentWindow) {
      if (selectionTouchesText()) window.getSelection().removeAllRanges();
      clearSearch();
    }
    viewer.screen = screen;
    expiredHistory = false;
    viewerScreenRevision += 1;
    updateScrollNote();
    scheduleViewerRender();
  }

  function measureCells() {
    if (measuredFontSize !== prefs.fontSize) {
      measuringContext.font = prefs.fontSize + 'px ' + FONT_FAMILY;
      baseMetrics = {
        cellWidth: measuringContext.measureText('M'.repeat(32)).width / 32,
        cellHeight: Math.ceil(prefs.fontSize * 1.4),
        fontSize: prefs.fontSize,
      };
      measuredFontSize = prefs.fontSize;
    }
    return baseMetrics;
  }

  function settings() {
    return { ...prefs };
  }

  function savePreferences() {
    try { localStorage.setItem(PREFS_KEY, JSON.stringify(prefs)); } catch (_) { /* Display settings still work. */ }
    cellMetrics = null;
    scheduleViewerRender();
  }

  function setFontSize(px) {
    if (!Number.isFinite(px)) return;
    const size = Math.max(12, Math.min(24, px));
    if (size === prefs.fontSize) return;
    prefs.fontSize = size;
    savePreferences();
  }

  function rememberLocalPosition() {
    if (viewer.watching && !prefs.overview && !restoreLocalPosition) {
      localPositions.set(positionKey(viewer.watching), {
        left: viewer.wrap.scrollLeft, top: viewer.wrap.scrollTop, follow: viewer.followingLive,
      });
    }
  }

  function stopFollowing() {
    if (!viewer.watching) return;
    if (waitingForResume && pendingHistoryRequest !== null) cancelHistoryWindow(pendingHistoryRequest);
    viewer.followingLive = false;
    retainedDisplay = true;
    rememberLocalPosition();
    updateScrollNote();
    notifyReadingChanged();
  }

  function resumeFollow() {
    if (waitingForResume && pendingHistoryRequest !== null) return;
    const selection = window.getSelection();
    if (selectionTouchesText()) selection.removeAllRanges();
    clearSearch();
    viewer.followingLive = true;
    const needsReset = (wireScreen && wireScreen.offset > 0) || pendingHistoryRequest !== null
      || (retainedDisplay && hooks.resumeHistory);
    if (needsReset) {
      retainedDisplay = true;
      waitingForResume = true;
      pendingHistoryRequest = null;
      if (hooks.resumeHistory) {
        registeringResume = true;
        try { hooks.resumeHistory(); } finally { registeringResume = false; }
      }
      if (pendingHistoryRequest === null) { waitingForResume = false; viewer.followingLive = false; }
    } else {
      retainedDisplay = false;
      waitingForResume = false;
      expiredHistory = false;
      if (wireScreen) publishScreen(wireScreen);
    }
    rememberLocalPosition();
    updateScrollNote();
    notifyReadingChanged();
    scheduleViewerRender();
  }
  if (viewer.resumeFollowButton) viewer.resumeFollowButton.addEventListener('click', resumeFollow);

  function setLocalPosition(left, top) {
    viewer.wrap.scrollLeft = left;
    viewer.wrap.scrollTop = top;
    automaticPosition = scrollGeometry();
  }

  function scrollGeometry() {
    return {
      left: viewer.wrap.scrollLeft, top: viewer.wrap.scrollTop,
      width: viewer.wrap.clientWidth, height: viewer.wrap.clientHeight,
      maxLeft: Math.max(0, viewer.wrap.scrollWidth - viewer.wrap.clientWidth),
      maxTop: Math.max(0, viewer.wrap.scrollHeight - viewer.wrap.clientHeight),
    };
  }

  function setOverview(overview) {
    overview = !!overview;
    if (overview === prefs.overview) return;
    rememberLocalPosition();
    if (overview && prefs.readableWrap) {
      prefs.readableWrap = false;
      viewer.wrap.classList.remove('readable');
      restoreLocalPosition = true;
    }
    prefs.overview = overview;
    viewer.wrap.classList.toggle('overview', overview);
    if (!overview) restoreLocalPosition = true;
    resetPan();
    savePreferences();
  }

  function setReadableWrap(readable) {
    readable = !!readable;
    if (readable === prefs.readableWrap) return;
    rememberLocalPosition();
    if (viewer.watching) stopFollowing();
    viewer.followingLive = false;
    prefs.readableWrap = readable;
    if (readable) prefs.overview = false;
    viewer.wrap.classList.toggle('readable', readable);
    viewer.wrap.classList.toggle('overview', prefs.overview);
    restoreLocalPosition = true;
    lastViewerRenderKey = '';
    resetPan();
    updateScrollNote();
    savePreferences();
  }

  function getSelectedText() {
    if (!viewer.privacy.hidden) return '';
    const selection = window.getSelection();
    return selection && selection.rangeCount && textLayer.contains(selection.anchorNode)
      && textLayer.contains(selection.focusNode) ? selection.toString() : '';
  }

  function selectionTouchesText() {
    const selection = window.getSelection();
    return !!(selection && selection.rangeCount && !selection.isCollapsed
      && selection.getRangeAt(0).intersectsNode(textLayer));
  }

  document.addEventListener('selectionchange', () => {
    if (viewer.privacy.hidden && selectionTouchesText()) stopFollowing();
  });
  document.addEventListener('copy', event => {
    if (!viewer.privacy.hidden && selectionTouchesText()) {
      if (event.clipboardData) event.clipboardData.setData('text/plain', '');
      event.preventDefault();
      return;
    }
    const text = getSelectedText();
    if (text && event.clipboardData) {
      event.clipboardData.setData('text/plain', text);
      event.preventDefault();
    }
  });

  function getCellMetrics() {
    return { ...(cellMetrics || measureCells()) };
  }

  viewer.wrap.addEventListener('scroll', () => {
    if (restoreLocalPosition || prefs.overview) return;
    const position = scrollGeometry();
    const samePosition = automaticPosition && position.left === automaticPosition.left
      && position.top === automaticPosition.top;
    const changedGeometry = automaticPosition && ['width', 'height', 'maxLeft', 'maxTop']
      .some(key => position[key] !== automaticPosition[key]);
    const layoutClamp = changedGeometry
      && position.left === Math.min(automaticPosition.left, position.maxLeft)
      && position.top === Math.min(automaticPosition.top, position.maxTop);
    if (!samePosition && !layoutClamp) stopFollowing();
    else automaticPosition = position;
    if (layoutClamp) scheduleViewerRender();
    rememberLocalPosition();
  }, { passive: true });
  viewer.wrap.addEventListener('wheel', (event) => {
    if (!event.ctrlKey && (event.deltaX || event.deltaY)) stopFollowing();
  }, { passive: true });
  viewer.wrap.addEventListener('touchmove', (event) => {
    if (event.touches.length === 1 && !(window.visualViewport && window.visualViewport.scale > 1.01)) stopFollowing();
  }, { passive: true });

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
      wireScreen = null;
      keyframeRequested = false;
      pendingHistoryRequest = null;
      if (waitingForResume) viewer.followingLive = false;
      waitingForResume = false;
      viewer.overlayTitle.textContent = copy[0];
      viewer.overlayDetail.textContent = copy[1];
      resetPan();
      notifyReadingChanged();
    }
    if (hooks.connectionChanged) hooks.connectionChanged(state, connected);
  }

  // 프라이버시 커튼 — 백그라운드로 나갈 때만 덮는다(시청 중일 때). 돌아오면 항상 걷는다.
  function setPrivacyCurtain(covered) {
    if (covered) {
      if (viewer.watching) {
        if (pendingHistoryRequest !== null) cancelHistoryWindow(pendingHistoryRequest);
        if (selectionTouchesText()) window.getSelection().removeAllRanges();
        viewer.privacy.hidden = false;
        textLayer.inert = true;
        textLayer.setAttribute('aria-hidden', 'true');
        clearSearch();
      }
    } else {
      viewer.privacy.hidden = true;
      textLayer.inert = false;
      textLayer.removeAttribute('aria-hidden');
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
    rememberLocalPosition();
    send({ type: 'unwatch' });
    viewer.watching = null;
    viewer.returnSession = null;
    viewer.screen = null;
    resetHistoryState();
    resetTextLayer();
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
    notifyReadingChanged();
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
    rememberLocalPosition();
    setViewerClosing(false);
    viewer.watching = sessionId;
    viewer.returnSession = sessionId;
    viewer.screen = null;
    resetHistoryState();
    clearViewerCanvas();
    resetTextLayer();
    viewer.canvas.style.width = '0px';
    viewer.canvas.style.height = '0px';
    restoreLocalPosition = true;
    viewer.followingLive = (localPositions.get(positionKey(sessionId)) || { follow: !prefs.readableWrap }).follow;
    retainedDisplay = !viewer.followingLive;
    cellMetrics = null;
    lastLayoutKey = '';
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
    notifyReadingChanged();
    return true;
  }

  // 재연결 — 서버 접속 상태(시청)가 초기화됐으므로 보던 세션을 다시 watch한다.
  // 식별자가 영속 UUID라 재시작 뒤에도 같은 세션이 잡힌다 (I1).
  function rewatch() {
    if (!viewer.watching) return;
    wireScreen = null;
    keyframeRequested = false;
    pendingHistoryRequest = null;
    if (waitingForResume) viewer.followingLive = false;
    waitingForResume = false;
    notifyReadingChanged();
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
    const request = msg.history && msg.history.request;
    const explicit = Number.isInteger(request) && request > 0 && request <= 0xffffffff;
    const matching = explicit && request === pendingHistoryRequest;
    if (explicit && !matching) {
      wireScreen = null;
      requestKeyframe();
      notifyReadingChanged();
      return;
    }
    if (!msg.keyframe && !wireScreen) {
      // delta인데 기준 화면이 없다 — 재동기화 요청 (P5c RequestKeyframe)
      requestKeyframe();
      return;
    }
    if (msg.keyframe) keyframeRequested = false;
    const next = msg.keyframe || wireScreen.cols !== msg.cols || wireScreen.rows !== msg.rows
      ? { cols: msg.cols, rows: msg.rows, lines: new Array(msg.rows).fill(null) }
      : { ...wireScreen, lines: wireScreen.lines.slice() };
    for (const line of msg.lines || []) {
      if (line.row < next.rows) next.lines[line.row] = line.runs || [];
    }
    next.cursor = msg.cursor || null;
    next.alt = !!msg.alt;
    next.offset = msg.offset | 0;
    next.history = msg.history ? { ...msg.history } : null;
    wireScreen = next;
    if ((!explicit || matching) && next.history && next.history.expired) {
      if (matching) {
        pendingHistoryRequest = null;
        waitingForResume = false;
      }
      retainedDisplay = true;
      expiredHistory = true;
      viewer.followingLive = false;
      rememberLocalPosition();
      updateScrollNote();
    } else if (matching) {
      pendingHistoryRequest = null;
      viewer.followingLive = waitingForResume && next.offset === 0;
      retainedDisplay = !viewer.followingLive;
      waitingForResume = false;
      publishScreen(next, true);
    } else if (!explicit) {
      if (!viewer.screen || (!retainedDisplay && pendingHistoryRequest === null && !waitingForResume)) publishScreen(next);
      else inferHistoryExpiration();
    }
    updateScrollNote();
    notifyReadingChanged();
  }

  function requestKeyframe() {
    if (keyframeRequested) return;
    keyframeRequested = true;
    send({ type: 'request_keyframe' });
  }

  // ── 세로 팬 제스처 — 터치/휠을 줄 단위 delta로 바꿔 호스트에 넘긴다(양수 = 과거로).
  // 실제 스크롤백 명령은 쓰기 권한이 있는 셸만 보낸다 — 코어는 제스처만 해석한다.
  let touchGesture = null;

  function pan(lines) {
    if (hooks.pan) hooks.pan(lines);
  }

  function resetPan() {
    touchGesture = null;
    if (hooks.resetPan) hooks.resetPan();
  }

  function updateScrollNote() {
    const note = viewer.scrollNote;
    const text = viewer.offsetText;
    if (!note) return;
    const offset = (viewer.screen && viewer.screen.offset) || 0;
    note.hidden = offset <= 0 && viewer.followingLive && !retainedDisplay && pendingHistoryRequest === null && !expiredHistory;
    if (pendingHistoryRequest !== null) text.textContent = waitingForResume ? '현재 화면 요청 중…' : '기록 화면 요청 중…';
    else if (expiredHistory) text.textContent = '불러온 기록이 만료되었습니다 (로컬 화면 보존)';
    else if (offset > 0) text.textContent = '↑ ' + offset + '줄 위 (과거 열람 중)';
    else if (!viewer.followingLive) text.textContent = '화면 따라가기 일시정지';
    if (viewer.resumeFollowButton) {
      viewer.resumeFollowButton.hidden = offset > 0 && !hooks.resumeHistory;
      viewer.resumeFollowButton.disabled = waitingForResume && pendingHistoryRequest !== null;
    }
  }

  const atHistoryEdge = delta => delta > 0 ? viewer.wrap.scrollTop <= 1
    : viewer.wrap.scrollTop + viewer.wrap.clientHeight >= viewer.wrap.scrollHeight - 1;
  viewer.wrap.addEventListener('touchstart', (e) => {
    touchGesture = e.touches.length === 1 ? {
      x: e.touches[0].clientX, y: e.touches[0].clientY,
      lastY: e.touches[0].clientY, axis: null,
    } : null;
  }, { passive: true });
  viewer.wrap.addEventListener('touchmove', (e) => {
    if (window.visualViewport && window.visualViewport.scale > 1.01) {
      touchGesture = null;
      return; // native pan while zoomed
    }
    if (e.touches.length !== 1) { touchGesture = null; return; }
    if (!touchGesture) return;
    const y = e.touches[0].clientY;
    const dx = e.touches[0].clientX - touchGesture.x;
    const totalY = y - touchGesture.y;
    let dy = y - touchGesture.lastY;
    touchGesture.lastY = y;
    if (!touchGesture.axis) {
      if (Math.max(Math.abs(dx), Math.abs(totalY)) < 8) return;
      touchGesture.axis = Math.abs(totalY) > Math.abs(dx) * 1.25 ? 'vertical' : 'horizontal';
      dy = totalY; // retain distance accumulated before the direction/threshold settled
    }
    if (touchGesture.axis !== 'vertical') return;
    if (!dy || getSelectedText() || (!prefs.overview && !atHistoryEdge(dy)) || !hooks.pan) return;
    e.preventDefault(); // viewport edge → connection-local history supplied by host
    // 손가락을 아래로 끌면(dy>0) 과거로 — 콘텐츠가 손가락을 따라온다.
    pan(dy / (viewer.cellH || 16));
  }, { passive: false });
  viewer.wrap.addEventListener('touchend', () => { touchGesture = null; }, { passive: true });
  viewer.wrap.addEventListener('touchcancel', () => { touchGesture = null; }, { passive: true });
  viewer.wrap.addEventListener('wheel', (e) => {
    if (e.ctrlKey) return; // preserve browser pinch zoom
    if (window.visualViewport && window.visualViewport.scale > 1.01) return;
    if (!e.deltaY || Math.abs(e.deltaX) > Math.abs(e.deltaY) || getSelectedText() || !hooks.pan) return;
    const pixels = e.deltaY * (e.deltaMode === 1 ? (viewer.cellH || 16)
      : e.deltaMode === 2 ? viewer.wrap.clientHeight : 1);
    if (!prefs.overview && !atHistoryEdge(-pixels)) return;
    e.preventDefault();
    // 휠 위(deltaY<0) = 과거로(양수 delta).
    pan(-pixels / (viewer.cellH || 16));
  }, { passive: false });

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
      notifyLayoutChanged();
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
    const measured = measureCells();
    const scale = prefs.overview ? Math.min(1,
      availableWidth / (screen.cols * measured.cellWidth),
      availableHeight / (screen.rows * measured.cellHeight),
    ) : 1;
    const cellW = measured.cellWidth * scale;
    const cellH = measured.cellHeight * scale;
    const fontSize = prefs.fontSize * scale;
    cellMetrics = { cellWidth: cellW, cellHeight: cellH, fontSize };
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
      cellW.toFixed(4),
      cellH.toFixed(4),
    ].join(':');
    canvas.style.width = cssWidth + 'px';
    canvas.style.height = cssHeight + 'px';
    renderTextLayer(screen, cssWidth, cssHeight, cellW, cellH, fontSize);
    if (restoreLocalPosition && !prefs.overview) {
      // New dimensions must be installed before the browser can restore an overflow position.
      const position = localPositions.get(positionKey(viewer.watching)) || { left: 0, top: 0 };
      setLocalPosition(position.left, position.top);
      restoreLocalPosition = false;
    }
    followLiveOutput(screen, cellW, cellH);
    paintSearchMarks();
    if (renderKey === lastViewerRenderKey) return;
    lastViewerRenderKey = renderKey;
    if (prefs.readableWrap) return;
    const pixelWidth = Math.max(1, Math.round(cssWidth * dpr));
    const pixelHeight = Math.max(1, Math.round(cssHeight * dpr));
    if (canvas.width !== pixelWidth) canvas.width = pixelWidth;
    if (canvas.height !== pixelHeight) canvas.height = pixelHeight;
    const ctx = canvas.getContext('2d');
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.fillStyle = '#000000';
    ctx.fillRect(0, 0, cssWidth, cssHeight);
    const A_BOLD = 1;
    const A_ITALIC = 2;
    const A_UNDERLINE = 4;
    const A_STRIKE = 8;
    const A_DIM = 16;
    const fontPx = fontSize.toFixed(2);
    const fontFor = (attrs) => {
      const style = attrs & A_ITALIC ? 'italic ' : '';
      const weight = attrs & A_BOLD ? '700 ' : '';
      return style + weight + fontPx + 'px ' + FONT_FAMILY;
    };
    ctx.font = fontFor(0);
    ctx.textBaseline = 'middle';
    for (let row = 0; row < screen.rows; row++) {
      const runs = screen.lines[row];
      if (!runs) continue;
      const y = row * cellH;
      for (const run of runs) {
        const advance = run.w ? cellW * 2 : cellW;
        // g preserves one entry per terminal owner cell, including combining scalars.
        const chars = Array.isArray(run.g) && run.g.every(text => typeof text === 'string' && text.length > 0)
          ? run.g : Array.from(run.t || '');
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
    if (cursor && cursor.visible && cursor.col >= 0 && cursor.col < screen.cols
        && cursor.row >= 0 && cursor.row < screen.rows) {
      const { col, span } = cursorCells(screen, cursor);
      const thickness = Math.max(1, Math.min(2, fontSize / 10));
      const x = col * cellW;
      const y = cursor.row * cellH;
      ctx.fillStyle = 'rgba(212, 212, 212, 0.45)';
      if (cursor.shape === 'beam') ctx.fillRect(x, y, Math.min(thickness, cellW), cellH);
      else if (cursor.shape === 'underline') ctx.fillRect(x, y + cellH - Math.min(thickness, cellH), span * cellW, Math.min(thickness, cellH));
      else ctx.fillRect(x, y, span * cellW, cellH);
    }
  }

  function dimmed(hex) {
    const match = /^#?([0-9a-f]{6})$/i.exec(hex || '');
    if (!match) return hex;
    const value = parseInt(match[1], 16);
    const fade = channel => Math.round(channel * 0.6);
    return `rgb(${fade((value >> 16) & 255)},${fade((value >> 8) & 255)},${fade(value & 255)})`;
  }

  function cursorCells(screen, cursor) {
    for (const run of screen.lines[cursor.row] || []) {
      if (!run.w) continue;
      const owners = Array.isArray(run.g) && run.g.every(text => typeof text === 'string' && text.length > 0)
        ? run.g : Array.from(run.t || '');
      if (cursor.col >= run.s && cursor.col < run.s + owners.length * 2) {
        return { col: run.s + Math.floor((cursor.col - run.s) / 2) * 2, span: 2 };
      }
    }
    return { col: cursor.col, span: 1 };
  }

  function resetTextLayer() {
    if (selectionTouchesText()) window.getSelection().removeAllRanges();
    textLayer.replaceChildren(searchMarks);
    textRows.length = 0;
    textRowKeys.length = 0;
    clearSearch();
  }

  function selectionPoint(node, offset, end) {
    if (node === textLayer) {
      const row = textRows[Math.min(textRows.length - 1, end ? Math.max(0, offset - 1) : offset)];
      return row ? ownerPoint(row, end ? row.textContent.length : 0, end) : null;
    }
    const row = (node.nodeType === Node.ELEMENT_NODE ? node : node.parentElement).closest('.viewer-text-row');
    if (!row || !textLayer.contains(row)) return null;
    const range = document.createRange();
    range.setStart(row, 0); range.setEnd(node, offset);
    return ownerPoint(row, range.toString().length, end);
  }

  function ownerPoint(row, offset, end) {
    if (offset === row.textContent.length) return { row: Number(row.dataset.row), boundary: 'end' };
    for (const cell of row.children) {
      const length = cell.textContent.length;
      if (offset < length || (end && offset <= length)) {
        const spaces = /^ +$/.test(cell.textContent) && length === Number(cell.dataset.span);
        return { row: Number(row.dataset.row), col: Number(cell.dataset.col) + (spaces ? offset : 0),
          inner: spaces ? 0 : offset };
      }
      offset -= length;
    }
    return null;
  }

  function textPoint(point) {
    const row = textRows[point.row];
    if (row && point.boundary === 'end') return [row, row.childNodes.length];
    if (row && point.col !== undefined) {
      for (const cell of row.children) {
        const col = Number(cell.dataset.col), span = Number(cell.dataset.span);
        if (point.col < col || point.col > col + span
            || (point.col === col + span && cell !== row.lastElementChild)) continue;
        const spaces = /^ +$/.test(cell.textContent) && cell.textContent.length === span;
        const offset = spaces ? point.col - col + point.inner : point.inner;
        return offset <= cell.textContent.length ? [cell.firstChild, offset] : null;
      }
      return null;
    }
    if (!row || point.offset > row.textContent.length) return null;
    const walker = document.createTreeWalker(row, NodeFilter.SHOW_TEXT);
    let remaining = point.offset;
    for (let node = walker.nextNode(); node; node = walker.nextNode()) {
      if (remaining <= node.length) return [node, remaining];
      remaining -= node.length;
    }
    return [row, 0];
  }

  function textRange(start, end) {
    const first = textPoint(start), last = textPoint(end);
    if (!first || !last) return null;
    const range = document.createRange();
    range.setStart(...first); range.setEnd(...last);
    return range;
  }

  function renderTextLayer(screen, width, height, cellW, cellH, fontSize) {
    const selection = window.getSelection();
    const copied = getSelectedText();
    const selectedRange = copied && selection.getRangeAt(0);
    const start = selectedRange && selectionPoint(selectedRange.startContainer, selectedRange.startOffset, false);
    const end = selectedRange && selectionPoint(selectedRange.endContainer, selectedRange.endOffset, true);
    let changedSelection = false;
    textLayer.style.setProperty('--viewer-cell-width', cellW + 'px');
    textLayer.style.setProperty('--viewer-cell-height', cellH + 'px');
    textLayer.style.font = fontSize + 'px ' + FONT_FAMILY;
    textLayer.style.width = prefs.readableWrap ? '100%' : width + 'px';
    textLayer.style.height = prefs.readableWrap ? 'auto' : height + 'px';
    for (let index = 0; index < screen.rows; index++) {
      const runs = screen.lines[index] || [];
      const key = JSON.stringify([screen.cols, runs]);
      if (key === textRowKeys[index]) continue;
      if (start && end && index >= start.row && index <= end.row) changedSelection = true;
      let row = textRows[index];
      if (!row) {
        row = document.createElement('div'); row.className = 'viewer-text-row'; row.dataset.row = index;
        textRows[index] = row; textLayer.insertBefore(row, searchMarks);
      }
      const cells = [];
      const cell = (text, span, attrs = 0, fg = '#d4d4d4', bg = '#000000', padding = false) => {
        const node = document.createElement('span');
        node.className = 'viewer-text-cell' + (padding ? ' viewer-text-padding' : '');
        node.textContent = text;
        node.dataset.col = next;
        node.dataset.span = span;
        node.style.width = `calc(var(--viewer-cell-width) * ${span})`;
        node.style.color = attrs & 16 ? dimmed(fg) : fg;
        node.style.backgroundColor = bg;
        if (attrs & 1) node.style.fontWeight = '700';
        if (attrs & 2) node.style.fontStyle = 'italic';
        node.style.textDecoration = [attrs & 4 ? 'underline' : '', attrs & 8 ? 'line-through' : ''].filter(Boolean).join(' ');
        cells.push(node);
      };
      let next = 0;
      for (const run of runs) {
        const owners = Array.isArray(run.g) && run.g.every(text => typeof text === 'string' && text.length > 0)
          ? run.g : Array.from(run.t || '');
        const span = run.w ? 2 : 1;
        if (run.s > next) {
          const gap = Math.min(screen.cols, run.s) - next;
          if (gap > 0) cell(' '.repeat(gap), gap);
          next += gap;
        }
        for (let owner = 0; owner < owners.length; owner++) {
          const col = run.s + owner * span;
          if (col < next || col >= screen.cols) continue;
          cell(owners[owner], Math.min(span, screen.cols - col), run.a || 0, run.fg, run.bg);
          next = col + span;
        }
      }
      if (next < screen.cols) cell(' '.repeat(screen.cols - next), screen.cols - next, 0, undefined, undefined, true);
      row.replaceChildren(...cells);
      textRowKeys[index] = key;
    }
    while (textRows.length > screen.rows) textRows.pop().remove();
    textRowKeys.length = screen.rows;
    if (copied && start && end && (changedSelection || end.row >= screen.rows)) {
      const range = textRange(start, end);
      selection.removeAllRanges();
      // Never silently turn copied text into different output when a selected row changes.
      if (range) selection.addRange(range);
      if (getSelectedText() !== copied) selection.removeAllRanges();
    }
    refreshSearch();
  }

  function searchResult() {
    return { query: searchQuery, total: searchMatches.length, index: searchIndex + 1, scope: 'loaded viewport' };
  }

  function refreshSearch() {
    if (!searchQuery) return;
    const current = searchMatches[searchIndex];
    const pattern = new RegExp(searchQuery.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'), 'giu');
    searchMatches = [];
    for (let row = 0; row < textRows.length; row++) {
      for (const match of textRows[row].textContent.matchAll(pattern)) {
        searchMatches.push({ row, offset: match.index, end: match.index + match[0].length });
      }
    }
    const retained = current && searchMatches.findIndex(match => match.row === current.row
      && match.offset === current.offset && match.end === current.end);
    searchIndex = retained >= 0 ? retained : Math.min(Math.max(0, searchIndex), searchMatches.length - 1);
    if (hooks.searchChanged) hooks.searchChanged(searchResult());
  }

  function currentSearchRange() {
    const match = searchMatches[searchIndex];
    return match ? textRange({ row: match.row, offset: match.offset }, { row: match.row, offset: match.end }) : null;
  }

  function paintSearchMarks() {
    const range = currentSearchRange();
    const bounds = textLayer.getBoundingClientRect();
    const marks = [];
    if (range) for (const rect of range.getClientRects()) {
      if (!rect.width || !rect.height) continue;
      const mark = document.createElement('span'); mark.className = 'viewer-search-mark';
      mark.style.left = rect.left - bounds.left + 'px'; mark.style.top = rect.top - bounds.top + 'px';
      mark.style.width = rect.width + 'px'; mark.style.height = rect.height + 'px'; marks.push(mark);
    }
    searchMarks.replaceChildren(...marks);
  }

  function findText(query, direction = 1) {
    if (!viewer.privacy.hidden) { clearSearch(); return searchResult(); }
    query = String(query || '');
    const same = query === searchQuery;
    searchQuery = query;
    if (!query) { clearSearch(); return searchResult(); }
    stopFollowing();
    if (!same) { searchMatches = []; searchIndex = -1; }
    refreshSearch();
    if (searchMatches.length) {
      searchIndex = same ? (searchIndex + (direction < 0 ? -1 : 1) + searchMatches.length) % searchMatches.length
        : direction < 0 ? searchMatches.length - 1 : 0;
      const range = currentSearchRange();
      const rect = range.getClientRects()[0];
      if (rect) {
        const wrap = viewer.wrap.getBoundingClientRect();
        const left = viewer.wrap.scrollLeft + rect.left - wrap.left;
        const top = viewer.wrap.scrollTop + rect.top - wrap.top;
        setLocalPosition(prefs.readableWrap ? 0 : Math.max(0, left), Math.max(0, top));
        rememberLocalPosition();
      }
    }
    paintSearchMarks();
    const result = searchResult();
    if (hooks.searchChanged) hooks.searchChanged(result);
    return result;
  }

  function clearSearch() {
    searchQuery = ''; searchMatches = []; searchIndex = -1;
    searchMarks.replaceChildren();
    if (hooks.searchChanged) hooks.searchChanged(searchResult());
  }

  function followLiveOutput(screen, cellW, cellH) {
    if (!viewer.followingLive || retainedDisplay || prefs.overview || screen.offset > 0 || getSelectedText()) return;
    if (prefs.readableWrap) {
      const last = textRows.findLast(row => row.textContent.trim());
      if (last) setLocalPosition(0, Math.max(0, last.offsetTop + last.offsetHeight - viewer.wrap.clientHeight));
      rememberLocalPosition();
      return;
    }
    const cursor = screen.cursor;
    let row = cursor && cursor.visible ? cursor.row : -1;
    if (row < 0) {
      for (let index = screen.rows - 1; index >= 0; index--) {
        if ((screen.lines[index] || []).some(run => (run.t || (run.g || []).join('')).trim())) {
          row = index;
          break;
        }
      }
    }
    if (row < 0 || row >= screen.rows) return;
    const reveal = (start, end, position, size) => start < position ? start
      : end > position + size ? Math.max(0, end - size) : position;
    const top = reveal(row * cellH, (row + 1) * cellH, viewer.wrap.scrollTop, viewer.wrap.clientHeight);
    const { col, span } = cursor && cursor.visible ? cursorCells(screen, cursor) : { col: -1, span: 1 };
    const left = col >= 0 && col < screen.cols
      ? reveal(col * cellW, (col + span) * cellW, viewer.wrap.scrollLeft, viewer.wrap.clientWidth)
      : viewer.wrap.scrollLeft;
    // A font or stage change can clamp native scroll even when the cursor is already visible.
    setLocalPosition(left, top);
    rememberLocalPosition();
  }

  function notifyLayoutChanged() {
    if (!viewer.watching || viewer.el.hidden) return;
    const metrics = {
      ...getCellMetrics(),
      stageWidth: viewer.wrap.clientWidth,
      stageHeight: viewer.wrap.clientHeight,
      configuredFontSize: prefs.fontSize,
    };
    const key = JSON.stringify(metrics);
    if (key === lastLayoutKey) return;
    lastLayoutKey = key;
    if (hooks.layoutChanged) hooks.layoutChanged(metrics);
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
  if (document.fonts) {
    document.fonts.ready.then(() => { measuredFontSize = 0; scheduleViewerRender(); });
  }

  function scheduleViewerRenderForLayoutChange(previousWrapHeight) {
    if (viewer.wrap.clientHeight !== previousWrapHeight) scheduleViewerRender();
  }

  clearStaleViewerHistory();

  return Object.assign(viewer, {
    settings,
    setFontSize,
    setOverview,
    setReadableWrap,
    getCellMetrics,
    getSelectedText,
    getHistoryState,
    expectHistoryWindow,
    cancelHistoryWindow,
    findText,
    clearSearch,
    resumeFollow,
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
