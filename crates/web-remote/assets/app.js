// Deppy Sijo 모바일 대시보드 (P2) — 프레임워크 없음, CSP(default-src 'self') 준수.
// WS로 승인/상태를 받아 렌더하고, Allow/Deny를 되보낸다. 신뢰경계: 서버가 보낸 문자열
// (preview/title/server/tool)은 전부 textContent로만 삽입한다 — innerHTML 절대 금지.
//
// 서빙되는 `/app.js`는 `web/shared/viewer-core.js` + 이 파일을 이어 붙인 **하나의 ES
// 모듈**이다(static_srv가 합친다 — index.html은 type="module"로 로드). 그래서 여기서
// `createViewer`를 import 없이 바로 쓴다. 읽기 전용 전체화면 뷰어 수명주기는 그 공용
// 모듈이 소유하고, 이 셸은 작성기·특수키·첨부·승인 같은 쓰기 경로만 hooks로 얹는다.
(() => {
  'use strict';
  const TOKEN_KEY = 'deppy.webToken';
  const LAST_DASHBOARD_KEY = 'deppy.lastDashboardAt';
  const IOS_HINT_KEY = 'deppy.iosHintDismissed';
  const view = document.body.dataset.view;
  const params = new URLSearchParams(location.search);
  const urlToken = params.get('token');

  if (view === 'pairing') {
    const saved = localStorage.getItem(TOKEN_KEY);
    if (!urlToken && saved) {
      // 재방문(URL에 토큰 없음) — 저장된 페어링으로 자동 복구.
      // **기존 쿼리를 보존한다**: 알림 딥링크(/?watch=N)로 콜드 스타트하면 토큰이 없어
      // 이 페이지가 뜨는데, 토큰만 붙여 리다이렉트하면 watch가 유실돼 자동 시청이
      // 통째로 불능이었다 (리뷰 P1-1 — P6c의 주 시나리오).
      const next = new URLSearchParams(location.search);
      next.set('token', saved);
      location.replace('/?' + next.toString());
    } else if (urlToken) {
      // 토큰을 제시했는데도 401 — 재발급 등으로 무효. 저장분을 폐기해 리다이렉트 루프를 막는다.
      localStorage.removeItem(TOKEN_KEY);
    }
    return;
  }

  // offline 뷰 — SW가 오프라인 네비게이션 폴백으로 서빙한다. 마지막 dashboard 수신 시각을
  // 보여주고 재시도(네트워크 복귀 시 '/'로)를 제공한다. 값은 textContent로만 렌더한다.
  if (view === 'offline') {
    const lastSeen = document.getElementById('last-seen');
    if (lastSeen) {
      const at = Number(localStorage.getItem(LAST_DASHBOARD_KEY) || 0);
      lastSeen.textContent = at
        ? '마지막 상태 수신: ' + new Date(at).toLocaleString('ko-KR')
        : '마지막 상태 수신 기록이 없습니다.';
    }
    const retry = document.getElementById('retry-btn');
    if (retry) retry.addEventListener('click', () => location.replace('/'));
    return;
  }

  // shell 뷰 — 토큰을 저장하고 주소창/히스토리에서 제거(위생). 재방문 복구는 401 페이지가 한다.
  if (urlToken) {
    localStorage.setItem(TOKEN_KEY, urlToken);
    history.replaceState(null, '', location.pathname);
  }

  if ('serviceWorker' in navigator) {
    navigator.serviceWorker.register('/sw.js').catch(() => {});
  }

  maybeShowIosInstallHint();
  maybeShowNotifyButton();

  const dot = document.getElementById('dot');
  const statusText = document.getElementById('status-text');
  const approvalsEl = document.getElementById('approvals');
  const approvalsEmpty = document.getElementById('approvals-empty');
  const approvalsCount = document.getElementById('approvals-count');
  const sessionsEl = document.getElementById('sessions');
  const sessionsEmpty = document.getElementById('sessions-empty');
  const resourceEl = document.getElementById('resource');
  const noticeBanner = document.getElementById('notice-banner');
  const workspaceCount = document.getElementById('workspace-count');
  const dashboardMenuStatus = document.getElementById('dashboard-menu-status');
  const approvalToggle = document.getElementById('approval-toggle');
  const approvalPanel = document.getElementById('approval-panel');
  const dashboardApprovalsButton = document.getElementById('dashboard-approvals-button');
  const viewerApprovalsButton = document.getElementById('viewer-menu-approvals');
  const viewerTitle = document.getElementById('viewer-title');
  const viewerSessionChip = document.getElementById('viewer-session-chip');
  const viewerQuickActions = document.getElementById('viewer-quick-actions');
  const viewerMenuStatus = document.getElementById('viewer-menu-status');
  const viewerMenu = document.getElementById('viewer-menu');
  const viewerStage = document.getElementById('viewer-stage');
  const viewerFontSmaller = document.getElementById('viewer-font-smaller');
  const viewerFontLarger = document.getElementById('viewer-font-larger');
  const viewerFontSize = document.getElementById('viewer-font-size');
  const viewerOverview = document.getElementById('viewer-overview');
  const viewerOverviewState = document.getElementById('viewer-overview-state');
  const viewerResizeControl = document.getElementById('viewer-resize-control');
  const viewerResizeStatus = document.getElementById('viewer-resize-status');
  const viewerReadableWrap = document.getElementById('viewer-readable-wrap');
  const viewerReadableState = document.getElementById('viewer-readable-state');
  const viewerSearchText = document.getElementById('viewer-search-text');
  const viewerSearchCount = document.getElementById('viewer-search-count');
  const viewerCopyStatus = document.getElementById('viewer-copy-status');
  const viewerReadingStatus = document.getElementById('viewer-reading-status');

  function attachMenu(buttonId, panelId) {
    const button = document.getElementById(buttonId);
    const panel = document.getElementById(panelId);
    button.addEventListener('click', () => {
      panel.hidden = !panel.hidden;
      button.setAttribute('aria-expanded', String(!panel.hidden));
    });
    document.addEventListener('click', (event) => {
      if (panel.hidden || panel.contains(event.target) || button.contains(event.target)) return;
      panel.hidden = true;
      button.setAttribute('aria-expanded', 'false');
    });
    return () => {
      panel.hidden = true;
      button.setAttribute('aria-expanded', 'false');
    };
  }

  const closeDashboardMenu = attachMenu('dashboard-menu-button', 'dashboard-menu');
  const sheetClosers = [];
  function attachSheet(buttonId, panelId, closeId) {
    const trigger = document.getElementById(buttonId);
    const panel = document.getElementById(panelId);
    const dialog = panel.querySelector('[role="dialog"]');
    let restoreFocus = null;
    let restoreInert = [];
    const close = (restore = true) => {
      if (panel.hidden) return;
      panel.hidden = true;
      trigger.setAttribute('aria-expanded', 'false');
      for (const [element, inert] of restoreInert) element.inert = inert;
      restoreInert = [];
      if (restore && restoreFocus?.isConnected) restoreFocus.focus({ preventScroll: true });
    };
    const open = () => {
      for (const closer of sheetClosers) closer(false);
      restoreFocus = trigger;
      document.activeElement?.blur?.();
      restoreInert = Array.from(panel.parentElement.children).filter(child => child !== panel)
        .map(child => [child, child.inert]);
      for (const [element] of restoreInert) element.inert = true;
      panel.hidden = false;
      trigger.setAttribute('aria-expanded', 'true');
      document.getElementById(closeId).focus({ preventScroll: true });
    };
    trigger.addEventListener('click', () => panel.hidden ? open() : close());
    document.getElementById(closeId).addEventListener('click', () => close());
    panel.addEventListener('click', event => { if (event.target === panel) close(); });
    panel.addEventListener('keydown', event => {
      if (event.key === 'Escape') { event.preventDefault(); event.stopPropagation(); close(); }
      if (event.key !== 'Tab') return;
      const focusable = Array.from(dialog.querySelectorAll('button,input,[tabindex="0"]'))
        .filter(element => !element.disabled && !element.hidden && element.getClientRects().length);
      const first = focusable[0], last = focusable[focusable.length - 1];
      if (event.shiftKey && document.activeElement === first) { event.preventDefault(); last?.focus(); }
      else if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first?.focus(); }
    });
    sheetClosers.push(close);
    return close;
  }
  const closeViewerMenu = attachSheet('viewer-menu-button', 'viewer-menu', 'viewer-menu-close');
  const closeSessionSheet = attachSheet('viewer-session-chip', 'viewer-session-sheet', 'viewer-session-close');
  function showApprovals() {
    approvalPanel.hidden = !approvalPanel.hidden;
    closeDashboardMenu();
    if (!viewer.el.hidden) requestCloseViewer();
  }
  approvalToggle.addEventListener('click', showApprovals);
  dashboardApprovalsButton.addEventListener('click', showApprovals);
  viewerApprovalsButton.addEventListener('click', () => {
    closeViewerMenu();
    approvalPanel.hidden = false;
    requestCloseViewer();
  });
  function toggleQuickActions() {
    const previousWrapHeight = viewer.wrap.clientHeight;
    viewerQuickActions.hidden = !viewerQuickActions.hidden;
    document.getElementById('input-keys-help').hidden = viewerQuickActions.hidden;
    document.getElementById('viewer-quick-button').setAttribute('aria-expanded', String(!viewerQuickActions.hidden));
    closeViewerMenu();
    scheduleViewerRenderForLayoutChange(previousWrapHeight);
  }
  document.getElementById('viewer-quick-button').addEventListener('click', toggleQuickActions);
  document.getElementById('viewer-menu-keys').addEventListener('click', toggleQuickActions);

  const token = localStorage.getItem(TOKEN_KEY);

  // 권한이 이미 허용돼 있으면(재방문) 구독을 보장한다 — 브라우저가 구독을 회전/삭제했거나
  // 서버 재시작으로 구독이 비었을 수 있다. 서버 등록은 endpoint upsert라 반복해도 안전하다.
  if (token && 'Notification' in window && Notification.permission === 'granted') {
    enablePush(token);
  }

  // 서버와 합의한 WS 프로토콜 버전 — welcome에서 대조한다(불일치 = 셸이 낡음).
  // v3: 워크스페이스 상태 값 "idle"→"suspended"(I1b-1) — 옛 캐시 셸을 재로드시킨다.
  const PROTOCOL_VERSION = 5;

  const STATUS_LABEL = {
    running: '실행 중',
    waiting: '입력 대기',
    needs_approval: '승인 필요',
    idle: '유휴',
    error: '오류',
    done: '완료',
  };

  let ws = null;
  let reconnectDelay = 1000;
  let reconnectTimer = null;
  let manualClose = false;
  const intentionallyClosedSockets = new WeakSet();

  function isCurrentSocket(socket) {
    return ws === socket && !intentionallyClosedSockets.has(socket);
  }

  function setStatus(cls, text) {
    dot.className = 'dot ' + cls;
    statusText.textContent = text;
    dashboardMenuStatus.textContent = text;
  }

  function wsUrl() {
    const scheme = location.protocol === 'https:' ? 'wss://' : 'ws://';
    return scheme + location.host + '/ws';
  }

  function connect() {
    if (!token) {
      setStatus('bad', '토큰 없음 — 재페어링 필요');
      return;
    }
    if (ws && (ws.readyState === WebSocket.OPEN || ws.readyState === WebSocket.CONNECTING)) {
      return;
    }
    manualClose = false;
    setStatus('', '연결 중…');
    setViewerConnection('connecting');
    let socket;
    try {
      socket = new WebSocket(wsUrl());
    } catch (e) {
      scheduleReconnect();
      return;
    }
    ws = socket;

    socket.addEventListener('open', () => {
      if (!isCurrentSocket(socket)) return;
      reconnectDelay = 1000;
      socket.send(JSON.stringify({ type: 'auth', v: PROTOCOL_VERSION, token }));
    });

    socket.addEventListener('message', (event) => {
      if (!isCurrentSocket(socket)) return;
      let msg;
      try {
        msg = JSON.parse(event.data);
      } catch {
        return;
      }
      handleMessage(msg);
    });

    socket.addEventListener('close', () => {
      const wasCurrent = ws === socket;
      if (wasCurrent) ws = null;
      if (!wasCurrent || intentionallyClosedSockets.has(socket)) return;
      setStatus('bad', '연결 끊김 — 재연결 중…');
      setViewerConnection('reconnecting');
      scheduleReconnect();
    });

    socket.addEventListener('error', () => {
      // close 이벤트가 뒤따른다 — 거기서 재연결한다.
    });
  }

  function scheduleReconnect() {
    if (manualClose || reconnectTimer) return;
    reconnectTimer = setTimeout(() => {
      reconnectTimer = null;
      connect();
    }, reconnectDelay);
    reconnectDelay = Math.min(reconnectDelay * 2, 15000);
  }

  function disconnect() {
    retireResizeControl();
    retireHistory();
    manualClose = true;
    if (reconnectTimer) {
      clearTimeout(reconnectTimer);
      reconnectTimer = null;
    }
    if (ws) {
      const socket = ws;
      ws = null;
      intentionallyClosedSockets.add(socket);
      try { socket.close(); } catch {}
    }
  }

  function handleMessage(msg) {
    switch (msg && msg.type) {
      case 'welcome':
        // 서버 프로토콜이 이 셸보다 새로우면(배포 후 오래 열려 있던 페이지) 스스로 재로드한다.
        // HTML은 network-first + 자산은 내용 해시 경로라, 재로드하면 반드시 짝이 맞는다.
        if (typeof msg.v === 'number' && msg.v !== PROTOCOL_VERSION) {
          setStatus('', '새 버전 — 다시 불러오는 중…');
          disconnect();
          location.reload();
          return;
        }
        setStatus('ok', '연결됨');
        setViewerConnection('connected');
        // 재연결이면 서버 접속 상태(시청)가 초기화됐다 — 뷰어 코어가 다시 watch한다.
        rewatch();
        break;
      case 'dashboard':
        // 오프라인 폴백 화면이 "마지막 상태 시각"을 보여줄 수 있게 수신 시각을 저장한다.
        localStorage.setItem(LAST_DASHBOARD_KEY, String(Date.now()));
        renderWorkspaces(msg.workspaces || [], msg.resource || null);
        showNotice(msg.notice || null); // 미러 진입 안내(I1b-2)
        break;
      case 'approvals':
        renderApprovals(msg.pending || []);
        break;
      case 'viewport':
        historyCompleting = historyPending && historyTargetCurrent(historyPending)
          && msg.session === historyPending.session && msg.history?.request === historyPending.request
          ? { context: historyPending, success: !msg.history.expired } : null;
        handleViewport(msg);
        historyCompleting = null;
        reevaluateResizeReading();
        break;
      case 'history_error':
        handleHistoryError(msg);
        break;
      case 'terminal_control':
        handleTerminalControl(msg);
        break;
      case 'input_pressure':
        // PTY 입력 큐 압박/거부 — 사유별로 다르게 다룬다 (리뷰 P2-2, P3-1).
        if (!msg.session) break;
        if (msg.session !== viewer.watching) {
          if (msg.reason === 'closed' || msg.reason === 'unavailable') {
            rememberTerminalInputLock(msg.session); // remember non-current terminal lock
          }
          if (msg.reason === 'queue_full' && (msg.queued || 0) === 0) {
            recentSentBySession.delete(msg.session); // stale pressure resolved
            scheduleRecentSentExpiry();
          } else restoreDraft('', msg.session);
          break;
        }
        handleInputPressure(msg);
        break;
      case 'error':
        setStatus('bad', '오류: ' + (msg.message || ''));
        break;
    }
  }

  const utf8Encoder = new TextEncoder();
  const MAX_INPUT_BYTES = 256 * 1024;
  const MAX_INPUT_FRAME_BYTES = 512 * 1024;

  function sendSerialized(serialized) {
    if (!ws || ws.readyState !== WebSocket.OPEN) return false;
    try { ws.send(serialized); return true; } catch (_) { return false; }
  }

  function send(msg) {
    return sendSerialized(JSON.stringify(msg));
  }

  // ── 터미널 뷰어 (P5d) — 읽기 전용 canvas + 최소 제어(Ctrl-C/Enter) ──
  // 서버 프레임(P5c): keyframe=전체 행, delta=바뀐 행만. 읽기 전용 수명주기(화면 모델·
  // canvas 렌더·팬/핀치·Back·프라이버시 커튼·재연결 표시)는 공용 모듈
  // web/shared/viewer-core.js가 소유한다. 이 셸은 그 위에 쓰기 경로(특수키·작성기·첨부)만
  // hooks로 얹는다 — 시청 전용 Relay 셸이 같은 코어를 hooks 없이 쓸 수 있어야 한다.
  const dashboardShell = document.getElementById('dashboard-shell');
  const sessionsTitle = document.getElementById('sessions-title');

  const keyRepeatCancels = [];

  function stopAllKeyRepeats() {
    for (const cancel of keyRepeatCancels) cancel();
  }

  function restoreViewerFocus(sessionId) {
    queueMicrotask(() => {
      const button = Array.from(document.querySelectorAll('.view-btn'))
        .find((item) => item.dataset.sessionId === sessionId);
      (button || sessionsTitle).focus();
    });
  }

  const viewer = createViewer({
    send,
    backdrop: dashboardShell,
    hooks: {
      closingChanged: (closing) => {
        if (closing) {
          retireResizeControl();
          retireHistory();
          stopAllKeyRepeats();
          resetInputContext();
        }
        inputBlocked = closing;
        for (const button of viewer.keys) button.disabled = closing;
        updateComposerEnabled();
      },
      connectionChanged: (state, connected) => {
        viewerMenuStatus.textContent = connected ? '연결됨' : (state === 'reconnecting' ? '재연결 중' : '연결 중');
        if (!connected) {
          retireResizeControl();
          retireHistory();
          stopAllKeyRepeats();
          resetInputContext();
          cancelActiveUpload();
          cancelWorkspaceSelection();
          for (const sessionId of Array.from(recentSentBySession.keys())) {
            restoreDraft(
              sessionId === viewer.watching
                ? '연결이 바뀌어 최근 입력을 복원했습니다 — 중복 여부를 확인하세요'
                : '',
              sessionId,
            );
          }
          inputBlocked = false; // reset per connection generation
        }
        updateComposerEnabled();
        syncResizeControl();
        if (connected) consumePendingUploadSelection();
      },
      // 컨트롤/작성기 높이 상한은 이 셸만의 것이다(시청 전용 셸에는 컨트롤이 없다).
      viewportSynced: (width, height) => {
        const controlsMaxHeightPx = Math.max(88, Math.floor(height * 0.45)) + 'px';
        const composerMaxHeightPx = '132px';
        if (viewer.el.style.getPropertyValue('--viewer-controls-max-height')
            !== controlsMaxHeightPx) {
          viewer.el.style.setProperty('--viewer-controls-max-height', controlsMaxHeightPx);
        }
        if (viewer.el.style.getPropertyValue('--viewer-composer-max-height')
            !== composerMaxHeightPx) {
          viewer.el.style.setProperty('--viewer-composer-max-height', composerMaxHeightPx);
        }
        positionViewerMenu(height);
      },
      layoutChanged: (metrics) => {
        syncViewerReadabilityControls();
        positionViewerMenu();
        refreshMobileResizeLayout(metrics);
      },
      historyRowsChanged: queueDesiredHistoryRows,
      historyNavigate: navigateHistory,
      pan: queueScroll,
      resetPan: resetScroll,
      searchChanged: syncViewerSearch,
      readingChanged: handleReadingChanged,
      resumeHistory: () => sendHistoryRequest('reset', 0),
      beforeOpen: () => {
        closeViewerMenu(false);
        closeSessionSheet(false);
        retireResizeControl();
        retireHistory();
        resizeLayout = null;
        stopAllKeyRepeats();
        resetInputContext();
        preserveComposerDraftForTransition();
        cancelActiveUpload();
        cancelPendingUploadSelection();
      },
      sessionChanged: (sessionId) => {
        inputBlocked = false;
        setComposerNote('');
        loadComposerDraft(sessionId);
      },
      afterOpen: () => updateComposerEnabled(),
      beforeRequestClose: () => {
        cancelWorkspaceSelection();
        retireResizeControl();
        retireHistory();
        resetInputContext();
        cancelActiveUpload();
        cancelPendingUploadSelection();
      },
      beforeClose: ({ discardDraft = false } = {}, returnSession) => {
        retireResizeControl();
        retireHistory();
        stopAllKeyRepeats();
        if (discardDraft) discardSessionDraft(returnSession);
        else saveComposerDraft();
        cancelActiveUpload();
        cancelPendingUploadSelection();
      },
      beforeHide: () => {
        retireResizeControl();
        retireHistory();
        resizeLayout = null;
        closeViewerMenu(false);
        closeSessionSheet(false);
        viewerQuickActions.hidden = true;
        document.getElementById('input-keys-help').hidden = true;
        document.getElementById('viewer-quick-button').setAttribute('aria-expanded', 'false');
        composerRecoveryWarningSession = null;
        composerSession = null;
        composerText.value = '';
        autoGrow();
        inputBlocked = false;
        setComposerNote('');
      },
      afterHide: ({ rerender = true, notice = '' } = {}) => {
        if (rerender) renderWorkspaces(lastWorkspaces, lastResource);
        const closeNotices = [notice, draftCacheNotice].filter(Boolean);
        draftCacheNotice = '';
        if (closeNotices.length) showNotice(closeNotices.join(' '));
      },
      afterClose: (options, returnSession) => {
        restoreViewerFocus(returnSession);
        queueMicrotask(() => consumePendingWatch(lastSessions));
      },
    },
  });
  // 특수키 행은 쓰기 경로라 코어가 아니라 이 셸이 소유한다.
  viewer.keys = Array.from(document.querySelectorAll('.viewer-keys button'));
  const {
    openViewer: openCoreViewer,
    requestCloseViewer,
    setViewerConnection,
    setPrivacyCurtain,
    handleViewport,
    rewatch,
    resetPan,
    scheduleViewerRender,
    scheduleViewerRenderForLayoutChange,
  } = viewer;

  // A lease is granted by the runtime, scoped to this socket and watched UUID.
  // Client numbers correlate replies only; they never imply authority.
  const resizeRequests = new WeakMap();
  const RESIZE_REQUEST_MAX = 0xffffffff;
  let resizeControl = null;
  let resizeLayout = null;
  let resizeTimer = null;
  let resizeReadingSuspended = null;

  function clearResizeTimer() {
    clearTimeout(resizeTimer);
    resizeTimer = null;
  }

  function nextResizeRequest(socket) {
    const previous = resizeRequests.get(socket) || 0;
    if (previous >= RESIZE_REQUEST_MAX) return null;
    const request = previous + 1;
    resizeRequests.set(socket, request);
    return request;
  }

  function resizeSocketReady(socket) {
    return !!socket && socket === ws && isCurrentSocket(socket) && socket.readyState === WebSocket.OPEN;
  }

  function resizeContextCurrent(context) {
    return context === resizeControl && context.session === viewer.watching && resizeSocketReady(context.socket);
  }

  function mobileResizeReady() {
    return resizeSocketReady(ws) && !!viewer.watching && !viewer.el.hidden
      && !viewer.closing && !document.hidden && viewer.connection === 'connected';
  }

  function mobileResizeSuspended() {
    const settings = viewer.settings();
    const history = viewer.getHistoryState();
    return settings.overview || settings.readableWrap || !viewer.followingLive
      || !!(viewer.screen && viewer.screen.offset > 0) || history.retained || history.pendingRequest !== null;
  }

  function refreshMobileResizeLayout(metrics = viewer.getCellMetrics()) {
    // Scrollbars depend on the received grid. Use the physical stage so a
    // mobile-size echo removing scrollbars cannot produce another resize.
    resizeLayout = { ...metrics, stageWidth: viewerStage.clientWidth, stageHeight: viewerStage.clientHeight };
    scheduleMobileResize();
  }

  if ('ResizeObserver' in window) {
    new ResizeObserver(() => {
      // A scrollbar can leave the inner wrap at zero while the physical stage recovers.
      if (!resizeLayout || viewerStage.clientWidth !== resizeLayout.stageWidth
          || viewerStage.clientHeight !== resizeLayout.stageHeight) refreshMobileResizeLayout();
    }).observe(viewerStage);
  }

  function reevaluateResizeReading() {
    if (mobileResizeSuspended() !== resizeReadingSuspended) refreshMobileResizeLayout();
  }

  function syncResizeControl() {
    const context = resizeControl;
    const active = context && resizeContextCurrent(context);
    const owned = !!(active && context.owned);
    const pending = active && context.reason === 'pending';
    viewerResizeControl.setAttribute('aria-pressed', String(owned));
    viewerResizeControl.textContent = owned ? '크기 제어 해제'
      : pending ? (context.action === 'acquire' ? '크기 요청 취소' : '해제 확인 중…') : '휴대폰 크기로 맞춤';
    // PTY input pressure does not prevent release or affect lease ownership.
    viewerResizeControl.disabled = !mobileResizeReady() || (pending && context.action === 'release')
      || (!owned && !pending && (terminalInputLockOverflow || terminalInputBlockedSessions.has(viewer.watching)));
    const reasons = {
      in_use: '다른 화면에서 터미널 크기를 제어 중입니다.',
      released: '크기 제어를 해제했습니다. 원격 크기를 그대로 표시합니다.',
      expired: '크기 제어 시간이 끝났습니다. 다시 요청할 수 있습니다.',
      unavailable: '이 터미널에서 크기 제어를 사용할 수 없습니다.',
      stale: '크기 제어가 바뀌었습니다. 다시 요청할 수 있습니다.',
      capacity: '크기 제어 요청이 많습니다. 잠시 후 다시 요청하세요.',
      timeout: '크기 제어를 확인하지 못했습니다. 다시 요청할 수 있습니다.',
      restore_failed: '크기 제어는 해제했지만 원격 화면 크기 복원에 실패했습니다.',
      invalid_size: '요청한 크기를 적용할 수 없습니다. 크기 제어는 유지됩니다.',
      resize_failed: '터미널 크기 변경에 실패했습니다. 크기 제어는 유지됩니다.',
    };
    viewerResizeStatus.textContent = owned && mobileResizeSuspended()
      ? '화면을 읽거나 전체 화면을 맞추는 동안 자동 크기 변경이 일시정지됩니다. 제어를 해제할 수 있습니다.'
      : pending ? (context.action === 'acquire' ? '터미널 크기 제어권을 요청 중입니다…' : '크기 제어 해제를 확인 중입니다…')
      : active && reasons[context.reason] ? reasons[context.reason]
      : owned ? '휴대폰에서 터미널 크기를 제어합니다.' : '원격 터미널 크기를 그대로 표시합니다.';
  }

  function retireResizeControl() {
    const previous = resizeControl;
    resizeControl = null;
    clearResizeTimer();
    if (previous && previous.action === 'acquire' && previous.acceptGrant
        && previous.socket.readyState === WebSocket.OPEN) {
      const request = nextResizeRequest(previous.socket);
      if (request !== null) {
        previous.socket.send(JSON.stringify({ type: 'resize_control', session: previous.session, action: 'release', request }));
      } else if (previous.socket === ws) disconnect(); // Exhaustion closes the lease; never wrap to zero.
    }
    syncResizeControl();
  }

  function requestResizeControl(action) {
    if (!mobileResizeReady()) return;
    if (action === 'acquire' && (terminalInputLockOverflow || terminalInputBlockedSessions.has(viewer.watching))) return;
    const socket = ws;
    const session = viewer.watching;
    clearResizeTimer();
    resizeControl = null; // Revoke permission before a new action or any synchronous reply.
    const request = nextResizeRequest(socket);
    if (request === null) {
      disconnect();
      setViewerConnection('connecting');
      connect();
      return; // A fresh socket still requires a fresh explicit click.
    }
    resizeControl = { socket, session, request, action, owned: false, reason: 'pending',
      acceptGrant: action === 'acquire', lastGeometry: null };
    syncResizeControl();
    socket.send(JSON.stringify({ type: 'resize_control', session, action, request }));
  }

  function handleTerminalControl(msg) {
    const context = resizeControl;
    if (!context || !resizeContextCurrent(context) || msg.session !== context.session
        || msg.request !== context.request || typeof msg.owned !== 'boolean') return;
    if (msg.owned && (!context.acceptGrant || context.action !== 'acquire')) return;
    if (msg.owned && !['owned', 'invalid_size', 'resize_failed'].includes(msg.reason)) return;
    if (msg.reason === 'pending' && context.owned) return;
    const granted = msg.owned && !context.owned;
    context.owned = msg.owned;
    context.reason = typeof msg.reason === 'string' ? msg.reason : 'unavailable';
    if (!msg.owned) {
      clearResizeTimer();
      if (context.reason !== 'pending') context.acceptGrant = false;
    }
    syncResizeControl();
    if (granted) scheduleMobileResize(); // Renew/error ACKs preserve geometry deduplication.
  }

  function scheduleMobileResize() {
    const context = resizeControl;
    resizeReadingSuspended = mobileResizeSuspended();
    if (!context || !resizeContextCurrent(context) || !context.owned || !mobileResizeReady()
        || resizeReadingSuspended) {
      clearResizeTimer();
      syncResizeControl();
      return;
    }
    const metrics = resizeLayout;
    const configuredFont = viewer.settings().fontSize;
    clearResizeTimer();
    if (!metrics || metrics.stageWidth <= 0 || metrics.stageHeight <= 0
        || metrics.cellWidth <= 0 || metrics.cellHeight <= 0
        || Math.abs(metrics.fontSize - configuredFont) > 0.01) return;
    const cols = Math.max(32, Math.min(400, Math.floor(metrics.stageWidth / metrics.cellWidth)));
    const rows = Math.max(2, Math.min(200, Math.floor(65536 / cols), Math.floor(metrics.stageHeight / metrics.cellHeight)));
    const geometry = cols + 'x' + rows;
    if (geometry === context.lastGeometry) return;
    resizeTimer = setTimeout(() => {
      resizeTimer = null;
      if (!resizeContextCurrent(context) || !context.owned || !mobileResizeReady() || mobileResizeSuspended()) return;
      context.lastGeometry = geometry;
      context.socket.send(JSON.stringify({ type: 'resize', session: context.session, request: context.request, cols, rows }));
    }, 200);
    syncResizeControl();
  }

  viewerResizeControl.addEventListener('click', () => {
    const context = resizeControl;
    requestResizeControl(context && (context.owned || (context.action === 'acquire' && context.reason === 'pending')) ? 'release' : 'acquire');
  });
  window.addEventListener('pagehide', retireResizeControl);
  // Core updates follow state before this listener; a local pan can pause
  // reading without changing its metrics or receiving a server frame.
  viewer.wrap.addEventListener('scroll', reevaluateResizeReading);

  function positionViewerMenu() { /* Sheets follow the visualViewport-sized viewer. */ }

  function syncViewerReadabilityControls() {
    const { fontSize, overview, readableWrap } = viewer.settings();
    viewerFontSize.textContent = fontSize + 'px';
    viewerFontSmaller.disabled = fontSize <= 12;
    viewerFontLarger.disabled = fontSize >= 24;
    viewerOverview.setAttribute('aria-pressed', String(overview));
    viewerOverviewState.textContent = overview ? '켜짐' : '꺼짐';
    viewerReadableWrap.setAttribute('aria-pressed', String(readableWrap));
    viewerReadableState.textContent = readableWrap ? '켜짐' : '꺼짐';
  }

  viewerFontSmaller.addEventListener('click', () => {
    viewer.setFontSize(viewer.settings().fontSize - 1);
    syncViewerReadabilityControls();
  });
  viewerFontLarger.addEventListener('click', () => {
    viewer.setFontSize(viewer.settings().fontSize + 1);
    syncViewerReadabilityControls();
  });
  viewerOverview.addEventListener('click', () => {
    viewer.setOverview(!viewer.settings().overview);
    syncViewerReadabilityControls();
    refreshMobileResizeLayout();
  });
  syncViewerReadabilityControls();

  viewerReadableWrap.addEventListener('click', () => {
    viewer.setReadableWrap(!viewer.settings().readableWrap);
    syncViewerReadabilityControls();
    refreshMobileResizeLayout();
  });
  function syncViewerSearch(result) {
    viewerSearchText.value = result.query;
    viewerSearchCount.textContent = result.query ? (result.total ? `${result.index} / ${result.total}` : '일치하는 텍스트가 없습니다.') : '';
  }
  const findLoadedText = (direction = 1) => syncViewerSearch(viewer.findText(viewerSearchText.value, direction));
  viewerSearchText.addEventListener('input', () => findLoadedText());
  viewerSearchText.addEventListener('keydown', (event) => {
    if (event.isComposing || event.keyCode === 229) return;
    if (event.key === 'Enter') {
      event.preventDefault();
      findLoadedText(event.shiftKey ? -1 : 1);
    } else if (event.key === 'Escape') {
      event.preventDefault();
      event.stopPropagation();
      viewer.clearSearch();
    }
  });
  document.getElementById('viewer-search-prev').addEventListener('click', () => findLoadedText(-1));
  document.getElementById('viewer-search-next').addEventListener('click', () => findLoadedText(1));
  document.getElementById('viewer-search-clear').addEventListener('click', () => viewer.clearSearch());
  const copySelection = document.getElementById('viewer-copy-selection');
  let copyRequest = 0;
  copySelection.addEventListener('pointerdown', (event) => event.preventDefault());
  copySelection.addEventListener('click', async () => {
    const request = ++copyRequest;
    const text = viewer.getSelectedText();
    if (!text) {
      viewerCopyStatus.textContent = '터미널에서 복사할 텍스트를 선택하세요.';
      return;
    }
    const socket = ws;
    const session = viewer.watching;
    const epoch = historyEpoch;
    viewerCopyStatus.textContent = '복사 중…';
    const current = () => request === copyRequest && socket === ws && session === viewer.watching && epoch === historyEpoch
      && !viewer.el.hidden && viewer.privacy.hidden;
    try {
      await navigator.clipboard.writeText(text);
      if (current()) viewerCopyStatus.textContent = '선택한 텍스트를 복사했습니다.';
    } catch (_) {
      if (current()) viewerCopyStatus.textContent = '복사 권한을 확인하거나 Ctrl/Cmd+C로 복사하세요.';
    }
  });

  function updateViewerHeading(sessionId) {
    const workspace = lastWorkspaces.find((item) =>
      (item.sessions || []).some((session) => session.id === sessionId));
    const session = workspace && workspace.sessions.find((item) => item.id === sessionId);
    viewerTitle.textContent = workspace?.name || '워크스페이스';
    viewerSessionChip.textContent = session?.title || '세션';
    const available = (workspace?.sessions || []).filter((item) => item.id && !item.exited);
    viewerSessionChip.disabled = available.length === 0;
    renderSessionSheet(workspace);
  }

  function openViewer(sessionId, title, fromWorkspaceSelection = false) {
    if (!fromWorkspaceSelection) cancelWorkspaceSelection();
    const opened = openCoreViewer(sessionId, title);
    if (opened) updateViewerHeading(sessionId);
    return opened;
  }

  function renderSessionSheet(workspace) {
    const list = document.getElementById('viewer-session-list');
    document.getElementById('viewer-session-workspace').textContent = workspace?.name || '';
    const focusedSession = list.contains(document.activeElement) ? document.activeElement.dataset.sessionId : null;
    list.replaceChildren();
    for (const session of workspace?.sessions || []) list.appendChild(sessionRow(session, workspace, true));
    if (focusedSession) list.querySelector(`[data-session-id="${CSS.escape(focusedSession)}"]`)?.focus({ preventScroll: true });
  }

  // ── 스크롤백 열람 — 코어가 해석한 팬 제스처를 줄 단위 delta로 바꿔 보낸다 (양수 = 과거로).
  // 60ms마다 표시 중인 기록 위치에서 이동한다. 소켓별 요청과 표시 anchor는
  // 데스크톱/다른 시청자의 커서와 독립적이며 passive 출력으로 바꾸지 않는다.
  let scrollAcc = 0;
  let scrollTimer = null;
  let scrollTarget = null;
  let historyEpoch = 0;
  let historyPending = null;
  let historyCompleting = null;
  let historyNotice = '';
  const historyRequests = new WeakMap();
  const HISTORY_REQUEST_MAX = 0xffffffff;
  const HISTORY_TIMEOUT_MS = 5000;
  const HISTORY_DELTA_CAP = 100000;
  let desiredHistoryRows = 0;
  let requestedHistoryRows = 0; // Last correlated successful rows request, never a sent-only request.
  let layoutHistoryAttempts = 0;
  let layoutRetryFollow = false;
  let historyRowsTimer = null;
  let desiredHistoryTarget = null;

  function queueDesiredHistoryRows(rows) {
    if (!Number.isFinite(rows)) return;
    const nextRows = Math.max(1, Math.min(256, Math.trunc(rows)));
    if (nextRows !== desiredHistoryRows) layoutHistoryAttempts = 0;
    desiredHistoryRows = nextRows;
    desiredHistoryTarget = { socket: ws, session: viewer.watching, epoch: historyEpoch };
    clearTimeout(historyRowsTimer);
    historyRowsTimer = setTimeout(flushDesiredHistoryRows, 100);
  }

  function flushDesiredHistoryRows() {
    historyRowsTimer = null;
    if (!historyTargetCurrent(desiredHistoryTarget) || !historyReady() || !viewer.screen
        || historyPending || viewer.getHistoryState().expired || !desiredHistoryRows || desiredHistoryRows === requestedHistoryRows
        || layoutHistoryAttempts >= 2) return;
    sendHistoryRequest('layout', 0, desiredHistoryRows, 'layout');
  }

  function navigateHistory({ delta, rows, reason = 'edge' }) {
    if (!Number.isFinite(delta) || !historyReady()) return;
    if (historyPending) return; // Core retries an edge once the current correlated window settles.
    layoutRetryFollow = false;
    sendHistoryRequest('manual', delta, rows, reason);
  }

  function historyTargetCurrent(target) {
    return !!target && target.socket === ws && target.session === viewer.watching
      && target.epoch === historyEpoch;
  }

  function historyReady() {
    return mobileResizeReady() && viewer.privacy.hidden;
  }

  function syncHistoryUi(state = viewer.getHistoryState()) {
    const pending = historyPending && historyTargetCurrent(historyPending)
      && state.pendingRequest === historyPending.request ? historyPending : null;
    const bottom = document.getElementById('viewer-bottom');
    bottom.disabled = !!(pending && pending.kind === 'reset');
    bottom.textContent = bottom.disabled ? '현재 화면 확인 중…' : '↓ 맨 아래로';
    viewerReadingStatus.textContent = historyNotice || (pending
      ? (pending.kind === 'reset' ? '현재 화면을 확인 중입니다…' : '기록을 불러오는 중입니다…')
      : state.expired ? '불러온 기록이 만료되었습니다. 현재 화면으로 돌아갈 수 있습니다.'
      : state.retained ? '불러온 화면을 유지합니다. 현재 화면으로 돌아갈 수 있습니다.' : '');
  }

  function clearHistoryPending(clearPan = true) {
    const previous = historyPending;
    historyPending = null;
    if (previous) clearTimeout(previous.timer);
    if (clearPan) resetScroll();
    return previous;
  }

  function cancelHistoryRequest(note = '') {
    const previous = clearHistoryPending();
    historyNotice = note;
    if (previous && historyTargetCurrent(previous) && viewer.getHistoryState().pendingRequest === previous.request) {
      viewer.cancelHistoryWindow(previous.request);
    }
    syncHistoryUi();
    if (note && previous?.kind === 'layout' && historyTargetCurrent(previous)) {
      queueMicrotask(() => { if (!historyRowsTimer) flushDesiredHistoryRows(); });
    }
  }

  function retireHistory() {
    cancelHistoryRequest();
    historyEpoch++;
    clearTimeout(historyRowsTimer);
    historyRowsTimer = null;
    desiredHistoryTarget = null;
    desiredHistoryRows = requestedHistoryRows = layoutHistoryAttempts = 0;
    layoutRetryFollow = false;
    viewerCopyStatus.textContent = '';
  }

  function handleReadingChanged(state) {
    // A user gesture can stop following while the automatic request is still
    // pending. Revoke its retry intent; request cancellation after an error has
    // pendingRequest=null and may retain the original one-retry live intent.
    if (historyPending?.kind === 'layout' && historyTargetCurrent(historyPending)
        && state.pendingRequest === historyPending.request && state.followingLive === false) {
      layoutRetryFollow = false;
    }
    let completed = false;
    if (historyPending && state.pendingRequest !== historyPending.request) {
      const matchedSuccess = historyCompleting && historyCompleting.context === historyPending && historyCompleting.success;
      if (matchedSuccess && historyPending.rows) requestedHistoryRows = historyPending.rows;
      clearHistoryPending(!matchedSuccess); // Keep an unsent gesture increment after its earlier ACK.
      completed = true;
    }
    syncHistoryUi(state);
    reevaluateResizeReading();
    if (completed) queueMicrotask(() => { if (!historyRowsTimer) flushDesiredHistoryRows(); });
  }

  function handleHistoryError(msg) {
    const pending = historyPending;
    if (!pending || !historyTargetCurrent(pending) || msg.session !== pending.session || msg.request !== pending.request) return;
    const notes = {
      timeout: '기록 요청을 확인하지 못했습니다. 현재 화면으로 다시 요청하세요.',
      unavailable: '기록을 사용할 수 없습니다. 현재 화면으로 다시 요청할 수 있습니다.',
      stale: '기록 요청이 바뀌었습니다. 현재 화면으로 다시 요청하세요.',
      invalid_anchor: '기록 위치를 확인할 수 없습니다. 현재 화면으로 다시 요청하세요.',
      capacity: '기록 요청이 많습니다. 잠시 후 다시 요청하세요.',
    };
    cancelHistoryRequest(notes[msg.reason] || '기록 요청을 완료하지 못했습니다. 다시 요청하세요.');
  }

  function sendHistoryRequest(kind, increment, rows = desiredHistoryRows || viewer.getDesiredHistoryRows?.(), reason = kind) {
    if (!historyReady()) return false;
    const socket = ws;
    const previous = historyPending;
    const wave = kind === 'manual' && previous?.kind === 'manual' && historyTargetCurrent(previous) ? previous : null;
    const state = viewer.getHistoryState();
    const anchor = wave ? wave.anchor : typeof state.generation === 'string' && typeof state.firstLine === 'string'
      ? { generation: state.generation, first_line: state.firstLine } : null;
    if (kind === 'layout' && layoutHistoryAttempts === 0) layoutRetryFollow = viewer.followingLive;
    const follow = kind === 'reset' || (kind === 'layout' && (viewer.followingLive || layoutRetryFollow));
    const boundedRows = Number.isFinite(rows) ? Math.max(1, Math.min(256, Math.trunc(rows))) : undefined;
    const delta = follow ? 0 : Math.max(-HISTORY_DELTA_CAP, Math.min(HISTORY_DELTA_CAP, (wave?.delta || 0) + increment));
    const sequence = historyRequests.get(socket) || 0;
    if (sequence >= HISTORY_REQUEST_MAX) {
      disconnect(); setViewerConnection('connecting'); connect();
      historyNotice = '연결을 새로 열었습니다. 현재 화면으로 다시 요청하세요.';
      syncHistoryUi();
      return false;
    }
    const request = sequence + 1;
    historyRequests.set(socket, request);
    if (kind === 'layout') layoutHistoryAttempts++;
    if (kind === 'reset') layoutHistoryAttempts = 0;
    const context = { socket, session: viewer.watching, epoch: historyEpoch, request, kind, anchor, delta, rows: boundedRows,
      deadline: wave ? wave.deadline : Date.now() + HISTORY_TIMEOUT_MS, timer: null };
    if (previous) clearTimeout(previous.timer);
    historyPending = context;
    historyNotice = '';
    const message = { type: 'scroll', session: context.session, delta, request, reset: follow };
    if (boundedRows) message.rows = boundedRows;
    if (!follow && anchor) message.anchor = anchor;
    try { socket.send(JSON.stringify(message)); } catch (_) {
      historyPending = previous;
      cancelHistoryRequest('연결이 바뀌어 기록을 요청하지 못했습니다. 다시 요청하세요.');
      return false;
    }
    if (!viewer.expectHistoryWindow(request, { reason, rows: boundedRows, preservePosition: kind !== 'reset', follow })) {
      cancelHistoryRequest('기록 요청을 확인하지 못했습니다. 다시 요청하세요.');
      return false;
    }
    context.timer = setTimeout(() => {
      if (historyPending !== context || !historyTargetCurrent(context)) return;
      cancelHistoryRequest('기록 요청을 확인하지 못했습니다. 현재 화면으로 다시 요청하세요.');
    }, Math.max(0, context.deadline - Date.now()));
    syncHistoryUi();
    return true;
  }

  function queueScroll(lines) {
    layoutRetryFollow = false;
    if (!historyReady() || !Number.isFinite(lines)) return;
    scrollAcc = Math.max(-HISTORY_DELTA_CAP, Math.min(HISTORY_DELTA_CAP, scrollAcc + lines));
    if (scrollTimer) return;
    scrollTarget = { socket: ws, session: viewer.watching, epoch: historyEpoch };
    scrollTimer = setTimeout(() => {
      scrollTimer = null;
      if (!historyTargetCurrent(scrollTarget) || !historyReady()) { resetScroll(); return; }
      const whole = Math.trunc(scrollAcc);
      scrollAcc -= whole;
      if (whole !== 0) sendHistoryRequest('manual', whole);
    }, 60);
  }

  function resetScroll() {
    scrollAcc = 0;
    scrollTarget = null;
    if (scrollTimer) {
      clearTimeout(scrollTimer);
      scrollTimer = null;
    }
  }

  function resumeCurrent() {
    if (historyPending?.kind === 'reset' && historyTargetCurrent(historyPending)) return;
    cancelHistoryRequest();
    viewer.resumeFollow();
    resetPan();
    refreshMobileResizeLayout();
  }
  document.getElementById('viewer-bottom').addEventListener('click', resumeCurrent);
  window.addEventListener('pagehide', retireHistory);

  function sendKey(key, target = inputTarget()) {
    const ctrl = key === 'ctrl_c' || key === 'ctrl_d';
    return sendInputKey(ctrl ? key.slice(-1) : key, { ctrl }, target);
  }

  // ── composer (P6b) — 자유 입력. 행동 계약:
  //   1) 입력은 항상 "시청 중인 세션"에만 간다(서버도 강제).
  //   2) 전송 시점에 target을 캡처한다 — 전송 중 세션이 바뀌어도 캡처된 세션으로만 간다.
  //   3) 전송 실패(WS 미연결)면 draft를 비우지 않는다.
  //   4) 큐 압박(InputPressure) 중에는 전송을 막고 배지로 알린다.
  let composerText = document.getElementById('composer-text');
  const composerSend = document.getElementById('composer-send');
  const composerNote = document.getElementById('composer-note');
  const composerAttach = document.getElementById('composer-attach');
  const composerFile = document.getElementById('composer-file');
  const composer = document.querySelector('.composer');
  const directCtrl = document.getElementById('direct-ctrl');
  const directAlt = document.getElementById('direct-alt');
  let inputGeneration = 0;
  let inputComposition = null;
  let compositionSettling = false;
  let compositionTimer = null;
  let directCtrlArmed = false;
  let directAltArmed = false;
  let inputBlocked = false;
  const MAX_TERMINAL_INPUT_LOCKS = 256;
  const TERMINAL_INPUT_LOCK_OVERFLOW_NOTICE =
    '세션 종료 상태가 너무 많아 입력을 차단했습니다 — 페이지를 새로고침하세요';
  const terminalInputBlockedSessions = new Set();
  let terminalInputLockOverflow = false;
  const MAX_CACHED_DRAFTS = 20;
  const MAX_EVICTED_DRAFT_FLAGS = 256;
  const MAX_DRAFT_CHARS = 256 * 1024;
  const MAX_RECENT_SENDS_PER_SESSION = 32;
  const MAX_RECENT_SENT_CHARS = MAX_DRAFT_CHARS * 2;
  const MAX_STORED_DRAFT_CHARS = MAX_DRAFT_CHARS
    + MAX_RECENT_SENT_CHARS
    + MAX_RECENT_SENDS_PER_SESSION;
  const viewerDrafts = new Map();
  const recentSentBySession = new Map();
  const recoveredDraftSessions = new Set();
  const evictedDraftSessions = new Set();
  let draftCacheNotice = '';
  const RECENT_SEND_TTL_MS = 30_000;
  let recentSentExpiryTimer = null;
  let composerSession = null;
  let composerRecoveryWarningSession = null;
  const RECOVERY_WARNING = '최근 입력을 복원했습니다 — 중복 여부를 확인하세요.';
  composerText.maxLength = MAX_DRAFT_CHARS;

  function setBoundedSessionValue(store, sessionId, value) {
    store.delete(sessionId);
    const bounded = typeof value === 'string'
      ? value.slice(0, MAX_STORED_DRAFT_CHARS)
      : value;
    if (bounded) store.set(sessionId, bounded);
    else if (store === viewerDrafts) recoveredDraftSessions.delete(sessionId);
    while (store.size > MAX_CACHED_DRAFTS) {
      const evicted = store.keys().next().value;
      store.delete(evicted);
      if (store === viewerDrafts) {
        recoveredDraftSessions.delete(evicted);
        evictedDraftSessions.delete(evicted);
        evictedDraftSessions.add(evicted);
        while (evictedDraftSessions.size > MAX_EVICTED_DRAFT_FLAGS) {
          evictedDraftSessions.delete(evictedDraftSessions.values().next().value);
        }
        draftCacheNotice = '메모리 제한으로 가장 오래된 세션 초안 1개를 정리했습니다.';
      }
    }
  }

  function saveComposerDraft() {
    if (!composerSession) return;
    setBoundedSessionValue(viewerDrafts, composerSession, composerText.value);
    // 활성 세션의 authoritative textarea를 다시 저장했으므로 이전 eviction 표시는 stale이다.
    evictedDraftSessions.delete(composerSession);
    if (composerRecoveryWarningSession === composerSession) {
      recoveredDraftSessions.add(composerSession);
    }
  }

  function preserveComposerDraftForTransition() {
    if (!composerSession) return;
    saveComposerDraft();
  }

  function loadComposerDraft(sessionId) {
    composerSession = sessionId;
    composerRecoveryWarningSession = null;
    composerText.value = viewerDrafts.get(sessionId) || '';
    autoGrow();
    const notices = [];
    if (draftCacheNotice) {
      notices.push(draftCacheNotice);
      draftCacheNotice = '';
    }
    if (evictedDraftSessions.delete(sessionId)) {
      notices.push('이 세션의 이전 초안을 복원하지 못했습니다.');
    }
    if (recoveredDraftSessions.delete(sessionId)) {
      notices.push(RECOVERY_WARNING);
      composerRecoveryWarningSession = sessionId;
    }
    setComposerNote(notices.join(' ')); // clear stale note
  }

  function discardSessionDraft(sessionId) {
    viewerDrafts.delete(sessionId);
    recentSentBySession.delete(sessionId);
    scheduleRecentSentExpiry();
    recoveredDraftSessions.delete(sessionId);
    evictedDraftSessions.delete(sessionId);
    terminalInputBlockedSessions.delete(sessionId);
    if (composerRecoveryWarningSession === sessionId) {
      composerRecoveryWarningSession = null;
    }
  }

  function rememberTerminalInputLock(sessionId) {
    if (terminalInputBlockedSessions.has(sessionId)) return;
    if (terminalInputBlockedSessions.size >= MAX_TERMINAL_INPUT_LOCKS) {
      terminalInputLockOverflow = true; // fail closed without growing memory
      setComposerNote(TERMINAL_INPUT_LOCK_OVERFLOW_NOTICE);
      updateComposerEnabled();
      return;
    }
    terminalInputBlockedSessions.add(sessionId);
  }

  function pruneRecentSent(now = Date.now()) {
    const cutoff = now - RECENT_SEND_TTL_MS;
    for (const [sessionId, entries] of recentSentBySession) {
      const recent = entries.filter((item) => item.at > cutoff);
      if (recent.length) recentSentBySession.set(sessionId, recent);
      else recentSentBySession.delete(sessionId);
    }
    scheduleRecentSentExpiry();
  }

  function scheduleRecentSentExpiry() {
    if (recentSentExpiryTimer) clearTimeout(recentSentExpiryTimer);
    recentSentExpiryTimer = null;
    let earliest = Infinity;
    for (const entries of recentSentBySession.values()) {
      for (const item of entries) earliest = Math.min(earliest, item.at);
    }
    if (!Number.isFinite(earliest)) return;
    const delay = Math.max(0, earliest + RECENT_SEND_TTL_MS - Date.now());
    recentSentExpiryTimer = setTimeout(() => {
      recentSentExpiryTimer = null;
      pruneRecentSent();
    }, delay);
  }

  function canRememberRecentSent(sessionId, text) {
    pruneRecentSent();
    const recent = recentSentBySession.get(sessionId) || [];
    if (!recentSentBySession.has(sessionId)
        && recentSentBySession.size >= MAX_CACHED_DRAFTS) return false;
    if (recent.length >= MAX_RECENT_SENDS_PER_SESSION) return false;
    return recent.reduce((sum, item) => sum + item.text.length, 0) + text.length
      <= MAX_RECENT_SENT_CHARS;
  }

  function rememberRecentSent(sessionId, text) {
    const cutoff = Date.now() - RECENT_SEND_TTL_MS;
    const recent = (recentSentBySession.get(sessionId) || [])
      .filter((item) => item.at >= cutoff);
    recent.push({ text, at: Date.now() });
    recentSentBySession.delete(sessionId);
    recentSentBySession.set(sessionId, recent);
    scheduleRecentSentExpiry(); // arm journal expiry
  }

  function autoGrow() {
    const previousWrapHeight = viewer.wrap.clientHeight;
    composerText.style.height = 'auto';
    // 최대 5행 — 그 이상은 내부 스크롤
    const max = 5 * 22 + 22;
    composerText.style.height = Math.max(44, Math.min(composerText.scrollHeight, max)) + 'px';
    scheduleViewerRenderForLayoutChange(previousWrapHeight);
  }

  function setComposerNote(text) {
    const previousWrapHeight = viewer.wrap.clientHeight;
    const notices = text ? [text] : [];
    if (composerRecoveryWarningSession === composerSession
        && !text.includes(RECOVERY_WARNING)) {
      notices.push(RECOVERY_WARNING);
    }
    if (terminalInputLockOverflow
        && !text.includes(TERMINAL_INPUT_LOCK_OVERFLOW_NOTICE)) {
      notices.push(TERMINAL_INPUT_LOCK_OVERFLOW_NOTICE);
    }
    const message = notices.join(' ');
    composerNote.hidden = !message;
    composerNote.textContent = message;
    scheduleViewerRenderForLayoutChange(previousWrapHeight);
  }

  function remoteInputReady() {
    return !!viewer.watching
      && !!ws && ws.readyState === WebSocket.OPEN
      && composerSession === viewer.watching
      && !viewer.closing
      && !document.hidden
      && viewer.connection === 'connected'
      && !terminalInputLockOverflow
      && !terminalInputBlockedSessions.has(viewer.watching)
      && !inputBlocked;
  }

  function updateComposerEnabled() {
    const ready = remoteInputReady();
    if (!ready && (inputComposition || compositionSettling || directCtrlArmed || directAltArmed)) resetInputContext();
    composerSend.disabled = !ready || !composerText.value || !!inputComposition || compositionSettling;
    composerText.disabled = !ready;
    composerAttach.disabled = !ready || uploadBusy;
    for (const button of viewer.keys) button.disabled = !ready;
    syncResizeControl();
  }

  function sendComposer() {
    // (2) 전송 시점 target 캡처 — 이후 전환돼도 이 세션으로만 간다.
    const target = viewer.watching;
    if (!target || !remoteInputReady() || inputComposition || compositionSettling) return;
    const text = composerText.value.normalize('NFC');
    if (!text) return;
    const inputMessage = { type: 'input', session: target, text, submit: true };
    const serializedInput = JSON.stringify(inputMessage);
    if (utf8Encoder.encode(text).byteLength > MAX_INPUT_BYTES
        || utf8Encoder.encode(serializedInput).byteLength > MAX_INPUT_FRAME_BYTES) {
      setComposerNote('입력이 너무 큽니다 (256KB 초과)');
      return;
    }
    if (!canRememberRecentSent(target, text)) {
      setComposerNote('최근 전송 확인 중입니다 — 잠시 후 다시 보내세요');
      return;
    }
    // (3) 전송 실패면 draft 유지 — send()가 false를 준다(WS 미연결).
    if (!sendSerialized(serializedInput)) {
      setComposerNote('연결이 끊겼습니다 — 재연결 후 다시 전송하세요');
      return;
    }
    resumeCurrent();
    rememberRecentSent(target, text);
    composerRecoveryWarningSession = null;
    setComposerNote('');
    composerText.value = '';
    saveComposerDraft();
    autoGrow();
    updateComposerEnabled();
  }

  /// 큐 거부로 유실된 입력을 composer로 되돌린다(사용자가 재타이핑하지 않게).
  function restoreDraft(note, sessionId) {
    const cutoff = Date.now() - RECENT_SEND_TTL_MS;
    const recent = (recentSentBySession.get(sessionId) || [])
      .filter((item) => item.at > cutoff);
    recentSentBySession.delete(sessionId);
    scheduleRecentSentExpiry();
    if (!recent.length) return false;
    const uncertain = recent.map((item) => item.text).join('\n');
    const current = composerSession === sessionId
      ? composerText.value
      : (viewerDrafts.get(sessionId) || '');
    const separator = uncertain && current && !/\s$/.test(uncertain) ? '\n' : '';
    const restored = uncertain + separator + current;
    setBoundedSessionValue(viewerDrafts, sessionId, restored);
    if (composerSession === sessionId) {
      composerText.value = restored;
      autoGrow();
      const restoreNotices = [draftCacheNotice, note].filter(Boolean);
      draftCacheNotice = '';
      composerRecoveryWarningSession = sessionId;
      setComposerNote(restoreNotices.join(' '));
    } else {
      recoveredDraftSessions.delete(sessionId);
      recoveredDraftSessions.add(sessionId);
      while (recoveredDraftSessions.size > MAX_CACHED_DRAFTS) {
        recoveredDraftSessions.delete(recoveredDraftSessions.values().next().value);
      }
    }
    return true;
  }

  function handleInputPressure(msg) {
    const queued = msg.queued || 0;
    switch (msg.reason) {
      case 'queue_full':
        // 큐가 차서 이번 입력이 버려졌다 — 되돌려주고, 빠질 때까지 전송을 막는다.
        inputBlocked = queued > 0;
        if (inputBlocked && !restoreDraft(
            '입력 대기열이 차 최근 입력을 복원했습니다 — 중복 여부를 확인하세요',
            msg.session)) {
          setComposerNote('입력 대기열이 찼습니다 — 잠시 후 다시 보내세요');
        } else if (!inputBlocked) {
          recentSentBySession.delete(msg.session);
          scheduleRecentSentExpiry();
          if (composerRecoveryWarningSession !== msg.session) setComposerNote('');
          consumePendingUploadSelection(); // pressure resolved
        }
        break;
      case 'closed':
      case 'unavailable':
        // 세션이 끝났거나 쓸 수 없다 — 재시도해도 소용없으니 차단하지 않고 알리기만 한다.
        inputBlocked = false;
        rememberTerminalInputLock(msg.session); // terminal input lock
        cancelActiveUpload(); // terminal session cannot accept upload
        cancelPendingUploadSelection(); // terminal pressure cannot resume pending upload
        if (!restoreDraft('세션이 종료되어 입력이 전달되지 않았습니다', msg.session)) {
          setComposerNote('세션이 종료되어 입력이 전달되지 않았습니다');
        }
        break;
      case 'too_large':
        // 해소 이벤트가 오지 않는 종류다(runtime이 재시도 큐에 넣지 않음) — 잠그지 않는다.
        inputBlocked = false;
        cancelPendingUploadSelection(); // terminal pressure cannot resume pending upload
        if (!restoreDraft('입력이 너무 커서 전달되지 않았습니다', msg.session)) {
          setComposerNote('입력이 너무 커서 전달되지 않았습니다');
        }
        break;
      case 'admission_denied':
        // Mode refresh can transiently deny an operation without entering the PTY queue.
        // Preserve a separate queue_full lock, and keep interrupts available otherwise.
        if (!restoreDraft('터미널 상태를 갱신 중이라 입력이 전달되지 않았습니다. 초안을 복원했습니다.', msg.session))
          setComposerNote('터미널 상태를 갱신 중이라 입력이 전달되지 않았습니다 — 다시 입력하세요');
        break;
      default:
        inputBlocked = queued > 0;
        setComposerNote(inputBlocked ? '입력 대기열이 찼습니다 — 잠시 후 다시 보내세요' : '');
    }
    updateComposerEnabled();
  }

  // ── 첨부 (P6d) — 파일 선택 → POST /upload(fetch, 토큰 쿼리) → 응답 경로를 composer에
  // 삽입한다. 파일명/경로는 전부 서버가 생성한다(이 클라는 Content-Type만 알려줄 뿐, 파일명을
  // 보내지 않는다) — 사용자가 문맥과 함께 전송해야 에이전트에 전달된다(전송은 별도 동작).
  const MAX_UPLOAD_BYTES = 10 * 1024 * 1024; // 서버 상한과 동일
  let uploadBusy = false;
  let activeUpload = null;
  let pickerSession = null;
  let pendingUploadSelection = null;

  function cancelActiveUpload() {
    if (!activeUpload) return;
    activeUpload.controller.abort();
    activeUpload = null;
    uploadBusy = false;
    setComposerNote('');
    updateComposerEnabled();
  }

  function cancelPendingUploadSelection() {
    pickerSession = null;
    pendingUploadSelection = null;
  }

  function uploadErrorNote(status) {
    if (status === 401) return '인증이 만료됐습니다 — 다시 페어링하세요';
    if (status === 404) return '파일 첨부가 비활성화돼 있습니다';
    if (status === 413) return '파일이 너무 큽니다 (10MB 초과)';
    if (status === 415) return '지원하지 않는 파일 형식입니다';
    return '업로드 실패 (' + status + ')';
  }

  composerAttach.addEventListener('click', () => {
    if (composerAttach.disabled) return;
    pickerSession = viewer.watching;
    composerFile.click();
  });

  function consumePendingUploadSelection() {
    if (!pendingUploadSelection || !remoteInputReady()) return;
    const pending = pendingUploadSelection;
    if (viewer.watching !== pending.session) {
      pendingUploadSelection = null;
      return;
    }
    pendingUploadSelection = null;
    beginSelectedUpload(pending.session, pending.file);
  }

  async function beginSelectedUpload(uploadSession, file) {
    if (viewer.watching !== uploadSession || !remoteInputReady()) return;
    const tokenValue = localStorage.getItem(TOKEN_KEY);
    if (!tokenValue) {
      setComposerNote('토큰이 없습니다 — 다시 페어링하세요');
      return;
    }
    const upload = { session: uploadSession, controller: new AbortController() };
    activeUpload = upload;
    uploadBusy = true;
    updateComposerEnabled();
    setComposerNote('업로드 중…');
    try {
      const res = await fetch('/upload?token=' + encodeURIComponent(tokenValue), {
        method: 'POST',
        headers: { 'Content-Type': file.type || 'application/octet-stream' },
        body: file,
        signal: upload.controller.signal,
      });
      if (activeUpload !== upload) return;
      if (!res.ok) {
        setComposerNote(uploadErrorNote(res.status));
        return;
      }
      const result = await res.json();
      if (activeUpload !== upload) return;
      if (!result || typeof result.path !== 'string' || !result.path) {
        setComposerNote('업로드 응답이 올바르지 않습니다');
        return;
      }
      if (activeUpload !== upload) return;
      if (viewer.watching !== uploadSession || !remoteInputReady()) {
        setComposerNote('연결 또는 세션이 바뀌어 업로드 경로를 입력하지 않았습니다');
        return;
      }
      // 기존 입력에 이어 붙인다(신뢰경계: value 대입만 — innerHTML 아님). 사용자가 문맥과
      // 함께 전송한다(데스크톱 이미지 paste와 동일 종단 — 에이전트가 경로를 읽는다).
      const separator = composerText.value && !/\s$/.test(composerText.value) ? '\n' : '';
      const nextComposerValue = composerText.value + separator + result.path.normalize('NFC') + ' ';
      if (nextComposerValue.length > MAX_DRAFT_CHARS) {
        setComposerNote('입력이 너무 길어 업로드 경로를 추가하지 않았습니다');
        return;
      }
      composerText.value = nextComposerValue;
      saveComposerDraft();
      autoGrow();
      setComposerNote('');
    } catch (error) {
      if (activeUpload !== upload) return;
      if (!(error && error.name === 'AbortError')) {
        setComposerNote('업로드 실패 — 네트워크를 확인하세요');
      }
    } finally {
      if (activeUpload === upload) {
        activeUpload = null;
        uploadBusy = false;
        updateComposerEnabled();
      }
    }
  }

  composerFile.addEventListener('change', () => {
    const uploadSession = pickerSession;
    pickerSession = null;
    const file = composerFile.files && composerFile.files[0];
    composerFile.value = '';
    if (!file) return;
    if (!uploadSession || viewer.watching !== uploadSession) {
      setComposerNote('세션이 바뀌어 선택한 파일을 업로드하지 않았습니다');
      return;
    }
    if (file.size > MAX_UPLOAD_BYTES) {
      setComposerNote('파일이 너무 큽니다 (10MB 초과)');
      return;
    }
    if (!remoteInputReady()) {
      pendingUploadSelection = { session: uploadSession, file };
      setComposerNote('재연결 후 선택한 파일을 업로드합니다…');
      return;
    }
    beginSelectedUpload(uploadSession, file);
  });

  function resetInputModifiers() {
    directCtrlArmed = false;
    directAltArmed = false;
    directCtrl.setAttribute('aria-pressed', 'false');
    directAlt.setAttribute('aria-pressed', 'false');
  }

  function resetInputContext() {
    inputGeneration++;
    clearTimeout(compositionTimer);
    inputComposition = null;
    compositionSettling = false;
    resetInputModifiers();
    // Keep the buffered value but retire the native IME element. Late events belong
    // to the detached input and cannot append to a different session's draft.
    const replacement = composerText.cloneNode(false);
    replacement.value = composerText.value;
    composerText.replaceWith(replacement);
    composerText = replacement;
  }

  function inputTarget() {
    return { session: viewer.watching, socket: ws, element: composerText, generation: inputGeneration,
      ctrl: directCtrlArmed, alt: directAltArmed };
  }

  function inputTargetReady(target) {
    return target.session === viewer.watching && target.socket === ws
      && target.generation === inputGeneration && target.element === composerText && remoteInputReady();
  }

  function sendInputKey(key, modifiers = {}, target = inputTarget()) {
    if (!inputTargetReady(target)) return false;
    const ctrl = !!(modifiers.ctrl || target.ctrl), alt = !!(modifiers.alt || target.alt);
    const interrupt = (key === 'esc' && !ctrl) || (ctrl && /^[cCdD]$/.test(key));
    if (inputComposition || compositionSettling) {
      if (!interrupt) {
        setComposerNote('한글 조합을 마친 뒤 보조 키를 다시 누르세요. 초안은 전송되지 않습니다.');
        return false;
      }
      // An explicit interrupt retires the IME target, preserves its buffered DOM
      // value, and sends only the key. A late composition event cannot submit it.
      resetInputContext();
      target = inputTarget();
      saveComposerDraft();
      autoGrow();
      composerText.focus({ preventScroll: true });
      updateComposerEnabled();
    }
    const ascii = key.length === 1 && /^[\x20-\x7e]$/.test(key);
    const named = ['enter', 'backspace', 'tab', 'esc', 'up', 'down', 'left', 'right',
      'home', 'end', 'delete', 'insert', 'page_up', 'page_down'].includes(key);
    const validCtrlAscii = /^[a-zA-Z @2\[3\\4\]5^6_7?8]$/.test(key);
    if ((!named && !(ascii && (ctrl || alt))) || (ascii && ctrl && !validCtrlAscii)
        || (ctrl && ['enter', 'tab', 'esc'].includes(key))) {
      resetInputModifiers();
      setComposerNote('지원하지 않는 Ctrl/Alt 키 조합입니다');
      return false;
    }
    resetInputModifiers();
    const sent = send({ type: 'direct_key', session: target.session, key, ctrl, alt,
      shift: !!modifiers.shift, meta: false });
    if (sent) {
      resumeCurrent();
      if (composerText.value) setComposerNote('보조 키만 전송했습니다. 작성 중인 초안은 유지됩니다.');
    }
    return sent;
  }

  for (const [button, modifier] of [[directCtrl, 'ctrl'], [directAlt, 'alt']]) {
    button.addEventListener('pointerdown', event => event.preventDefault());
    button.addEventListener('click', () => {
      if (!remoteInputReady() || inputComposition || compositionSettling) return;
      if (modifier === 'ctrl') directCtrlArmed = !directCtrlArmed;
      else directAltArmed = !directAltArmed;
      button.setAttribute('aria-pressed', String(modifier === 'ctrl' ? directCtrlArmed : directAltArmed));
      composerText.focus({ preventScroll: true });
    });
  }

  function onBufferedInput(type, handler) {
    composer.addEventListener(type, event => { if (event.target === composerText) handler(event); }, true);
  }
  onBufferedInput('compositionstart', () => {
    if (!remoteInputReady()) return;
    clearTimeout(compositionTimer);
    compositionSettling = false;
    inputComposition = inputTarget();
    updateComposerEnabled();
  });
  onBufferedInput('compositionend', () => {
    if (!inputComposition) return;
    const target = inputComposition;
    inputComposition = null;
    compositionSettling = true;
    compositionTimer = setTimeout(() => {
      if (!inputTargetReady(target)) return;
      compositionSettling = false;
      saveComposerDraft();
      autoGrow();
      updateComposerEnabled();
    }, 0);
  });
  onBufferedInput('input', () => {
    if (composerRecoveryWarningSession) { composerRecoveryWarningSession = null; setComposerNote(''); }
    saveComposerDraft();
    autoGrow();
    updateComposerEnabled();
  });
  onBufferedInput('keydown', event => {
    if (event.isComposing || event.keyCode === 229 || inputComposition || compositionSettling) return;
    if (event.ctrlKey && event.shiftKey && !event.altKey && !event.metaKey
        && (event.key.toLowerCase() === 'm' || event.code === 'KeyM')) {
      event.preventDefault();
      document.getElementById('viewer-menu-button').focus({ preventScroll: true });
    } else if (event.key === 'Enter' && !event.shiftKey) {
      event.preventDefault();
      sendComposer();
    } else if (!event.metaKey && !event.getModifierState('AltGraph') && event.key.length === 1
        && (directCtrlArmed || directAltArmed)) {
      event.preventDefault();
      sendInputKey(event.key);
    }
  });
  // Soft-keyboard beforeinput line breaks remain editable newlines; only explicit
  // Enter keydown and the send button submit the buffered value.
  composerSend.addEventListener('pointerdown', event => event.preventDefault());
  composerSend.addEventListener('click', sendComposer);
  window.addEventListener('pagehide', resetInputContext);

  for (const btn of document.querySelectorAll('.viewer-keys button[data-key]')) {
    const key = btn.dataset.key;
    let repeatTarget = null, repeatTimer = null, repeatInterval = null, repeated = false;
    const stopRepeat = () => { clearTimeout(repeatTimer); clearInterval(repeatInterval); repeatTimer = repeatInterval = null; };
    keyRepeatCancels.push(() => { repeated = repeated || !!repeatTarget; repeatTarget = null; stopRepeat(); });
    btn.addEventListener('pointerdown', event => {
      event.preventDefault();
      repeatTarget = inputTarget();
      repeated = false;
      if (['up', 'down', 'left', 'right'].includes(key)) {
        stopRepeat();
        repeatTimer = setTimeout(() => { repeatInterval = setInterval(() => {
          repeated = true; sendKey(key, repeatTarget);
        }, 120); }, 400);
      }
    });
    btn.addEventListener('pointerup', stopRepeat);
    for (const name of ['pointerleave', 'pointercancel']) btn.addEventListener(name, () => {
      repeated = true; repeatTarget = null; stopRepeat();
    });
    btn.addEventListener('click', () => {
      if (!repeated) sendKey(key, repeatTarget || inputTarget());
      repeated = false; repeatTarget = null;
    });
  }

  function renderApprovals(pending) {
    approvalsCount.textContent = String(pending.length);
    approvalToggle.hidden = pending.length === 0;
    dashboardApprovalsButton.textContent = '승인 대기 ' + pending.length;
    dashboardApprovalsButton.disabled = pending.length === 0;
    viewerApprovalsButton.disabled = pending.length === 0;
    if (pending.length === 0) approvalPanel.hidden = true;
    approvalsEmpty.hidden = pending.length > 0;
    approvalsEl.textContent = '';
    for (const item of pending) {
      approvalsEl.appendChild(approvalCard(item));
    }
    // 앱 아이콘 뱃지(iOS 16.4+ 설치형 + 알림 권한 시) — P3에서 권한 유도. 실패는 무시.
    if (navigator.setAppBadge) {
      if (pending.length > 0) navigator.setAppBadge(pending.length).catch(() => {});
      else if (navigator.clearAppBadge) navigator.clearAppBadge().catch(() => {});
    }
  }

  function approvalCard(item) {
    const card = document.createElement('div');
    card.className = 'approval-card';

    const head = document.createElement('div');
    head.className = 'card-head';
    const server = document.createElement('span');
    server.className = 'server';
    server.textContent = item.server || '';
    const tool = document.createElement('span');
    tool.className = 'tool';
    tool.textContent = item.tool || '';
    head.appendChild(server);
    head.appendChild(tool);
    card.appendChild(head);

    // 어느 세션의 승인인지 (I2). 세션 불명이면 표시하지 않는다. textContent만.
    // 어느 세션의 승인인지 — 제목은 이미 받은 세션 목록에서 찾고, 없으면 서버 폴백.
    const known = item.session && lastSessions.find((x) => x.id === item.session);
    const title = (known && known.title) || item.session_title;
    if (title) {
      const sess = document.createElement('div');
      sess.className = 'approval-session';
      sess.textContent = '세션: ' + title;
      card.appendChild(sess);
    }

    // 미리보기(URL·인자)는 proxy가 이미 redact한 표시용 텍스트 — textContent로만.
    if (item.preview) {
      const pre = document.createElement('pre');
      pre.className = 'preview';
      pre.textContent = item.preview;
      card.appendChild(pre);
    }

    const remember = document.createElement('label');
    remember.className = 'remember';
    const cb = document.createElement('input');
    cb.type = 'checkbox';
    remember.appendChild(cb);
    remember.appendChild(document.createTextNode(' 이 결정을 기억(규칙으로 저장)'));
    card.appendChild(remember);

    const actions = document.createElement('div');
    actions.className = 'actions';
    // 승인 전 맥락 확인 — 그 세션 화면을 연다 (I2). session(영속 UUID)이 있을 때만.
    // 서버는 활성 워크스페이스의 승인에만 이 값을 채운다(u64 앨리어싱 방지).
    if (known && !known.exited) {
      const view = document.createElement('button');
      view.className = 'approval-view';
      view.type = 'button';
      view.textContent = '화면 보기';
      view.addEventListener('click', () => {
        const row = lastSessions.find((session) => session.id === item.session);
        if (!row || row.exited) return;
        openViewer(item.session, row.title || item.session_title || '세션');
      });
      actions.appendChild(view);
    }
    const deny = document.createElement('button');
    deny.className = 'deny';
    deny.textContent = '거부';
    deny.addEventListener('click', () => resolve(item.id, false, cb.checked, card));
    const allow = document.createElement('button');
    allow.className = 'allow';
    allow.textContent = '허용';
    allow.addEventListener('click', () => resolve(item.id, true, cb.checked, card));
    actions.appendChild(deny);
    actions.appendChild(allow);
    card.appendChild(actions);
    return card;
  }

  function resolve(id, allowed, remember, card) {
    if (!ws || ws.readyState !== WebSocket.OPEN) return;
    ws.send(JSON.stringify({ type: 'resolve', id, allowed, remember }));
    // 낙관적 비활성화 — 서버의 다음 approvals 프레임이 목록을 확정한다.
    card.classList.add('resolving');
    for (const btn of card.querySelectorAll('button')) btn.disabled = true;
  }

  // ── 알림 딥링크 (P6c) — 알림 탭 → 그 세션 화면. 세션 id는 worker-로컬(재시작 시
  // 재배정)이라, 대시보드에 실재하는 id일 때만 자동 시청한다(스테일 알림 방어).
  let pendingWatch = null;
  let pendingWatchDeadline = 0;
  // 세션 식별자는 **영속 UUID 문자열**이다 (I1) — 재시작해도 같은 세션을 가리킨다.
  // 그래도 기한을 둔다: 이미 끝난 세션의 알림을 한참 뒤 탭하면 조용히 폐기한다.
  const PENDING_WATCH_TTL_MS = 30_000;

  function setPendingWatch(session) {
    pendingWatch = session;
    pendingWatchDeadline = Date.now() + PENDING_WATCH_TTL_MS;
  }
  {
    const watchParam = params.get('watch');
    if (watchParam) {
      setPendingWatch(watchParam);
      history.replaceState(null, '', location.pathname); // URL 위생
    }
  }
  if ('serviceWorker' in navigator) {
    // 이미 열린 창에 알림 클릭이 도착한 경우 — SW가 postMessage로 전달한다.
    navigator.serviceWorker.addEventListener('message', (event) => {
      const msg = event.data;
      if (msg && msg.type === 'watch' && typeof msg.session === 'string') {
        setPendingWatch(msg.session);
        consumePendingWatch(lastSessions);
      }
    });
  }

  let lastSessions = [];   // 시청 가능한 세션(활성 워크스페이스)
  let lastWorkspaces = [];
  let lastResource = null; // 재렌더 시 CPU/RAM 줄이 깜빡 사라지지 않게 보관

  /// 딥링크 대상이 현재 세션 목록에 있으면 시청을 시작한다(1회성).
  function consumePendingWatch(sessions) {
    if (!pendingWatch) return false;
    if (Date.now() > pendingWatchDeadline) {
      pendingWatch = null;
      return false;
    }
    if (viewer.closing) return false;
    if (pendingWatch === viewer.watching) {
      pendingWatch = null;
      return true;
    }
    const target = sessions.find((session) => session.id === pendingWatch);
    if (!target || target.exited) {
      if (target && target.exited) pendingWatch = null;
      return false;
    }
    const id = pendingWatch;
    if (!openViewer(id, target.title || ('세션 ' + id))) return false;
    pendingWatch = null;
    return true;
  }

  const WORKSPACE_STATE_LABEL = { active: '활성', warm: '대기', suspended: '절전' };

  // 미러 진입 안내 배너(I1b-2). 서버가 Dashboard 프레임에 실어 보내는 일시 안내(전환 상한
  // 초과 등)를 표시한다. 서버가 TTL 동안 같은 안내를 반복해 보내도 내용이 바뀔 때만 한 번
  // 띄우고, 클라 타이머로 몇 초 뒤 자동으로 숨긴다. 내용은 textContent로만 삽입한다.
  let lastNotice = null;
  let noticeTimer = null;
  function showNotice(text) {
    if (!text) {
      lastNotice = null; // 서버가 내림 — 다음에 같은 문구가 와도 다시 뜨게 리셋
      return;
    }
    if (text === lastNotice) return; // 같은 안내 반복 표시 방지
    lastNotice = text;
    noticeBanner.textContent = text;
    noticeBanner.hidden = false;
    if (noticeTimer) clearTimeout(noticeTimer);
    noticeTimer = setTimeout(() => {
      noticeBanner.hidden = true;
    }, 6000);
  }

  const workspaceExpansions = new Map();
  let pendingWorkspaceSelection = null;
  let workspaceSelectionTimer = null;
  function cancelWorkspaceSelection(note = '') {
    clearTimeout(workspaceSelectionTimer); workspaceSelectionTimer = null;
    pendingWorkspaceSelection = null;
    if (note) showNotice(note);
  }
  function selectDormantSession(workspace, session) {
    cancelWorkspaceSelection();
    const stable = typeof session.selection_id === 'string' && session.selection_id ? session.selection_id : null;
    if (stable) {
      pendingWorkspaceSelection = { socket: ws, workspace: workspace.id, selection: stable };
      workspaceSelectionTimer = setTimeout(() => cancelWorkspaceSelection('워크스페이스 전환을 확인하지 못했습니다. 세션을 다시 선택하세요.'), 15000);
    }
    if (!send({ type: 'switch', workspace: workspace.id })) {
      cancelWorkspaceSelection('연결이 바뀌어 워크스페이스를 전환하지 못했습니다. 다시 선택하세요.'); return;
    }
    showNotice(stable ? 'Mac 워크스페이스도 전환됩니다. 선택한 세션을 준비하고 있습니다.'
      : 'Mac 워크스페이스가 전환됩니다. 전환 후 세션을 선택하세요.');
  }
  function consumeWorkspaceSelection() {
    const pending = pendingWorkspaceSelection;
    if (!pending) return;
    if (pending.socket !== ws || !isCurrentSocket(pending.socket)) { cancelWorkspaceSelection(); return; }
    const workspace = lastWorkspaces.find(candidate => candidate.id === pending.workspace);
    if (!workspace) { cancelWorkspaceSelection('선택한 워크스페이스가 없어 세션 전환을 취소했습니다.'); return; }
    const session = workspace.sessions?.find(candidate => candidate.selection_id === pending.selection);
    if (!session || session.exited) { cancelWorkspaceSelection('선택한 세션이 없어 전환을 취소했습니다.'); return; }
    if (workspace.state !== 'active' || !session.id) return;
    cancelWorkspaceSelection();
    openViewer(session.id, session.title || '세션', true);
  }
  const workspaceSearch = document.getElementById('workspace-search');
  const searchable = value => String(value || '').normalize('NFC').toLocaleLowerCase();
  function sessionStatusLabel(session, workspace) {
    if (session.exited) return '종료';
    if (workspace.state !== 'active') return workspace.state === 'warm' ? '대기 · 저장된 세션' : '절전 · 저장된 세션';
    return STATUS_LABEL[session.status] || session.status || '상태 정보 없음';
  }
  function matchesSession(session, workspace, query) {
    return searchable([session.title, session.agent, session.status, sessionStatusLabel(session, workspace)].join(' ')).includes(query);
  }
  workspaceSearch.addEventListener('input', () => renderWorkspaceList());

  function renderWorkspaces(workspaces, resource) {
    lastWorkspaces = workspaces;
    workspaceCount.textContent = workspaces.length + ' 워크스페이스';
    if (viewer.watching) updateViewerHeading(viewer.watching);
    if (resource) lastResource = resource;
    lastSessions = workspaces.flatMap(workspace => workspace.sessions || []).filter(session => session.id);
    consumeWorkspaceSelection();
    const endedSession = viewer.watching;
    const watched = endedSession ? lastSessions.find(session => session.id === endedSession) : null;
    if (endedSession && (!watched || watched.exited)) {
      queueMicrotask(() => {
        if (viewer.watching !== endedSession) return;
        requestCloseViewer({ rerender: true,
          notice: '선택한 세션이 종료되었습니다. 작성 중인 초안은 이 세션에 보관했습니다.', discardDraft: false });
      });
    }
    renderWorkspaceList();
    if (lastResource) {
      const cpu = lastResource.cpu != null ? lastResource.cpu.toFixed(0) + '%' : '—';
      resourceEl.textContent = 'CPU ' + cpu + ' · RAM ' + (lastResource.rss_mb || 0) + 'MB';
    }
    consumePendingWatch(lastSessions);
  }

  function renderWorkspaceList() {
    const query = searchable(workspaceSearch.value.trim());
    const focused = sessionsEl.contains(document.activeElement) ? {
      session: document.activeElement.dataset.sessionId, workspace: document.activeElement.dataset.workspaceId,
    } : null;
    const alive = new Set(lastWorkspaces.map(workspace => workspace.id));
    for (const id of workspaceExpansions.keys()) if (!alive.has(id)) workspaceExpansions.delete(id);
    sessionsEl.replaceChildren();
    let matched = 0;
    for (const workspace of lastWorkspaces) {
      const groupMatches = searchable([workspace.name, workspace.current_directory].join(' ')).includes(query);
      const sessions = (workspace.sessions || []).filter(session => !query || groupMatches || matchesSession(session, workspace, query));
      if (query && !groupMatches && !sessions.length) continue;
      matched++;
      const expanded = !!query || (workspaceExpansions.has(workspace.id) ? workspaceExpansions.get(workspace.id) : workspace.state === 'active');
      const group = document.createElement('li'); group.className = 'ws-group';
      const head = document.createElement('button'); head.type = 'button'; head.className = 'ws-head';
      head.dataset.workspaceId = workspace.id; head.setAttribute('aria-expanded', String(expanded));
      const label = document.createElement('span'); label.className = 'ws-label';
      const name = document.createElement('strong'); name.className = 'ws-name'; name.textContent = workspace.name || workspace.id;
      const count = document.createElement('small'); count.textContent = (workspace.sessions || []).length + '개 세션';
      label.append(name, count);
      const state = document.createElement('span'); state.className = 'ws-state ' + (workspace.state || 'suspended');
      state.textContent = workspace.state === 'active' ? '현재' : WORKSPACE_STATE_LABEL[workspace.state] || '절전';
      const chevron = document.createElement('span'); chevron.className = 'ws-chevron'; chevron.textContent = expanded ? '⌄' : '›'; chevron.setAttribute('aria-hidden', 'true');
      head.append(label, state, chevron);
      head.addEventListener('click', () => {
        workspaceExpansions.set(workspace.id, !expanded); renderWorkspaceList();
      });
      group.append(head);
      const body = document.createElement('div'); body.hidden = !expanded;
      if (workspace.current_directory) {
        const path = document.createElement('p'); path.className = 'ws-path'; path.textContent = workspace.current_directory; body.append(path);
      }
      const list = document.createElement('ul'); list.className = 'ws-sessions';
      for (const session of sessions) list.append(sessionRow(session, workspace));
      body.append(list);
      if (!sessions.length) {
        const empty = document.createElement('p'); empty.className = 'workspace-note'; empty.textContent = '세션 없음'; body.append(empty);
      }
      if (workspace.state !== 'active') {
        const note = document.createElement('p'); note.className = 'workspace-note';
        note.textContent = '세션을 누르면 Mac 워크스페이스도 전환됩니다.'; body.append(note);
      }
      group.append(body); sessionsEl.append(group);
    }
    sessionsEmpty.hidden = matched > 0;
    sessionsEmpty.textContent = query ? '검색 결과가 없습니다.' : '연결할 워크스페이스가 없습니다.';
    if (focused?.session) sessionsEl.querySelector(`[data-session-id="${CSS.escape(focused.session)}"]`)?.focus({ preventScroll: true });
    else if (focused?.workspace) sessionsEl.querySelector(`[data-workspace-id="${CSS.escape(focused.workspace)}"]`)?.focus({ preventScroll: true });
  }

  function sessionRow(session, workspace, inSheet = false) {
    const item = document.createElement('li');
    const row = document.createElement('button'); row.type = 'button'; row.className = 'session view-btn m-session-row';
    if (session.id) row.dataset.sessionId = session.id;
    if (session.selection_id) row.dataset.selectionId = session.selection_id;
    const dormant = workspace.state !== 'active';
    const status = session.exited ? 'done' : dormant ? 'dormant' : session.status || 'idle';
    const dot = document.createElement('span'); dot.className = 'session-dot ' + status; dot.setAttribute('aria-hidden', 'true');
    const nameBox = document.createElement('span'); nameBox.className = 'session-name';
    const title = document.createElement('strong'); title.className = 'title'; title.textContent = session.title || '세션';
    const meta = document.createElement('span'); meta.className = 'session-meta';
    const agent = document.createElement('span'); agent.className = 'agent'; agent.textContent = session.agent || '터미널';
    const badge = document.createElement('span'); badge.className = 'session-status ' + status; badge.textContent = sessionStatusLabel(session, workspace);
    meta.append(badge, document.createTextNode(' · '), agent); nameBox.append(title, meta);
    const arrow = document.createElement('span'); arrow.className = 'row-arrow'; arrow.setAttribute('aria-hidden', 'true');
    arrow.textContent = session.id === viewer.watching ? '✓' : '›';
    if (session.id === viewer.watching) row.setAttribute('aria-current', 'true');
    row.disabled = !!session.exited || (!dormant && !session.id);
    row.append(dot, nameBox, arrow);
    row.addEventListener('click', () => {
      const freshWorkspace = lastWorkspaces.find(candidate => candidate.id === workspace.id);
      if (!freshWorkspace) return;
      if (freshWorkspace.state !== 'active') {
        workspaceExpansions.set(workspace.id, true);
        selectDormantSession(freshWorkspace, session);
        return;
      }
      const fresh = freshWorkspace.sessions?.find(candidate => candidate.id === session.id);
      if (!fresh?.id || fresh.exited) return;
      if (inSheet) closeSessionSheet(false);
      if (fresh.id !== viewer.watching) openViewer(fresh.id, fresh.title || '세션');
      else if (inSheet) viewerSessionChip.focus({ preventScroll: true });
      renderWorkspaceList();
    });
    item.append(row); return item;
  }

  // P3: iOS 설치 안내 — iOS Safari이고 아직 설치(standalone) 전일 때만 노출. 닫으면 억제한다.
  function maybeShowIosInstallHint() {
    const el = document.getElementById('ios-install');
    if (!el) return;
    // iPadOS 13+는 기본 "데스크톱 사이트 요청"으로 UA가 macOS로 위장한다 — UA만 보면 대다수
    // iPad를 놓친다. MacIntel + 멀티터치를 보조 판별로 더해 iPad를 잡는다(P3 리뷰).
    const isIos = /iphone|ipad|ipod/i.test(navigator.userAgent)
      || (navigator.platform === 'MacIntel' && navigator.maxTouchPoints > 1);
    const standalone = window.matchMedia('(display-mode: standalone)').matches
      || window.navigator.standalone === true;
    if (!isIos || standalone || localStorage.getItem(IOS_HINT_KEY)) return;
    el.hidden = false;
    const close = document.getElementById('ios-install-close');
    if (close) {
      close.addEventListener('click', () => {
        el.hidden = true;
        localStorage.setItem(IOS_HINT_KEY, '1');
      });
    }
  }

  // P4: 알림 권한 유도 + 웹푸시 구독 — 앱 아이콘 뱃지(iOS 설치형)·푸시는 알림 권한 승인 후에만
  // 동작한다. 권한이 아직 미결정(default)일 때만 버튼을 노출한다. 같은 클릭 제스처 안에서
  // 권한 요청 → 허용 시 pushManager.subscribe → 서버 등록(POST)까지 이어간다.
  function maybeShowNotifyButton() {
    const btn = document.getElementById('notify-enable');
    if (!btn) return;
    if (!('Notification' in window) || Notification.permission !== 'default') return;
    btn.hidden = false;
    btn.addEventListener('click', () => {
      // iOS/WebKit은 사용자 제스처(클릭 핸들러) 안에서만 권한 요청을 허용한다.
      requestNotificationPermission().then((permission) => {
        btn.hidden = true;
        if (permission === 'granted') enablePush(localStorage.getItem(TOKEN_KEY));
      });
    });
  }

  // 권한 요청을 Promise로 정규화한다 — iOS 16.4+는 Promise형, 구형 Safari는 콜백형.
  function requestNotificationPermission() {
    try {
      const result = Notification.requestPermission();
      if (result && typeof result.then === 'function') return result;
      return new Promise((resolve) => {
        Notification.requestPermission((perm) => resolve(perm));
      });
    } catch {
      return Promise.resolve('denied');
    }
  }

  // P4: 웹푸시 구독을 보장하고 서버에 등록한다. 권한이 이미 'granted'일 때 호출한다.
  // iOS 제약: 웹푸시는 **홈 화면 설치형(standalone) + 사용자 제스처 + iOS 16.4+** 에서만
  // 동작한다. 데스크톱 알림(notify-rust)과는 중복 억제하지 않는다(다른 기기).
  async function enablePush(token) {
    if (!token) return;
    if (!('serviceWorker' in navigator) || !('PushManager' in window)) return;
    try {
      const reg = await navigator.serviceWorker.ready;
      let sub = await reg.pushManager.getSubscription();
      if (!sub) {
        // VAPID 공개키를 서버에서 받아 구독한다(applicationServerKey).
        const res = await fetch('/push/vapid?token=' + encodeURIComponent(token));
        if (!res.ok) return; // 푸시 비활성(키 없음) — 조용히 종료
        const body = await res.json();
        if (!body || !body.key) return;
        sub = await reg.pushManager.subscribe({
          userVisibleOnly: true,
          applicationServerKey: urlBase64ToUint8Array(body.key),
        });
      }
      const json = sub.toJSON(); // { endpoint, keys: { p256dh, auth } }
      if (!json || !json.endpoint || !json.keys) return;
      await fetch('/push/subscribe?token=' + encodeURIComponent(token), {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ endpoint: json.endpoint, keys: json.keys }),
      });
    } catch {
      // 구독 실패(권한/브라우저 제약)는 무시 — 다음 방문에 재시도한다.
    }
  }

  // base64url VAPID 공개키를 pushManager가 요구하는 Uint8Array로 변환한다.
  function urlBase64ToUint8Array(base64Url) {
    const padding = '='.repeat((4 - (base64Url.length % 4)) % 4);
    const base64 = (base64Url + padding).replace(/-/g, '+').replace(/_/g, '/');
    const raw = atob(base64);
    const out = new Uint8Array(raw.length);
    for (let i = 0; i < raw.length; i++) out[i] = raw.charCodeAt(i);
    return out;
  }

  // 탭 백그라운드 시 스트림 정지(서버 접속 종료 → 0연결 예산 준수). 포그라운드 복귀 시 재연결.
  function projectVisibility() {
    if (document.hidden) {
      setPrivacyCurtain(true);
      setViewerConnection('paused');
      disconnect();
      setStatus('', '일시정지(백그라운드)');
    } else {
      setPrivacyCurtain(false);
      setViewerConnection('connecting');
      connect();
    }
  }

  document.addEventListener('visibilitychange', projectVisibility);
  if (document.hidden) projectVisibility();
  else connect();
})();
