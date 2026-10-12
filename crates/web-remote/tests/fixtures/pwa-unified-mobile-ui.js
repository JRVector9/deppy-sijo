// Isolated production-shell regression: this script precedes the real core/app module.
(() => {
  localStorage.setItem('deppy.webToken', 'unified-mobile-isolated');
  localStorage.removeItem('deppy-viewer:prefs');
  globalThis.unifiedMobileSent = [];
  globalThis.unifiedMobileErrors = [];
  globalThis.unifiedMobileSendFails = false;
  globalThis.unifiedMobileHistoryFailures = 1;
  globalThis.unifiedMobileHoldHistory = true;
  globalThis.unifiedMobileHeldHistory = [];
  window.addEventListener('error', event => unifiedMobileErrors.push(event.message));
  class TestViewport extends EventTarget {
    width = 390; height = 844; offsetTop = 0; offsetLeft = 0; scale = 1;
  }
  globalThis.unifiedMobileViewport = new TestViewport();
  Object.defineProperty(window, 'visualViewport', { get: () => unifiedMobileViewport });
  globalThis.unifiedMobileWorkspaces = [
    { id: 'ws-active', name: '현재 프로젝트', current_directory: '/projects/한글', state: 'active', sessions: [
      { id: 'session-a', title: '모바일 통합 입력과 긴 제목의 줄바꿈을 검증하는 세션 · 세션별 초안과 한글 경로 보존', agent: 'Codex · GPT', task: '검토해 PF별 개선사항 보고', status: 'running', exited: false },
      { id: 'session-b', title: '한글 경로 입력', agent: 'Claude', status: 'waiting', exited: false },
    ] },
    { id: 'ws-warm', name: '대기 프로젝트', current_directory: '/projects/archive', state: 'warm', sessions: [
      { selection_id: 'stable-warm', title: '저장된 작업', agent: 'Saved agent', status: 'running', exited: false },
    ] },
  ];
  class FakeSocket {
    static OPEN = 1; static CONNECTING = 0;
    readyState = 0; listeners = new Map(); sent = [];
    constructor() {
      globalThis.unifiedMobileSocket = this;
      setTimeout(() => { this.readyState = 1; this.emit('open'); }, 5);
    }
    addEventListener(type, fn) { this.listeners.set(type, [...(this.listeners.get(type) || []), fn]); }
    emit(type, value = {}) { for (const fn of this.listeners.get(type) || []) fn(value); }
    message(frame) { this.emit('message', { data: JSON.stringify(frame) }); }
    dashboard() { this.message({ type: 'dashboard', workspaces: unifiedMobileWorkspaces }); }
    viewport(session, request, requestedRows = 4, reset = true, anchor, delta = 0, cols = 80, nativeRows = 4) {
      const rows = Math.max(1, Math.min(256, requestedRows));
      const first = reset ? 10020n - BigInt(rows) : BigInt(anchor?.first_line || 10016) - BigInt(delta);
      const tail = first + BigInt(rows);
      this.message({ type: 'viewport', session, seq: request || 1, keyframe: true, cols, rows,
        offset: Number(10020n - tail), alt: false, cursor: { row: rows - 1, col: 2, visible: true },
        lines: Array.from({ length: rows }, (_, row) => ({ row, runs: [{ s: 0, t: `${first + BigInt(row)} 한글 terminal line` }] })),
        history: { generation: '1', first_line: String(first), live_first_line: String(10020n - BigInt(nativeRows)), native_rows: nativeRows,
          total_lines: 5000, request, expired: false } });
    }
    send(raw) {
      const frame = JSON.parse(raw);
      if (unifiedMobileSendFails && ['input', 'direct_key'].includes(frame.type)) throw Error('isolated send failed');
      unifiedMobileSent.push(frame); this.sent.push(frame);
      if (frame.type === 'auth') setTimeout(() => { this.message({ type: 'welcome', v: frame.v }); this.dashboard(); }, 5);
      if (frame.type === 'watch') setTimeout(() => this.viewport(frame.session), 5);
      if (frame.type === 'resize_control') setTimeout(() => this.message({ type: 'terminal_control', session: frame.session,
        request: frame.request, owned: frame.action === 'acquire', reason: frame.action === 'acquire' ? 'owned' : 'released' }), 5);
      if (frame.type === 'resize') setTimeout(() => this.viewport(frame.session, undefined, frame.rows, true, undefined, 0, frame.cols, frame.rows), 5);
      if (frame.type === 'scroll' && unifiedMobileHoldHistory) { unifiedMobileHeldHistory.push(frame); return; }
      if (frame.type === 'scroll') setTimeout(() => {
        if (unifiedMobileHistoryFailures > 0) {
          unifiedMobileHistoryFailures--;
          this.message({ type: 'history_error', session: frame.session, request: frame.request, reason: 'timeout' });
          this.viewport(frame.session); // Same native geometry and desired row count after a failed fill.
        } else this.viewport(frame.session, frame.request, frame.rows || 4, frame.reset, frame.anchor, frame.delta);
      }, 5);
    }
    close() { this.readyState = 3; this.emit('close'); }
  }
  globalThis.WebSocket = FakeSocket;
})();

(async () => {
  const $ = id => document.getElementById(id);
  const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
  const waitFor = async (predicate, label) => {
    for (let attempt = 0; attempt < 120; attempt++) { if (predicate()) return; await sleep(20); }
    throw Error('timeout: ' + label);
  };
  let checks = 0;
  const check = (value, label) => { if (!value) throw Error(label); checks++; };
  const inputFrames = () => unifiedMobileSent.filter(frame => frame.type === 'input');
  const type = value => { $('composer-text').value = value; $('composer-text').dispatchEvent(new InputEvent('input', { bubbles: true, inputType: 'insertText' })); };
  const key = (key, options = {}) => {
    const event = new KeyboardEvent('keydown', { key, bubbles: true, cancelable: true, ...options });
    $('composer-text').dispatchEvent(event); return event;
  };
  const selectSession = async id => {
    $('viewer-session-chip').click();
    $('viewer-session-list').querySelector(`[data-session-id="${id}"]`).click();
    await waitFor(() => !$('composer-text').disabled && $('viewer-session-chip').textContent === unifiedMobileWorkspaces[0].sessions.find(session => session.id === id).title, 'session ready');
  };
  try {
    await waitFor(() => document.querySelector('#sessions [data-session-id="session-a"]'), 'dashboard');
    check($('workspace-search').getAttribute('type') === 'search', 'workspace search exists');
    // Headless window-size can clamp the layout viewport to500px. Constrain the
    // actual containing block to prove narrow rows independently of screenshot crops.
    const originalDocumentWidth = document.documentElement.style.width;
    for (const width of [320,390]) {
      document.documentElement.style.width = width + 'px'; await sleep(20);
      const appBounds = $('dashboard-shell').getBoundingClientRect();
      check(Math.abs(appBounds.width - width) < 1, `${width}px workspace fills its actual containing block`);
      const children = document.querySelectorAll('#dashboard-shell .brand, #dashboard-shell .m-topbar-actions, #sessions .ws-head, #sessions .ws-state, #sessions .ws-chevron, #sessions .session, #sessions .row-arrow');
      check(Array.from(children).every(node => { const rect = node.getBoundingClientRect(); return rect.left >= appBounds.left - 1 && rect.right <= appBounds.right + 1; }), `${width}px workspace header/state/session arrows remain inside shell`);
      const narrowRow = document.querySelector('#sessions [data-session-id="session-a"]');
      check(Math.abs(narrowRow.getBoundingClientRect().width - width) < 1 && narrowRow.getBoundingClientRect().height >= 76, `${width}px session has full-width76px touch bounds`);
      check(narrowRow.querySelector('.title').getBoundingClientRect().height === 40, `${width}px long session title actually wraps to two14px lines`);
      check($('sessions').scrollWidth <= $('sessions').clientWidth + 1, `${width}px session list has no horizontal overflow`);
    }
    document.documentElement.style.width = originalDocumentWidth; await sleep(20);
    const activeHead = document.querySelector('[data-workspace-id="ws-active"]');
    check(activeHead.getAttribute('aria-expanded') === 'true', 'active expands by default');
    const warmHead = document.querySelector('[data-workspace-id="ws-warm"]');
    check(warmHead.getAttribute('aria-expanded') === 'false', 'dormant collapses by default');
    warmHead.click(); unifiedMobileSocket.dashboard();
    check(document.querySelector('[data-workspace-id="ws-warm"]').getAttribute('aria-expanded') === 'true', 'expansion survives refresh');
    const dormantStatus = document.querySelectorAll('#sessions .session-status')[2];
    check(dormantStatus.textContent.includes('저장된') && !dormantStatus.textContent.includes('실행 중'), 'saved status never claims live running');
    const search = $('workspace-search'); search.value = '한글'; search.dispatchEvent(new Event('input'));
    unifiedMobileSocket.dashboard();
    check(search.value === '한글' && document.querySelectorAll('#sessions .ws-group').length === 1, 'NFC path search survives refresh');
    search.value = 'Saved agent'; search.dispatchEvent(new Event('input'));
    check(document.querySelectorAll('#sessions .session').length === 1, 'agent search finds saved session');
    document.querySelector('#sessions .session').click();
    check(unifiedMobileSent.at(-1).type === 'switch' && unifiedMobileSent.at(-1).workspace === 'ws-warm', 'dormant row requests exact workspace only');
    check(!unifiedMobileSent.some(frame => frame.type === 'watch'), 'dormant saved title never chooses guessed session');
    search.value = ''; search.dispatchEvent(new Event('input'));
    unifiedMobileWorkspaces[0].name = '긴워크스페이스이름'.repeat(40);
    unifiedMobileSocket.dashboard();
    window.scrollTo(1000, 0);
    check(window.scrollX === 0 && document.documentElement.scrollWidth <= window.innerWidth, 'long workspace content cannot pan the document horizontally');
    const row = document.querySelector('#sessions [data-session-id="session-a"]');
    check(row.getBoundingClientRect().height >= 76, 'session touch row at least76px');
    check(getComputedStyle(row.querySelector('.title')).fontSize === '14px', 'session title14px');
    check(getComputedStyle(row.querySelector('.agent')).fontSize === '12px', 'agent12px');
    check(row.querySelector('.session-meta').firstElementChild.classList.contains('session-status'), 'status precedes agent as approved');
    check(row.querySelector('.session-task')?.textContent === '검토해 PF별 개선사항 보고', 'agent task line shows under the session');
    check(getComputedStyle(row.querySelector('.session-task')).whiteSpace === 'nowrap', 'task line stays one line');
    check(getComputedStyle(row.querySelector('.title')).webkitLineClamp === '2', 'title max2lines');
    row.click(); await waitFor(() => !$('composer-text').disabled, 'viewer ready');
    const warm = unifiedMobileWorkspaces[1], savedWarm = structuredClone(warm);
    warm.state = 'active'; warm.sessions = [{ id: 'stable-warm', selection_id: 'stable-warm', title: '저장된 작업', agent: 'Saved agent', status: 'running', exited: false }];
    unifiedMobileSocket.dashboard(); await sleep(20);
    check(!unifiedMobileSent.some(frame => frame.type === 'watch' && frame.session === 'stable-warm'), 'manual session navigation cancels pending dormant handoff');
    Object.assign(warm, savedWarm); unifiedMobileSocket.dashboard();
    check(!$('viewer-search-text') && !document.querySelector('.viewer-search-controls'), 'loaded-screen search has no menu DOM');
    check(document.querySelectorAll('#viewer textarea').length === 1, 'single visible buffered input DOM');
    check(!$('viewer-mode-direct') && !$('direct-text'), 'mode split removed');
    check(getComputedStyle($('composer-text')).fontSize === '16px', 'input16px');
    check($('composer-text').getBoundingClientRect().height === 44, 'empty input44px');
    check($('viewer-session-chip').getBoundingClientRect().width <= $('viewer').getBoundingClientRect().width * .42 + 1, 'header session picker max42percent of actual visual viewer width');
    const pickerStyle = getComputedStyle($('viewer-session-chip'));
    const pickerChevron = getComputedStyle($('viewer-session-chip'), '::after');
    check(pickerChevron.content === '"⌄"' && pickerChevron.position === 'absolute' && pickerChevron.pointerEvents === 'none', 'session picker displays the approved dropdown chevron');
    check(parseFloat(pickerStyle.paddingRight) >= parseFloat(pickerChevron.right) + parseFloat(pickerChevron.width) + 4 && pickerStyle.textOverflow === 'ellipsis', 'long session title reserves space for its visible chevron');
    check($('viewer-menu-button').getBoundingClientRect().width === 44, 'header settings touch button remains44px without flex shrink');
    await waitFor(() => unifiedMobileSent.some(frame => frame.type === 'scroll' && frame.rows > 4), 'bounded height fill');
    await waitFor(() => unifiedMobileHeldHistory.length === 1, 'first layout held before initial fill');
    const retiredSocket = unifiedMobileSocket, retiredLayout = unifiedMobileHeldHistory[0];
    type('재연결 초안'); retiredSocket.close();
    await waitFor(() => unifiedMobileSocket !== retiredSocket && unifiedMobileSocket.readyState === 1 && !$('composer-text').disabled, 'fresh socket native viewport');
    const freshSocket = unifiedMobileSocket;
    await waitFor(() => freshSocket.sent.some(frame => frame.type === 'scroll' && frame.rows > 4), 'unchanged dimensions re-notify desired rows after rewatch');
    const freshLayout = freshSocket.sent.find(frame => frame.type === 'scroll' && frame.rows > 4);
    check($('composer-text').value === '재연결 초안' && inputFrames().length === 0, 'before-fill reconnect preserves draft without submitting it');
    retiredSocket.viewport(retiredLayout.session, retiredLayout.request, 256, true);
    await sleep(20);
    check(document.querySelectorAll('.viewer-text-row').length === 4, 'retired socket layout reply cannot fill fresh context');
    check(freshLayout.rows === retiredLayout.rows && unifiedMobileViewport.width === 390 && unifiedMobileViewport.height === 844, 'fresh row request survives unchanged geometry dedupe');
    unifiedMobileHoldHistory = false; unifiedMobileHistoryFailures = 0;
    freshSocket.message({ type: 'history_error', session: freshLayout.session, request: freshLayout.request, reason: 'timeout' });
    freshSocket.viewport(freshLayout.session);
    await waitFor(() => document.querySelectorAll('.viewer-text-row').length > 4, 'bounded fill retries after initial history_error');
    const requests = unifiedMobileSent.filter(frame => frame.type === 'scroll');
    const freshRequests = freshSocket.sent.filter(frame => frame.type === 'scroll');
    check(freshRequests.length >= 2 && freshRequests[0].rows === freshRequests[1].rows && freshRequests[1].reset, 'failed initial layout fill retries same rows and live intent');
    unifiedMobileHeldHistory.length = 0; type('');
    check(requests.every(frame => frame.rows <= 256), 'rows bounded256');
    check(!unifiedMobileSent.some(frame => frame.type === 'resize'), 'history fill never resizes PTY');
    check($('viewer-scroll-note').hidden, 'following has no history rail');
    await sleep(130); const beforeBurst = unifiedMobileSent.filter(frame => frame.type === 'scroll').length;
    for (let index = 0; index < 3; index++) { $('viewer-font-larger').click(); await sleep(30); }
    await sleep(180);
    const burst = unifiedMobileSent.filter(frame => frame.type === 'scroll').slice(beforeBurst);
    check(burst.length === 1 && burst[0].rows <= 256, 'rapid font layout changes debounce to latest bounded row request');
    for (let index = 0; index < 3; index++) $('viewer-font-smaller').click(); await sleep(180);
    unifiedMobileHoldHistory = true;
    $('viewer-font-larger').click();
    await waitFor(() => unifiedMobileHeldHistory.length > 0, 'held automatic layout request');
    const heldLayout = unifiedMobileHeldHistory[0];
    check(heldLayout.reset === true, 'automatic layout initially requests live rows');
    const heldWrap = document.querySelector('.viewer-wrap');
    heldWrap.dispatchEvent(new WheelEvent('wheel', { deltaX: 80, deltaY: -60, bubbles: true, cancelable: true }));
    heldWrap.scrollTop = Math.max(0, heldWrap.scrollTop - 60);
    heldWrap.dispatchEvent(new Event('scroll'));
    unifiedMobileSocket.message({ type: 'history_error', session: heldLayout.session, request: heldLayout.request, reason: 'timeout' });
    await waitFor(() => unifiedMobileHeldHistory.length === 2, 'one bounded layout retry after held failure');
    const pinnedRetry = unifiedMobileHeldHistory[1];
    check(pinnedRetry.reset === false && pinnedRetry.anchor?.first_line, 'user wheel while layout pending revokes live-follow retry intent');
    unifiedMobileHoldHistory = false;
    unifiedMobileSocket.viewport(pinnedRetry.session, pinnedRetry.request, pinnedRetry.rows, pinnedRetry.reset, pinnedRetry.anchor, pinnedRetry.delta);
    await sleep(30);
    check(!$('viewer-scroll-note').hidden, 'failure retry preserves paused reading state');
    $('viewer-bottom').click(); await waitFor(() => $('viewer-scroll-note').hidden, 'explicit bottom resumes after pinned retry');
    $('viewer-font-smaller').click(); await sleep(180);
    type('초안\n'.repeat(100));
    check($('composer-text').getBoundingClientRect().height <= 132, 'long draft capped5lines');
    check($('composer-text').scrollHeight > $('composer-text').clientHeight, 'long draft internally scrolls');
    check(inputFrames().length === 0, 'typing never directly sends draft');
    check(!key('Enter', { shiftKey: true }).defaultPrevented, 'ShiftEnter stays newline');
    const beforeKeys = inputFrames().length;
    $('viewer-quick-button').click();
    check($('viewer-quick-button').getAttribute('aria-expanded') === 'true', 'accessory toggle expanded');
    document.querySelector('[data-key="enter"]').click();
    check(unifiedMobileSent.at(-1).type === 'direct_key' && unifiedMobileSent.at(-1).key === 'enter', 'supplementary Enter sends only key');
    check(inputFrames().length === beforeKeys && $('composer-text').value.startsWith('초안'), 'special key retains unsent draft');
    $('direct-ctrl').click(); key('c');
    check(unifiedMobileSent.at(-1).ctrl === true && unifiedMobileSent.at(-1).key === 'c', 'armed Ctrl maps next ASCII key');
    check($('composer-text').value.startsWith('초안'), 'Ctrl never flushes draft');
    type('한글 /tmp/경로'); key('Enter');
    check(inputFrames().at(-1).text === '한글 /tmp/경로' && inputFrames().at(-1).submit === true, 'explicit Enter NFC buffered submit');
    check($('composer-text').value === '', 'successful send clears only captured draft');
    type('한'.repeat(100000)); const tooLarge = $('composer-text').value, beforeLarge = inputFrames().length;
    $('composer-send').click();
    check(inputFrames().length === beforeLarge && $('composer-text').value === tooLarge, 'oversized Korean draft rejected without losing text');
    type('조합 중'); $('composer-text').dispatchEvent(new CompositionEvent('compositionstart', { bubbles: true }));
    const beforeIme = inputFrames().length;
    key('Enter', { isComposing: true, keyCode: 229 }); $('composer-send').click();
    check(inputFrames().length === beforeIme, 'IME Enter and send never submit candidate');
    $('composer-text').dispatchEvent(new CompositionEvent('compositionend', { bubbles: true })); key('Enter');
    check(inputFrames().length === beforeIme, 'compositionend Enter protected until settled');
    await sleep(20); key('Enter'); check(inputFrames().length === beforeIme + 1, 'settled explicit Enter submits once');
    type('조합 중 interrupt 초안'); const interruptedInput = $('composer-text');
    interruptedInput.dispatchEvent(new CompositionEvent('compositionstart', { bubbles: true }));
    const beforeInterrupt = inputFrames().length;
    document.querySelector('[data-key="ctrl_c"]').click();
    check(unifiedMobileSent.at(-1).type === 'direct_key' && unifiedMobileSent.at(-1).ctrl && unifiedMobileSent.at(-1).key === 'c', 'explicit CtrlC interrupts during IME');
    check(inputFrames().length === beforeInterrupt && $('composer-text').value === '조합 중 interrupt 초안', 'IME interrupt retains buffered candidate without submit');
    interruptedInput.value = 'stale interruption tail'; interruptedInput.dispatchEvent(new InputEvent('input', { bubbles: true }));
    check($('composer-text').value === '조합 중 interrupt 초안', 'late retired interrupt input ignored');
    type('긴 문장 '.repeat(1000));
    const preserved = $('composer-text').value, oldElement = $('composer-text');
    oldElement.dispatchEvent(new CompositionEvent('compositionstart', { bubbles: true }));
    await selectSession('session-b');
    oldElement.value = 'late wrong-session text'; oldElement.dispatchEvent(new InputEvent('input', { bubbles: true }));
    check($('composer-text').value === '', 'late old IME cannot overwrite other session');
    type('두 번째 세션 초안'); await selectSession('session-a');
    check($('composer-text').value === preserved, 'long per-session draft restored');
    await selectSession('session-b'); check($('composer-text').value === '두 번째 세션 초안', 'other session draft independent');
    $('viewer-session-chip').click();
    check(!$('viewer-session-sheet').hidden && $('viewer-stage').inert, 'session bottom sheet makes underlying viewer inert');
    $('viewer-session-sheet').dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true, cancelable: true }));
    check($('viewer-session-sheet').hidden && !$('viewer-stage').inert, 'Escape closes and removes inert');
    check(document.activeElement === $('viewer-session-chip'), 'sheet focus restored');
    $('viewer-menu-button').click();
    const settings = $('viewer-menu').querySelector('[role="dialog"]');
    check(!$('viewer-menu').hidden && getComputedStyle(settings).maxHeight === '80%', 'settings bottom panel bounded');
    const connectionLabel = $('viewer-menu-status').previousElementSibling.getBoundingClientRect();
    check($('viewer-menu-status').getBoundingClientRect().left >= connectionLabel.right + 7, 'settings connection label and value have readable spacing');
    check(settings.getBoundingClientRect().bottom <= $('viewer').getBoundingClientRect().bottom + 1, 'settings stays inside viewport');
    $('viewer-menu').click(); check($('viewer-menu').hidden, 'backdrop closes settings');
    type('전송 실패 보호'); unifiedMobileSendFails = true; $('composer-send').click(); unifiedMobileSendFails = false;
    check($('composer-text').value === '전송 실패 보호', 'send exception preserves draft');
    $('composer-send').click();
    unifiedMobileSocket.message({ type: 'input_pressure', session: 'session-b', reason: 'queue_full', queued: 1 });
    check($('composer-text').value.includes('전송 실패 보호'), 'rejected send restored to owner draft');
    check($('composer-send').disabled, 'pressure blocks further submit');
    unifiedMobileSocket.message({ type: 'input_pressure', session: 'session-b', reason: 'queue_full', queued: 0 });
    type('연결 변경 보호'); const beforeReconnect = inputFrames().length;
    unifiedMobileSocket.close();
    check($('composer-text').value === '연결 변경 보호' && $('composer-send').disabled, 'disconnect preserves buffered draft and blocks submit');
    await waitFor(() => unifiedMobileSocket.readyState === 1 && !$('composer-text').disabled, 'reconnect');
    check(inputFrames().length === beforeReconnect && $('composer-text').value === '연결 변경 보호', 'reconnect never auto-sends draft');
    unifiedMobileViewport.height = 410; unifiedMobileViewport.offsetTop = 30;
    unifiedMobileViewport.dispatchEvent(new Event('resize')); await sleep(350);
    check($('viewer').getBoundingClientRect().height === 410 && $('viewer').getBoundingClientRect().top === 30, 'keyboard visualViewport bounds honored');
    check($('composer-text').getBoundingClientRect().height >= 44, 'keyboard keeps unified touch input');
    unifiedMobileViewport.height = 844; unifiedMobileViewport.offsetTop = 0; unifiedMobileViewport.dispatchEvent(new Event('resize')); await sleep(350);
    for (const [width, height] of [[320,568],[390,844],[844,390]]) {
      unifiedMobileViewport.width = width; unifiedMobileViewport.height = height;
      unifiedMobileViewport.dispatchEvent(new Event('resize')); await sleep(350);
      const bounds = $('viewer').getBoundingClientRect();
      check(bounds.width === width && bounds.height === height, `${width}x${height} viewport shell fills available bounds`);
      check($('viewer-session-chip').getBoundingClientRect().width <= width * .42 + 1 && $('viewer-menu-button').getBoundingClientRect().width === 44, `${width}x${height} header picker and settings retain approved widths`);
      check($('composer-text').getBoundingClientRect().bottom <= bounds.bottom + 1, `${width}x${height} input stays inside shell`);
      const head = document.querySelector('.viewer-header');
      check(head.getBoundingClientRect().height <= 45.1, `${width}x${height} compact terminal header`);
      const titleRect = $('viewer-title').getBoundingClientRect(), statusRect = $('viewer-connection').getBoundingClientRect();
      check(Math.abs((titleRect.top + titleRect.bottom) / 2 - (statusRect.top + statusRect.bottom) / 2) < 1, `${width}x${height} workspace and connection share one line`);
      check($('viewer-stage').getBoundingClientRect().height > 0, `${width}x${height} terminal keeps positive height`);
    }
    unifiedMobileViewport.width = 390; unifiedMobileViewport.height = 844; unifiedMobileViewport.dispatchEvent(new Event('resize')); await sleep(350);
    await selectSession('session-a');
    unifiedMobileSocket.viewport('session-a', undefined, 200); await sleep(80);
    $('viewer-overview').click(); await sleep(80);
    const fitCanvas = $('viewer-canvas'), fitWrap = document.querySelector('.viewer-wrap');
    check(Math.abs(fitCanvas.getBoundingClientRect().width - fitWrap.clientWidth) < 1, 'long history fits the full phone width');
    check(fitCanvas.getBoundingClientRect().height > fitWrap.clientHeight && fitWrap.scrollHeight > fitWrap.clientHeight, 'long history scrolls vertically without shrinking to its height');
    unifiedMobileViewport.width = 320; unifiedMobileViewport.height = 568; unifiedMobileViewport.dispatchEvent(new Event('resize')); await sleep(350);
    while (!$('viewer-font-larger').disabled) $('viewer-font-larger').click(); await sleep(100);
    const beforePhoneFit = unifiedMobileSent.length;
    $('viewer-resize-control').click();
    await waitFor(() => unifiedMobileSent.slice(beforePhoneFit).some(frame => frame.type === 'resize'), 'phone-fit granted geometry');
    await sleep(350);
    const phoneResize = unifiedMobileSent.slice(beforePhoneFit).find(frame => frame.type === 'resize');
    const phoneWrap = document.querySelector('.viewer-wrap');
    const phoneWidth = $('viewer-canvas').getBoundingClientRect().width;
    check(phoneResize.cols >= 32 && phoneResize.session === 'session-a', 'phone-fit retains the exact native session and minimum columns');
    check($('viewer-overview').getAttribute('aria-pressed') === 'false' && $('viewer-readable-wrap').getAttribute('aria-pressed') === 'false', 'phone fit exits stale presentation modes');
    check(phoneWidth <= phoneWrap.clientWidth + 1 && phoneWidth >= phoneWrap.clientWidth - phoneWidth / phoneResize.cols - 1, 'owned phone geometry fills available width within one native cell');
    $('viewer-resize-control').click(); await sleep(50);
    $('viewer-back').click(); await waitFor(() => $('viewer').hidden, 'close before dormant switch');
    document.querySelector('[data-workspace-id="ws-warm"]').click();
    const dormant = document.querySelector('#sessions [data-selection-id="stable-warm"]');
    if (!dormant.getClientRects().length) document.querySelector('[data-workspace-id="ws-warm"]').click();
    dormant.click();
    const beforeFresh = unifiedMobileSent.filter(frame => frame.type === 'watch').length;
    warm.state = 'active'; warm.sessions = [
      { id: 'wrong-title-match', selection_id: 'wrong-title-match', title: '저장된 작업', status: 'running', exited: false },
      { id: 'stable-warm', selection_id: 'stable-warm', title: '저장된 작업', status: 'running', exited: false },
    ];
    unifiedMobileSocket.dashboard(); await waitFor(() => unifiedMobileSent.some(frame => frame.type === 'watch' && frame.session === 'stable-warm'), 'fresh stable dormant selection');
    const freshWatches = unifiedMobileSent.filter(frame => frame.type === 'watch').slice(beforeFresh);
    check(freshWatches.length === 1 && freshWatches[0].session === 'stable-warm', 'one-click dormant handoff watches only exact stable identity');
    check(!unifiedMobileSent.some(frame => frame.type === 'watch' && frame.session === 'wrong-title-match'), 'duplicate title never routes dormant session');
    unifiedMobileSocket.message({ type: 'input_pressure', session: 'session-b', reason: 'closed', queued: 0 });
    check(!$('composer-text').disabled, 'stale session pressure cannot lock fresh selected session');
    check(unifiedMobileErrors.length === 0, 'no production JavaScript errors: ' + unifiedMobileErrors.join(';'));
    document.body.dataset.status = 'ok'; document.body.textContent = 'PWA_UNIFIED_MOBILE_UI_OK:' + checks;
  } catch (error) {
    document.body.dataset.status = 'error'; document.body.textContent = 'PWA_UNIFIED_MOBILE_UI_ERROR:' + error.stack;
  }
})();
