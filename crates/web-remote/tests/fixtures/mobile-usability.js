(() => {
  localStorage.setItem('deppy.webToken', 'usability-test-token');
  globalThis.mobileUsabilitySent = [];
  globalThis.mobileUsabilityErrors = [];
  globalThis.mobileHistoryAutoReply = true;
  globalThis.mobileInputSendFailure = false;
  window.addEventListener('error', (event) => mobileUsabilityErrors.push(event.message));
  class TestViewport extends EventTarget {
    constructor() {
      super();
      this.width = 390;
      this.height = 844;
      this.offsetTop = 0;
      this.offsetLeft = 0;
      this.scale = 1;
    }
  }
  globalThis.mobileUsabilityViewport = new TestViewport();
  Object.defineProperty(window, 'visualViewport', { get: () => mobileUsabilityViewport });
  class FakeSocket {
    static OPEN = 1;
    static CONNECTING = 0;
    constructor() {
      this.readyState = 0;
      this.listeners = new Map();
      this.liveFrames = new Map();
      this.publishedFrames = new Map();
      globalThis.mobileUsabilitySocket = this;
      setTimeout(() => {
        this.readyState = 1;
        this.emit('open');
      }, 10);
    }
    addEventListener(type, handler) {
      this.listeners.set(type, [...(this.listeners.get(type) || []), handler]);
    }
    emit(type, event = {}) {
      for (const handler of this.listeners.get(type) || []) handler(event);
    }
    message(frame) {
      if (frame.type === 'viewport') {
        // A real source-window change is a keyframe, never a delta decoded against another window.
        const previous = this.publishedFrames.get(frame.session);
        if (!frame.history) frame.history = { generation: '1', first_line: String(10000 - (frame.offset || 0)), total_lines: 5000, expired: false };
        if (!frame.keyframe && previous && (previous.cols !== frame.cols || previous.rows !== frame.rows
            || previous.offset !== frame.offset || previous.history?.first_line !== frame.history.first_line)) {
          frame.keyframe = true;
          if (!frame.lines.length && previous.cols === frame.cols && previous.rows === frame.rows) frame.lines = previous.lines;
        }
        this.publishedFrames.set(frame.session, frame);
      }
      if (frame.type === 'viewport' && !frame.history?.request && frame.offset === 0) {
        this.liveFrames.set(frame.session, { ...this.liveFrames.get(frame.session), ...frame });
      }
      this.emit('message', { data: JSON.stringify(frame) });
    }
    send(raw) {
      const frame = JSON.parse(raw);
      if (mobileInputSendFailure && ['input', 'direct_input', 'direct_key'].includes(frame.type)) throw new Error('send failed');
      mobileUsabilitySent.push(frame);
      if (frame.type === 'auth') {
        setTimeout(() => {
          this.message({ type: 'welcome', v: frame.v });
          this.message({ type: 'dashboard', workspaces: [{
            id: 'w-1', name: 'Mobile usability', state: 'active', sessions: [
              { id: 's-1', title: 'Claude', status: 'running', exited: false },
              { id: 's-2', title: 'Codex', status: 'running', exited: false },
            ],
          }] });
        }, 10);
      }
      if (frame.type === 'watch') {
        setTimeout(() => this.message({
          type: 'viewport', session: frame.session, keyframe: true, cols: 180, rows: 40,
          lines: Array.from({ length: 40 }, (_, row) => ({ row, runs: [
            { s: 0, t: `${row} 한글 /path/to/project ┌────┐ readable terminal`, fg: '#d4d4d4' },
          ] })),
          cursor: { visible: true, row: 2, col: 6 }, offset: 0,
          history: { generation: '1', first_line: '10000', total_lines: 5000, expired: false },
        }), 10);
      }
      if (frame.type === 'scroll' && mobileHistoryAutoReply) {
        const live = this.liveFrames.get(frame.session);
        if (!live) return;
        const base = BigInt(frame.anchor?.first_line || '10000');
        const desired = frame.reset ? 10000n : base - BigInt(frame.delta);
        const first = desired < 5040n ? 5040n : desired > 10000n ? 10000n : desired;
        setTimeout(() => this.message({ ...live, keyframe: true, offset: Number(10000n - first),
          history: { generation: '1', first_line: String(first), total_lines: 5000, request: frame.request, expired: false },
        }), 10);
      }
    }
    close() {
      this.readyState = 3;
      this.emit('close');
    }
  }
  globalThis.WebSocket = FakeSocket;
})();

let usabilityChecks = Number(sessionStorage.getItem('mobile-usability-checks') || 0);
const usabilityCheck = (condition, message) => {
  usabilityChecks++;
  if (!condition) throw new Error(message);
};
const usabilityWait = (condition) => new Promise((resolve, reject) => {
  const deadline = Date.now() + 3000;
  const poll = () => {
    if (condition()) return resolve();
    if (Date.now() > deadline) return reject(new Error('mobile usability condition timed out'));
    setTimeout(poll, 20);
  };
  poll();
});
const usabilitySettle = () => new Promise((resolve) =>
  requestAnimationFrame(() => requestAnimationFrame(() => setTimeout(resolve, 90))));
const usabilityById = (id) => document.getElementById(id);
const usabilityFont = () => Number(usabilityById('viewer-canvas').getContext('2d').font.match(/([\d.]+)px/)[1]);
function usabilitySwitchSession() {
  const current = usabilityById('viewer-session-chip').textContent;
  usabilityById('viewer-session-chip').click();
  const rows = Array.from(document.querySelectorAll('#viewer-session-list [data-session-id]'));
  const next = rows.find(row => row.querySelector('.title')?.textContent !== current);
  if (!next) throw Error('no alternate session in picker');
  next.click();
}

async function runMobileReadingChecks() {
  const wrapToggle = usabilityById('viewer-readable-wrap');
  const search = usabilityById('viewer-search-text');
  const count = usabilityById('viewer-search-count');
  const copy = usabilityById('viewer-copy-selection');
  const note = usabilityById('viewer-reading-status');
  const wrap = document.querySelector('.viewer-wrap');
  const layer = document.querySelector('.viewer-text-layer');
  const sleep = (ms = 100) => new Promise((resolve) => setTimeout(resolve, ms));
  const scrolls = () => mobileUsabilitySent.filter((frame) => frame.type === 'scroll');
  let checkpoint = scrolls().length;
  const take = () => { const result = scrolls().slice(checkpoint); checkpoint = scrolls().length; return result; };
  const frame = (request, first = '10000', text = '한글🙂 <b>Alpha</b> alpha', expired = false) => {
    mobileUsabilitySocket.message({ type: 'viewport', session: 's-1', keyframe: true, cols: 180, rows: 40,
      lines: [{ row: 0, runs: [{ s: 0, t: text, fg: '#d4d4d4' }] }, { row: 1, runs: [{ s: 0, t: 'Beta Alpha', a: 1 }] }],
      cursor: { visible: true, row: 1, col: 1 }, offset: Number(10000n - BigInt(first)),
      history: { generation: '1', first_line: first, total_lines: 5000, ...(request ? { request } : {}), expired },
    });
  };
  const select = () => {
    const cells = layer.querySelector('[data-row="0"]').querySelectorAll('.viewer-text-cell');
    const range = document.createRange(); range.setStart(cells[0].firstChild, 0); range.setEnd(cells[2].firstChild, cells[2].firstChild.length);
    const selection = window.getSelection(); selection.removeAllRanges(); selection.addRange(range);
    document.dispatchEvent(new Event('selectionchange'));
    return selection.toString();
  };
  const find = (value) => { search.value = value; search.dispatchEvent(new Event('input', { bubbles: true })); };
  const pan = (rows) => {
    wrap.scrollTop = rows > 0 ? 0 : wrap.scrollHeight;
    wrap.dispatchEvent(new WheelEvent('wheel', { bubbles: true, cancelable: true, deltaY: -rows, deltaMode: 1 }));
  };
  frame(); await usabilitySettle();
  let copied = [];
  let clipboardOutcome = 'ok';
  let finishClipboard;
  Object.defineProperty(navigator, 'clipboard', { configurable: true, value: { writeText(text) {
    copied.push(text);
    if (clipboardOutcome === 'fail') return Promise.reject(new Error('permission'));
    if (clipboardOutcome === 'pending') return new Promise((resolve) => { finishClipboard = resolve; });
    return Promise.resolve();
  } } });
  const selected = select(); copy.click(); await sleep(20);
  usabilityCheck(selected === '한글🙂' && copied[0] === selected && usabilityById('viewer-copy-status').textContent.includes('복사했습니다'),
    'explicit copy uses exactly the selected Unicode terminal text');
  find('alpha'); usabilityCheck(count.textContent === '1 / 3', 'literal case-insensitive search counts loaded rows');
  usabilityById('viewer-search-next').click(); usabilityCheck(count.textContent === '2 / 3', 'search next advances one loaded match');
  usabilityById('viewer-search-prev').click(); usabilityCheck(count.textContent === '1 / 3', 'search previous returns to loaded match');
  usabilityCheck(window.getSelection().toString() === selected && usabilityById('viewer-search-scope').textContent.includes('불러온 범위'),
    'search preserves copy selection and visibly limits its scope');
  find('<b>'); usabilityCheck(count.textContent === '1 / 1' && !layer.querySelector('b'), 'HTML-like search remains literal text');
  find('not present'); usabilityCheck(count.textContent.includes('없습니다'), 'no-match search is honest');
  search.dispatchEvent(new KeyboardEvent('keydown', { bubbles: true, cancelable: true, key: 'Escape', isComposing: true, keyCode: 229 }));
  usabilityCheck(search.value === 'not present', 'search IME Escape does not discard the composing query');
  usabilityById('viewer-search-clear').click(); usabilityCheck(search.value === '' && count.textContent === '', 'clear resets query and count');
  clipboardOutcome = 'fail'; copy.click(); await sleep(20);
  usabilityCheck(usabilityById('viewer-copy-status').textContent.includes('권한'), 'clipboard rejection offers native-copy recovery');
  window.getSelection().removeAllRanges(); copy.click(); await sleep(20);
  usabilityCheck(copied.length === 2, 'empty selection never writes to clipboard');
  select(); usabilityById('viewer-privacy-curtain').hidden = false; copy.click(); find('alpha');
  usabilityCheck(copied.length === 2 && count.textContent === '', 'privacy protects explicit copy and search');
  usabilityById('viewer-privacy-curtain').hidden = true;
  wrapToggle.click(); await usabilitySettle();
  usabilityCheck(wrapToggle.getAttribute('aria-pressed') === 'true' && getComputedStyle(usabilityById('viewer-canvas')).display === 'none', 'readable wrap is explicit');
  usabilityById('viewer-overview').click(); await usabilitySettle();
  usabilityCheck(wrapToggle.getAttribute('aria-pressed') === 'false' && usabilityById('viewer-overview').getAttribute('aria-pressed') === 'true', 'overview and readable wrap are exclusive');
  usabilityById('viewer-overview').click();
  mobileHistoryAutoReply = false;
  usabilityById('viewer-bottom').click();
  let sent = take();
  usabilityCheck(sent.length === 1 && sent[0].reset === true && sent[0].delta === 0 && sent[0].request > 0,
    'retained offset-zero current action sends an absolute correlated reset');
  const reset = sent[0];
  usabilityById('viewer-bottom').click(); usabilityById('viewer-bottom').click();
  usabilityCheck(take().length === 0, 'repeated current action deduplicates its pending reset');
  frame(reset.request); await usabilitySettle();
  usabilityCheck(!usabilityById('viewer-bottom').disabled, 'matching current reply completes reset and clears pending UI');
  pan(5); await sleep(); sent = take();
  usabilityCheck(sent.length === 1 && sent[0].delta === 5 && sent[0].anchor?.first_line === '10000' && sent[0].anchor.generation === '1', 'relative history captures displayed anchor');
  const firstPan = sent[0]; pan(3); await sleep(); sent = take();
  usabilityCheck(sent.length === 1 && sent[0].delta === 8 && sent[0].request > firstPan.request && sent[0].anchor.first_line === '10000', 'in-flight history wave accumulates movement from one displayed base');
  const lastPan = sent[0]; frame(firstPan.request, '9995', 'OLD'); frame(undefined, '10000', 'PASSIVE'); await usabilitySettle();
  usabilityCheck(layer.textContent.includes('한글🙂') && !layer.textContent.includes('OLD'), 'stale reply and passive live source cannot rebase the reader');
  frame(lastPan.request, '9992', 'MATCHED'); await usabilitySettle();
  usabilityCheck(layer.textContent.includes('MATCHED'), 'latest matching reply publishes the requested source');
  pan(2); await sleep(); const queuedFirst = take()[0]; pan(3);
  frame(queuedFirst.request, '9990', 'ACK BEFORE FLUSH'); await sleep(); sent = take();
  usabilityCheck(sent.length === 1 && sent[0].delta === 3 && sent[0].anchor.first_line === '9990', 'matching ACK before coalesced flush preserves unsent increment from newly displayed base');
  frame(sent[0].request, '9987', 'CONTINUED'); await usabilitySettle();
  usabilityById('viewer-bottom').click(); const canceledReset = take()[0];
  find('CONTINUED');
  usabilityCheck(!usabilityById('viewer-bottom').disabled, 'new search cancels pending reset and restores current action');
  pan(4); await sleep(); sent = take();
  usabilityCheck(sent.length === 1 && sent[0].anchor.first_line === '9987' && sent[0].delta === 4, 'relative pan after canceled server reset starts from retained displayed source');
  frame(canceledReset.request); frame(sent[0].request, '9983', 'AFTER CANCEL'); await usabilitySettle();
  usabilityCheck(layer.textContent.includes('AFTER CANCEL') && search.value === '', 'matching source replacement clears old search without canceled reset jump');
  usabilityById('viewer-bottom').click(); const failed = take()[0];
  mobileUsabilitySocket.message({ type: 'history_error', session: 's-1', request: failed.request - 1, reason: 'unavailable' });
  usabilityCheck(usabilityById('viewer-bottom').disabled, 'unrelated query error cannot cancel current reset');
  mobileUsabilitySocket.message({ type: 'history_error', session: 's-1', request: failed.request, reason: 'unavailable' });
  usabilityCheck(!usabilityById('viewer-bottom').disabled && note.textContent.includes('사용할 수'), 'matching query failure retains display and exposes recovery');
  frame(failed.request); await usabilitySettle(); usabilityCheck(layer.textContent.includes('AFTER CANCEL'), 'failed query late answer cannot install');
  usabilityById('viewer-bottom').click(); const timeout = take()[0]; await sleep(4900);
  usabilityCheck(usabilityById('viewer-bottom').disabled, 'an older canceled deadline cannot cancel the newer reset');
  await sleep(250);
  usabilityCheck(!usabilityById('viewer-bottom').disabled && note.textContent.includes('확인하지'), 'connected-socket query has bounded timeout and reachable retry');
  frame(timeout.request); await usabilitySettle(); usabilityCheck(layer.textContent.includes('AFTER CANCEL'), 'timed-out late answer stays retained');
  mobileUsabilitySocket.message({ type: 'input_pressure', session: 's-1', reason: 'queue_full', queued: 1 });
  pan(2); await sleep(); sent = take();
  usabilityCheck(sent.length === 1 && sent[0].anchor.first_line === '9983', 'input queue pressure never blocks independent reading navigation');
  frame(sent[0].request, '9983', 'EXPIRED FALLBACK', true); await usabilitySettle();
  usabilityCheck(note.textContent.includes('만료'), 'matching expiry is honestly reported: ' + note.textContent + ' requests=' + JSON.stringify(scrolls().slice(-3)));
  usabilityCheck(layer.textContent.includes('AFTER CANCEL') && !layer.textContent.includes('EXPIRED FALLBACK'), 'expired response preserves loaded text');
  mobileUsabilitySocket.message({ type: 'input_pressure', session: 's-1', reason: 'queue_full', queued: 0 });
  usabilityById('viewer-bottom').click(); let inputReset = take()[0]; frame(inputReset.request); await usabilitySettle();
  const editDraft = (text) => {
    const input = usabilityById('composer-text'); input.value = text;
    input.dispatchEvent(new Event('input', { bubbles: true }));
  };
  select(); find('alpha');
  const composingInput = usabilityById('composer-text');
  composingInput.dispatchEvent(new CompositionEvent('compositionstart', { bubbles: true })); editDraft('ㅎ');
  composingInput.dispatchEvent(new KeyboardEvent('keydown', { bubbles: true, cancelable: true, key: 'Enter', isComposing: true }));
  usabilityCheck(take().length === 0 && search.value === 'alpha', 'buffered unconfirmed IME preserves reading without reset');
  editDraft('한'); composingInput.dispatchEvent(new CompositionEvent('compositionend', { bubbles: true, data: '한' })); await sleep(20);
  const compositionScrolls = take();
  usabilityCheck(compositionScrolls.every(f => !f.reset && f.delta === 0 && f.anchor?.first_line === '10000') && search.value === 'alpha', 'confirmed text stays in the unified draft: ' + JSON.stringify(compositionScrolls) + ' search=' + search.value);
  composingInput.dispatchEvent(new KeyboardEvent('keydown', { bubbles: true, cancelable: true, key: 'Enter' }));
  sent = take(); inputReset = sent[0];
  usabilityCheck(sent.length === 1 && inputReset.reset && search.value === '', 'explicit buffered Enter resumes current exactly once');
  frame(inputReset.request); await usabilitySettle();
  select(); find('alpha'); const loadedBeforeFailure = layer.textContent; mobileInputSendFailure = true;
  editDraft('초안'); usabilityById('composer-send').click();
  const composer = usabilityById('composer-text');
  usabilityCheck(composer.value === '초안' && take().length === 0 && search.value === 'alpha' && layer.textContent === loadedBeforeFailure,
    'failed buffered transport preserves draft and reading');
  mobileInputSendFailure = false;
  const originalFetch = window.fetch;
  try {
    window.fetch = async () => ({ ok: true, json: async () => ({ path: '/tmp/mobile-attachment.txt' }) });
    usabilityById('composer-attach').click();
    const files = new DataTransfer(); files.items.add(new File(['attachment'], 'attachment.txt', { type: 'text/plain' }));
    usabilityById('composer-file').files = files.files;
    usabilityById('composer-file').dispatchEvent(new Event('change', { bubbles: true })); await sleep(20);
  } finally { window.fetch = originalFetch; }
  usabilityCheck(composer.value.includes('/tmp/mobile-attachment.txt') && search.value === 'alpha' && take().length === 0,
    'upload path insertion edits the independent draft without resuming reading');
  usabilityById('composer-send').click(); sent = take();
  usabilityCheck(sent.length === 1 && sent[0].reset && search.value === '', 'successful committed composer resumes current once');
  frame(sent[0].request); await usabilitySettle();
  wrapToggle.click(); await usabilitySettle(); wrapToggle.click();
  usabilityCheck(!usabilityById('viewer-scroll-note').hidden && !usabilityById('viewer-bottom').disabled, 'leaving readable mode stays pinned until explicit current recovery');
  mobileHistoryAutoReply = true;
  usabilityById('viewer-bottom').click(); await usabilitySettle(); take();
  clipboardOutcome = 'pending'; select(); copy.click(); const obsoleteCopy = finishClipboard;
  window.getSelection().removeAllRanges(); copy.click(); obsoleteCopy(); await sleep(20);
  usabilityCheck(usabilityById('viewer-copy-status').textContent.includes('선택하세요'), 'new copy intent invalidates an older clipboard completion');
  clipboardOutcome = 'pending'; frame(); await usabilitySettle(); select(); copy.click();
  usabilitySwitchSession(); usabilitySwitchSession(); await usabilitySettle(); finishClipboard(); await sleep(20);
  usabilityCheck(!usabilityById('viewer-copy-status').textContent.includes('복사했습니다'), 'old clipboard promise cannot label a new watch as copied');
  clipboardOutcome = 'ok';
  usabilityById('viewer-bottom').click(); await usabilitySettle(); take();
  mobileHistoryAutoReply = false;
  pan(1000000); await sleep(); const capped = take()[0];
  usabilityCheck(capped.delta === 100000 && capped.anchor.first_line === '10000', 'relative cumulative delta uses the safe agreed cap');
  const beforeQueuedSwitch = mobileUsabilitySent.length;
  pan(3); usabilitySwitchSession(); usabilitySwitchSession(); await sleep();
  usabilityCheck(!mobileUsabilitySent.slice(beforeQueuedSwitch).some((value) => value.type === 'scroll'), 'watch transition clears queued history gestures');
  frame(capped.request, '9990', 'OLD WATCH REPLY'); await usabilitySettle();
  usabilityCheck(!layer.textContent.includes('OLD WATCH REPLY'), 'same-UUID old-watch history reply cannot install');
  mobileHistoryAutoReply = true;
  usabilityById('viewer-bottom').click(); await usabilitySettle(); take();
}

async function runMobileResizeChecks() {
  const button = usabilityById('viewer-resize-control');
  const status = usabilityById('viewer-resize-status');
  usabilityCheck(button && status, 'explicit mobile terminal size control and status must exist');
  const all = () => mobileUsabilitySent.filter((frame) => frame.type === 'resize_control' || frame.type === 'resize');
  let checkpoint = all().length;
  const take = () => { const result = all().slice(checkpoint); checkpoint = all().length; return result; };
  const sleep = (ms = 320) => new Promise((resolve) => setTimeout(resolve, ms));
  const viewport = (width, height) => {
    Object.assign(mobileUsabilityViewport, { width, height, offsetTop: 0, offsetLeft: 0 });
    mobileUsabilityViewport.dispatchEvent(new Event('resize'));
  };
  const response = (request, owned, reason = owned ? 'owned' : 'released', session = 's-1', socket = mobileUsabilitySocket) =>
    socket.message({ type: 'terminal_control', session, request, owned, reason });
  const action = (name, session = 's-1') => {
    button.click();
    const sent = take();
    usabilityCheck(sent.length === 1 && sent[0].type === 'resize_control' && sent[0].session === session
      && sent[0].action === name && Number.isInteger(sent[0].request) && sent[0].request > 0,
      `explicit ${name} sends one correlated control for ${session}`);
    return sent[0].request;
  };
  const expectedGrid = () => {
    const stage = usabilityById('viewer-stage');
    const font = Number(usabilityById('viewer-font-size').textContent.replace('px', ''));
    const canvas = usabilityById('viewer-canvas');
    const measure = document.createElement('canvas').getContext('2d');
    measure.font = canvas.getContext('2d').font;
    const width = measure.measureText('M'.repeat(32)).width / 32;
    const cols = Math.max(32, Math.min(400, Math.floor(stage.clientWidth / width)));
    return { cols, rows: Math.max(2, Math.min(200, Math.floor(65536 / cols), Math.floor(stage.clientHeight / Math.ceil(font * 1.4)))) };
  };
  const expectResize = async (request, message, session = 's-1') => {
    // A bounded display-history ACK may finish after the200ms native geometry settle.
    await usabilityWait(() => all().slice(checkpoint).some(frame => frame.type === 'resize'));
    const sent = take();
    const size = expectedGrid();
    usabilityCheck(sent.length === 1 && sent[0].type === 'resize' && sent[0].session === session
      && sent[0].request === request && sent[0].cols === size.cols && sent[0].rows === size.rows,
      `${message}: one configured-font stage resize; sent=${JSON.stringify(sent)} expected=${JSON.stringify(size)}`);
    return sent[0];
  };

  const scrollbars = document.createElement('style');
  scrollbars.textContent = '#viewer .viewer-wrap::-webkit-scrollbar { width:24px; height:24px; }';
  document.head.append(scrollbars);
  mobileUsabilityViewport.dispatchEvent(new Event('resize'));
  await usabilitySettle();
  usabilityCheck(document.querySelector('.viewer-wrap').offsetWidth > document.querySelector('.viewer-wrap').clientWidth,
    'geometry echo regression exercises real layout scrollbars');
  usabilityCheck(all().length === 0 && button.getAttribute('aria-pressed') === 'false', 'watch never auto-acquires terminal size');
  if (globalThis.mobileResizeSequenceLimit) {
    const firstRequest = action('acquire'); response(firstRequest, true); await sleep(); await expectResize(firstRequest, 'bounded sequence first lease');
    const releaseRequest = action('release'); response(releaseRequest, false);
    const lastRequest = action('acquire'); response(lastRequest, true); await sleep(); await expectResize(lastRequest, 'bounded sequence final lease');
    usabilityCheck(lastRequest === mobileResizeSequenceLimit, 'bounded sequence reaches the maximum without wrap');
    const exhaustedSocket = mobileUsabilitySocket;
    button.click();
    await usabilityWait(() => mobileUsabilitySocket !== exhaustedSocket && usabilityById('viewer-connection-label').textContent === '연결됨');
    await sleep();
    usabilityCheck(exhaustedSocket.readyState === 3 && take().length === 0,
      'sequence exhaustion closes the owned socket without zero request or automatic reacquisition');
    const freshRequest = action('acquire');
    usabilityCheck(freshRequest === 1, 'only a fresh explicit action on a new socket starts a new sequence');
    response(freshRequest, true); await sleep(); await expectResize(freshRequest, 'fresh socket after sequence exhaustion');
    const finalRelease = action('release'); response(finalRelease, false);
    scrollbars.remove();
    return;
  }
  let acquire = action('acquire');
  usabilityCheck(button.getAttribute('aria-pressed') === 'false' && status.textContent.includes('요청'), 'pending request never claims ownership');
  viewport(430, 932); await sleep();
  usabilityCheck(take().length === 0, 'layout cannot resize before an authoritative grant');
  response(acquire, true, 'owned', 's-2'); response(acquire + 1, true); await sleep();
  usabilityCheck(take().length === 0 && button.getAttribute('aria-pressed') === 'false', 'wrong UUID and request cannot grant local resize permission');
  response(acquire, false, 'in_use');
  usabilityCheck(status.textContent.includes('다른') && button.getAttribute('aria-pressed') === 'false', 'denial is honestly view-only and explicitly retryable');
  const denied = acquire;
  acquire = action('acquire');
  usabilityCheck(acquire > denied, 'request sequence increases after an explicit retry');
  response(denied, true); await sleep();
  usabilityCheck(take().length === 0, 'late rejected acquire grant cannot satisfy a newer request');
  response(acquire, true); await sleep();
  const first = await expectResize(acquire, 'authoritative grant');
  usabilityCheck(button.getAttribute('aria-pressed') === 'true', 'only a matching grant displays owned state');
  for (const reason of ['owned', 'owned', 'invalid_size', 'resize_failed', 'owned']) response(acquire, true, reason);
  mobileUsabilitySocket.message({ type: 'viewport', session: 's-1', keyframe: true,
    cols: first.cols, rows: first.rows, lines: [], cursor: null, offset: 0 });
  await sleep();
  usabilityCheck(take().length === 0, 'renewal/error ACKs and actual geometry echoes cannot create resize feedback');

  mobileUsabilitySocket.message({ type: 'viewport', session: 's-1', keyframe: true,
    cols: 180, rows: 40, lines: [], cursor: { visible: true, row: 2, col: 6 }, offset: 0 });
  await usabilitySettle();
  const readingWrap = document.querySelector('.viewer-wrap');
  readingWrap.scrollLeft = 120; readingWrap.scrollTop = 80;
  readingWrap.dispatchEvent(new Event('scroll'));
  viewport(844, 390); usabilityById('viewer-font-larger').click(); await sleep();
  usabilityCheck(take().length === 0 && status.textContent.includes('일시정지') && !button.disabled,
    'local reading suspends rotation/font resize and retains explicit release');
  usabilityById('viewer-bottom').click(); await sleep(); await expectResize(acquire, 'returning from local reading');
  // Connection-local history is a correlated query; native desktop scroll no longer changes it.
  mobileHistoryAutoReply = false;
  readingWrap.scrollTop = 0;
  readingWrap.dispatchEvent(new WheelEvent('wheel', { bubbles: true, cancelable: true, deltaY: -20, deltaMode: 1 }));
  await sleep(110);
  const historicalQuery = mobileUsabilitySent.filter(frame => frame.type === 'scroll').at(-1);
  mobileUsabilitySocket.message({ type: 'viewport', session: 's-1', keyframe: true,
    cols: 180, rows: 40, lines: [], cursor: null, offset: 20,
    history: { generation: '1', first_line: '9980', total_lines: 5000, request: historicalQuery.request, expired: false } });
  viewport(430, 600); await sleep();
  usabilityCheck(take().length === 0 && status.textContent.includes('일시정지'), 'historical viewport remains stable during layout changes');
  usabilityById('viewer-bottom').click(); await sleep(80);
  usabilityCheck(take().length === 0, 'return-to-current cannot resize until its historical viewport reaches offset zero');
  const currentQuery = mobileUsabilitySent.filter(frame => frame.type === 'scroll').at(-1);
  mobileUsabilitySocket.message({ type: 'viewport', session: 's-1', keyframe: true,
    cols: 180, rows: 40, lines: [], cursor: { visible: true, row: 2, col: 6 }, offset: 0,
    history: { generation: '1', first_line: '10000', total_lines: 5000, request: currentQuery.request, expired: false } });
  mobileHistoryAutoReply = true;
  await sleep(); await expectResize(acquire, 'current viewport resumes resize after historical reading');
  viewport(500, 650); await sleep(70);
  readingWrap.scrollLeft = 200; readingWrap.dispatchEvent(new Event('scroll')); await sleep();
  usabilityCheck(take().length === 0, 'starting local reading cancels an already pending resize');
  usabilityById('viewer-bottom').click(); await sleep(); await expectResize(acquire, 'local reading transition explicitly resumes pending geometry');

  const beforeOverviewRelease = action('release'); response(beforeOverviewRelease, false);
  mobileUsabilitySocket.message({ type: 'viewport', session: 's-1', keyframe: true,
    cols: 32, rows: 2, lines: [], cursor: null, offset: 0 });
  await usabilitySettle();
  const fittingFont = usabilityFont();
  usabilityById('viewer-overview').click(); await usabilitySettle();
  usabilityCheck(Math.abs(usabilityFont() - fittingFont) < 0.01, 'overview regression has scale one and unchanged stage metrics');
  acquire = action('acquire'); response(acquire, true); await sleep();
  usabilityCheck(take().length === 0, 'grant during fitting overview still suspends resize');
  usabilityById('viewer-overview').click(); await sleep();
  await expectResize(acquire, 'leaving fitting overview explicitly schedules the first granted size');
  viewport(550, 700); await sleep(70); usabilityById('viewer-overview').click(); await sleep();
  usabilityCheck(take().length === 0, 'entering fitting overview cancels prior pending geometry');
  usabilityById('viewer-overview').click(); await sleep(); await expectResize(acquire, 'leaving fitting overview reevaluates pending geometry');
  viewport(650, 720);
  for (let index = 0; index < 3; index++) {
    await sleep(80);
    mobileUsabilitySocket.message({ type: 'viewport', session: 's-1', keyframe: false,
      cols: 32, rows: 2, lines: [], cursor: null, offset: 0 });
  }
  await sleep(100);
  await expectResize(acquire, 'passive frames do not restart the geometry settle timer');
  usabilityById('viewer-readable-wrap').click(); viewport(855, 510); usabilityById('viewer-font-larger').click(); await sleep();
  usabilityCheck(take().length === 0 && status.textContent.includes('일시정지') && !button.disabled,
    'owned lease remains releasable while readable mode suspends geometry');
  usabilityById('viewer-readable-wrap').click(); await sleep();
  usabilityCheck(take().length === 0, 'leaving readable wrap does not implicitly leave retained reading');
  usabilityById('viewer-bottom').click(); await sleep(); await expectResize(acquire, 'explicit matching current reply resumes resize after reading');

  viewport(390, 600); await sleep(70); viewport(390, 470); await sleep(70); viewport(390, 360); await sleep(100);
  usabilityCheck(take().length === 0, 'keyboard animation waits for 200ms settled geometry');
  await sleep(170); await expectResize(acquire, 'settled keyboard geometry');
  viewport(844, 390); await sleep(70); viewport(932, 430); await sleep(); await expectResize(acquire, 'settled rotation');
  usabilityById('viewer-font-larger').click(); await sleep(); await expectResize(acquire, 'font size change');
  usabilityById('viewer-overview').click(); viewport(390, 600); await sleep();
  usabilityCheck(take().length === 0 && status.textContent.includes('일시정지') && !button.disabled,
    'overview suspends auto-resize without losing the release control');
  usabilityById('viewer-font-larger').click(); await sleep();
  usabilityCheck(take().length === 0, 'shrunk overview font metrics never resize the PTY');
  usabilityById('viewer-overview').click(); await sleep(); await expectResize(acquire, 'leaving overview uses configured font');
  viewport(10000, 10000); await sleep();
  const bounded = await expectResize(acquire, 'huge viewport');
  usabilityCheck(bounded.cols === 400 && bounded.rows === 163 && bounded.cols * bounded.rows <= 65536, 'mobile geometry honors dimensions and cell budget');
  viewport(320, 80); await sleep();
  usabilityCheck(usabilityById('viewer-stage').clientHeight === 0 && take().length === 0
    && button.getAttribute('aria-pressed') === 'true' && !button.disabled,
    'zero physical stage suspends resize while ownership and release remain available');
  viewport(320, 120); await sleep();
  usabilityCheck(usabilityById('viewer-stage').clientHeight > 0 && document.querySelector('.viewer-wrap').clientHeight === 0,
    'keyboard recovery changes physical geometry while scrollbar-constrained inner geometry stays zero');
  const tiny = await expectResize(acquire, 'tiny keyboard viewport recovery');
  usabilityCheck(tiny.cols === 32 && tiny.rows === 2, 'positive tiny stage uses the protocol minimum geometry');

  mobileUsabilitySocket.message({ type: 'input_pressure', session: 's-1', reason: 'queue_full', queued: 1 });
  viewport(430, 844);
  const release = action('release');
  usabilityCheck(release > acquire && button.getAttribute('aria-pressed') === 'false', 'release stops local ownership immediately during queue pressure');
  response(acquire, true); await sleep();
  usabilityCheck(take().length === 0, 'release clears a pending resize timer and ignores late grant');
  response(release, false, 'restore_failed');
  usabilityCheck(status.textContent.includes('복원') && button.getAttribute('aria-pressed') === 'false', 'restore failure does not claim retained ownership');
  mobileUsabilitySocket.message({ type: 'input_pressure', session: 's-1', reason: 'queue_full', queued: 0 });
  const canceledAcquire = action('acquire');
  const cancel = action('release');
  response(canceledAcquire, true); response(cancel, false); await sleep();
  usabilityCheck(take().length === 0 && cancel > canceledAcquire, 'pending acquire can be canceled without a late resize');
  const timedOut = action('acquire'); response(timedOut, false, 'timeout'); response(timedOut, true); await sleep();
  usabilityCheck(take().length === 0 && status.textContent.includes('확인하지'), 'final timeout cannot be revived by a late grant with the same request');
  const expiring = action('acquire'); response(expiring, true); await sleep(); await expectResize(expiring, 'lease before expiry');
  viewport(844, 390); response(expiring, false, 'expired'); response(expiring, true); await sleep();
  usabilityCheck(take().length === 0 && status.textContent.includes('끝났') && button.getAttribute('aria-pressed') === 'false',
    'lease expiry clears pending geometry and cannot be revived by a same-request grant');

  acquire = action('acquire'); response(acquire, true); await sleep(); await expectResize(acquire, 'fresh lease');
  viewport(390, 500);
  const sentBeforeSwitch = mobileUsabilitySent.length;
  usabilitySwitchSession();
  const transition = mobileUsabilitySent.slice(sentBeforeSwitch);
  const oldRelease = take();
  usabilityCheck(oldRelease.length === 1 && oldRelease[0].session === 's-1' && oldRelease[0].action === 'release'
    && oldRelease[0].request > acquire && transition.findIndex((f) => f.type === 'resize_control') < transition.findIndex((f) => f.type === 'watch'),
    'session switch releases the captured old UUID before watching a new one');
  response(acquire, true); await sleep(); usabilityCheck(take().length === 0, 'old session grant and timer cannot affect the new session');
  usabilitySwitchSession(); await sleep();
  usabilityCheck(take().length === 0, 'switching back never auto-acquires');
  const afterWatch = action('acquire');
  usabilityCheck(afterWatch > oldRelease[0].request, 'request sequence survives A to B to A on one socket');
  response(acquire, true); await sleep(); usabilityCheck(take().length === 0, 'same UUID stale grant cannot satisfy a later watch');
  response(afterWatch, true); await sleep(); await expectResize(afterWatch, 'new watch matching grant');
  window.dispatchEvent(new Event('pagehide'));
  const pageRelease = take();
  usabilityCheck(pageRelease.length === 1 && pageRelease[0].action === 'release' && pageRelease[0].session === 's-1', 'pagehide releases captured ownership');
  response(afterWatch, true); await sleep(); usabilityCheck(take().length === 0, 'pagehide retirement blocks late grant');

  acquire = action('acquire');
  const oldSocket = mobileUsabilitySocket;
  oldSocket.close();
  await usabilityWait(() => mobileUsabilitySocket !== oldSocket && usabilityById('viewer-connection-label').textContent === '연결됨');
  await sleep();
  usabilityCheck(take().length === 0 && button.getAttribute('aria-pressed') === 'false', 'reconnect re-watches but never acquires automatically');
  response(acquire, true, 'owned', 's-1', oldSocket); await sleep();
  usabilityCheck(take().length === 0, 'stale socket cannot grant control after reconnect');
  acquire = action('acquire'); response(acquire, true); await sleep(); await expectResize(acquire, 'fresh socket explicit grant');
  let hidden = true;
  Object.defineProperty(document, 'hidden', { configurable: true, get: () => hidden });
  const backgroundSocket = mobileUsabilitySocket;
  document.dispatchEvent(new Event('visibilitychange'));
  const backgroundRelease = take();
  usabilityCheck(backgroundRelease.length === 1 && backgroundRelease[0].action === 'release'
    && backgroundSocket.readyState === 3, 'background releases while captured socket is still open');
  hidden = false; document.dispatchEvent(new Event('visibilitychange'));
  await usabilityWait(() => mobileUsabilitySocket !== backgroundSocket && usabilityById('viewer-connection-label').textContent === '연결됨');
  delete document.hidden;
  await sleep(); usabilityCheck(take().length === 0, 'foreground return never reacquires');
  acquire = action('acquire'); response(acquire, true); await sleep(); await expectResize(acquire, 'lease before explicit close');
  usabilityById('viewer-back').click();
  await usabilityWait(() => usabilityById('viewer').hidden);
  const closeRelease = take();
  usabilityCheck(closeRelease.length === 1 && closeRelease[0].action === 'release' && closeRelease[0].session === 's-1', 'Back releases before viewer closes');
  document.querySelector('.session[data-session-id="s-1"]').click(); await sleep();
  usabilityCheck(take().length === 0, 'reopening is view-only');
  scrollbars.remove();
}

async function runUnifiedInputChecks() {
  const input = () => usabilityById('composer-text');
  const sleep = () => new Promise(resolve => setTimeout(resolve, 30));
  const frames = () => mobileUsabilitySent.filter(f => ['input', 'direct_input', 'direct_key'].includes(f.type));
  let checkpoint = frames().length;
  const take = () => { const result = frames().slice(checkpoint); checkpoint = frames().length; return result; };
  const edit = (text, composing = false) => {
    input().value = text;
    input().dispatchEvent(new InputEvent('input', { bubbles: true, inputType: composing ? 'insertCompositionText' : 'insertText', data: text, isComposing: composing }));
  };
  const key = (name, props = {}) => {
    const event = new KeyboardEvent('keydown', { bubbles: true, cancelable: true, key: name, ...props });
    input().dispatchEvent(event); return event;
  };
  usabilityCheck(document.querySelectorAll('#viewer textarea').length === 1 && !document.querySelector('[id^="viewer-mode-"]'), 'one unified input without modes');
  usabilityCheck(mobileUsabilitySent.some(f => f.type === 'auth' && f.v === 5), 'protocol v5 retained');
  edit('  한글🙂 '); key('Backspace'); key('ArrowLeft'); key('Tab');
  usabilityCheck(take().length === 0, 'buffered edits and navigation do not write to terminal');
  usabilityCheck(!key('Enter', { shiftKey: true }).defaultPrevented, 'Shift Enter remains editable newline');
  const lineBreak = new InputEvent('beforeinput', { bubbles: true, cancelable: true, inputType: 'insertLineBreak' });
  input().dispatchEvent(lineBreak); edit('첫줄\n둘째줄');
  usabilityCheck(!lineBreak.defaultPrevented && take().length === 0, 'soft keyboard newline does not submit');
  input().dispatchEvent(new CompositionEvent('compositionstart', { bubbles: true })); edit('ㅎ', true);
  key('Enter', { isComposing: true, keyCode: 229 }); usabilityById('composer-send').click();
  usabilityCheck(take().length === 0 && usabilityById('composer-send').disabled, 'unconfirmed Korean IME never submits');
  edit('한글'); input().dispatchEvent(new CompositionEvent('compositionend', { bubbles: true, data: '한글' }));
  key('Enter'); usabilityCheck(take().length === 0, 'composition settling Enter is not accidental submission');
  await sleep(); key('Enter'); const committed = take();
  usabilityCheck(committed.length === 1 && committed[0].type === 'input' && committed[0].text === '한글' && committed[0].submit,
    'explicit settled Enter submits the complete draft exactly once');
  edit('/tmp/' + '한글'.normalize('NFD')); key('Enter');
  usabilityCheck(take()[0]?.text === '/tmp/한글', 'complete buffered path submits as NFC');
  edit('보관할 초안'); usabilityById('viewer-quick-button').click();
  for (const [name, wire, ctrl] of [['esc', 'esc', false], ['tab', 'tab', false], ['ctrl_c', 'c', true], ['ctrl_d', 'd', true], ['enter', 'enter', false]]) {
    document.querySelector(`[data-key="${name}"]`).click(); const sent = take();
    usabilityCheck(sent.length === 1 && sent[0].type === 'direct_key' && sent[0].key === wire && sent[0].ctrl === ctrl
      && input().value === '보관할 초안', `accessory ${name} sends only its key and retains draft`);
  }
  usabilityById('direct-ctrl').click(); key('c'); const modified = take();
  usabilityCheck(modified.length === 1 && modified[0].key === 'c' && modified[0].ctrl
    && usabilityById('direct-ctrl').getAttribute('aria-pressed') === 'false', 'modifier applies once without sending draft');
  const repeat = document.querySelector('[data-key="up"]');
  repeat.dispatchEvent(new PointerEvent('pointerdown', { bubbles: true, cancelable: true }));
  await new Promise(resolve => setTimeout(resolve, 580));
  repeat.dispatchEvent(new PointerEvent('pointerup', { bubbles: true })); repeat.click();
  const repeated = take(); usabilityCheck(repeated.length > 0 && repeated.every(f => f.key === 'up' && f.session === 's-1'), 'held arrows target captured session');
  const retired = input(); retired.dispatchEvent(new CompositionEvent('compositionstart', { bubbles: true })); edit('이전 세션', true);
  usabilitySwitchSession(); await sleep();
  retired.value = '이전 세션 늦은 입력'; retired.dispatchEvent(new Event('input', { bubbles: true }));
  retired.dispatchEvent(new KeyboardEvent('keydown', { bubbles: true, cancelable: true, key: 'Enter' }));
  usabilityCheck(take().length === 0 && input().value !== retired.value, 'late events on retired IME input cannot target new session');
  edit('B 전송'); key('Enter'); const onB = take();
  usabilityCheck(onB.length === 1 && onB[0].session === 's-2' && onB[0].text === 'B 전송', 'draft targets chosen stable session');
  usabilitySwitchSession(); await sleep();
  edit('실패 보존'); mobileInputSendFailure = true; key('Enter'); mobileInputSendFailure = false;
  usabilityCheck(take().length === 0 && input().value === '실패 보존', 'transport failure retains draft');
  const oldSocket = mobileUsabilitySocket, retiredSocketInput = input(); oldSocket.close(); await sleep();
  usabilityCheck(input().disabled && take().length === 0, 'disconnect locks submission without replay');
  await usabilityWait(() => mobileUsabilitySocket !== oldSocket && !input().disabled);
  retiredSocketInput.value = '늦은 전송'; retiredSocketInput.dispatchEvent(new Event('input', { bubbles: true }));
  retiredSocketInput.dispatchEvent(new KeyboardEvent('keydown', { bubbles: true, cancelable: true, key: 'Enter' }));
  usabilityCheck(take().length === 0, 'retired socket native input cannot replay');
  oldSocket.message({ type: 'input_pressure', session: 's-1', reason: 'closed', queued: 0 });
  usabilityCheck(!input().disabled, 'old socket cannot close current input');
  mobileUsabilitySocket.message({ type: 'input_pressure', session: 's-1', reason: 'queue_full', queued: 1 });
  key('Enter'); document.querySelector('[data-key="enter"]').click();
  usabilityCheck(input().disabled && take().length === 0, 'queue pressure blocks draft and accessory keys');
  mobileUsabilitySocket.message({ type: 'input_pressure', session: 's-1', reason: 'queue_full', queued: 0 });
  edit('복귀'); key('Enter'); usabilityCheck(take()[0]?.text === '복귀', 'pressure recovery accepts explicit new submission');
  usabilityById('viewer-quick-button').click();
}

async function runMobileUsability() {
  await usabilityWait(() => document.querySelector('.session[data-session-id="s-1"]'));
  document.querySelector('.session[data-session-id="s-1"]').click();
  await usabilityWait(() => usabilityById('viewer-canvas').width > 0);
  await usabilitySettle();
  if (sessionStorage.getItem('mobile-mode-restore') === 'yes') {
    usabilityCheck(!document.querySelector('.composer').hidden && document.querySelectorAll('#viewer textarea').length === 1 && !document.querySelector('[id^=viewer-mode-]'),
      'unified input remains the sole input after reload');
    usabilityCheck(mobileUsabilityErrors.length === 0, 'no JavaScript exceptions after mode restoration');
    document.body.append(`MOBILE_USABILITY_OK (${usabilityChecks} assertions including persisted reload)`);
    document.body.dataset.status = 'ok';
    return;
  }
  const smaller = usabilityById('viewer-font-smaller');
  const larger = usabilityById('viewer-font-larger');
  const fontLabel = usabilityById('viewer-font-size');
  const overview = usabilityById('viewer-overview');
  usabilityCheck(smaller && larger && fontLabel && overview, 'readability controls must exist');
  usabilityCheck(usabilityById('viewer-readable-wrap') && usabilityById('viewer-search-text')
    && usabilityById('viewer-copy-selection'), 'reading, loaded-window search and explicit selected-copy controls must exist');
  const restored = sessionStorage.getItem('mobile-usability-restore') === 'yes';
  usabilityCheck(fontLabel.textContent === (restored ? '17px' : '15px'), 'configured font default/restoration');
  usabilityCheck(overview.getAttribute('aria-pressed') === String(restored), 'explicit overview default/restoration');
  if (restored) overview.click();
  await usabilitySettle();

  if (!restored) {
    for (const [width, height] of [[320, 568], [390, 844], [430, 932], [844, 390]]) {
      Object.assign(mobileUsabilityViewport, { width, height, offsetTop: 0, offsetLeft: 0 });
      mobileUsabilityViewport.dispatchEvent(new Event('resize'));
      await usabilitySettle();
      usabilityCheck(Math.abs(usabilityFont() - 15) < 0.01, `readable 15px font at ${width}x${height}`);
      const wrap = document.querySelector('.viewer-wrap');
      usabilityCheck(wrap.scrollWidth > wrap.clientWidth, 'wide grid must remain horizontally accessible');
    }
    for (let i = 0; i < 20; i++) larger.click();
    await usabilitySettle();
    usabilityCheck(fontLabel.textContent === '24px' && larger.disabled, 'font maximum is 24px');
    for (let i = 0; i < 20; i++) smaller.click();
    await usabilitySettle();
    usabilityCheck(fontLabel.textContent === '12px' && smaller.disabled, 'font minimum is 12px');
    for (let i = 0; i < 5; i++) larger.click();
    overview.click();
    await usabilitySettle();
    usabilityCheck(usabilityFont() < 12, 'overview may shrink only after explicit selection');
    usabilityCheck(fontLabel.textContent === '17px', 'overview preserves configured font');
    sessionStorage.setItem('mobile-usability-restore', 'yes');
    sessionStorage.setItem('mobile-usability-checks', String(usabilityChecks));
    location.reload();
    return;
  }

  Object.assign(mobileUsabilityViewport, { width: 390, height: 360, offsetTop: 160, offsetLeft: 7 });
  mobileUsabilityViewport.dispatchEvent(new Event('resize'));
  mobileUsabilityViewport.dispatchEvent(new Event('scroll'));
  await usabilitySettle();
  usabilityById('viewer-menu-button').click();
  const menu = usabilityById('viewer-menu');
  const bounds = menu.getBoundingClientRect();
  const viewerBounds = usabilityById('viewer').getBoundingClientRect();
  usabilityCheck(bounds.top >= viewerBounds.top && bounds.bottom <= viewerBounds.bottom,
    'terminal menu must stay inside the keyboard visual viewport');
  usabilityCheck(bounds.left >= viewerBounds.left && bounds.right <= viewerBounds.right,
    'terminal menu must follow horizontal visual viewport offset');
  Object.assign(mobileUsabilityViewport, { height: 180, offsetTop: 220, offsetLeft: 12 });
  mobileUsabilityViewport.dispatchEvent(new Event('resize'));
  mobileUsabilityViewport.dispatchEvent(new Event('scroll'));
  await usabilitySettle();
  const movedBounds = menu.getBoundingClientRect();
  const movedViewerBounds = usabilityById('viewer').getBoundingClientRect();
  usabilityCheck(movedBounds.top >= movedViewerBounds.top && movedBounds.bottom <= movedViewerBounds.bottom,
    'already open menu follows the visual viewport and stays vertically contained');
  usabilityCheck(movedBounds.left >= movedViewerBounds.left && movedBounds.right <= movedViewerBounds.right,
    'already open menu follows the visual viewport horizontally');
  const menuContent = menu.querySelector('.m-sheet-content');
  usabilityCheck(menuContent.scrollHeight > menuContent.clientHeight && getComputedStyle(menuContent).overflowY === 'auto',
    'short keyboard viewport keeps all menu items reachable by scrolling');
  usabilityById('viewer-menu-button').click();
  const wrap = document.querySelector('.viewer-wrap');
  wrap.scrollLeft = 220;
  wrap.scrollTop = 150;
  wrap.dispatchEvent(new Event('scroll'));
  await usabilitySettle();
  usabilityCheck(!usabilityById('viewer-scroll-note').hidden,
    'local pan exposes return-to-current action even without server history offset');
  const pausedPosition = { left: wrap.scrollLeft, top: wrap.scrollTop };
  mobileUsabilitySocket.message({ type: 'viewport', session: 's-1', keyframe: false,
    cols: 180, rows: 40, lines: [], cursor: { visible: true, row: 35, col: 170 }, offset: 0,
    history: { generation: '1', first_line: '10000', total_lines: 5000, expired: false } });
  await usabilitySettle();
  usabilityCheck(wrap.scrollLeft === pausedPosition.left && wrap.scrollTop === pausedPosition.top,
    'live output does not steal local reading position');
  usabilityById('viewer-bottom').click();
  await usabilitySettle();
  usabilityCheck(wrap.scrollLeft > pausedPosition.left && wrap.scrollTop > pausedPosition.top,
    'return-to-current resumes following the live cursor');
  mobileUsabilitySocket.message({ type: 'viewport', session: 's-1', keyframe: false,
    cols: 180, rows: 40, lines: [], cursor: { visible: true, row: 1, col: 1 }, offset: 0,
    history: { generation: '1', first_line: '10000', total_lines: 5000, expired: false } });
  await usabilitySettle();
  usabilityCheck(wrap.scrollLeft < pausedPosition.left && wrap.scrollTop < pausedPosition.top,
    'later live frames continue following after return-to-current');
  await runMobileReadingChecks();
  await runMobileResizeChecks();
  await runUnifiedInputChecks();
  let composer = usabilityById('composer-text');
  composer.value = '세션 A 긴 지시 초안';
  composer.dispatchEvent(new Event('input', { bubbles: true }));
  larger.click();
  usabilitySwitchSession();
  composer = usabilityById('composer-text'); composer.value = '세션 B 초안';
  composer.dispatchEvent(new Event('input', { bubbles: true }));
  usabilitySwitchSession();
  composer = usabilityById('composer-text');
  usabilityCheck(composer.value === '세션 A 긴 지시 초안', 'font and session changes must preserve composer draft');
  composer.value = '기존 작성창 전송';
  usabilityById('composer-send').click();
  usabilityCheck(mobileUsabilitySent.some((frame) => frame.type === 'input'
    && frame.session === 's-1' && frame.text === '기존 작성창 전송' && frame.submit), 'composer still sends to watched session');
  mobileUsabilitySocket.message({ type: 'input_pressure', session: 's-1', reason: 'queue_full', queued: 1 });
  usabilityCheck(composer.disabled && usabilityById('composer-send').disabled, 'pressure keeps composer locked');
  const count = mobileUsabilitySent.length;
  usabilityById('composer-send').click();
  document.querySelector('[data-key="enter"]').click();
  smaller.click();
  usabilityCheck(mobileUsabilitySent.length === count, 'readability controls never bypass input lock or send protocol frames');
  usabilityCheck(!smaller.disabled && !larger.disabled, 'readability controls remain available while input is locked');
  mobileUsabilitySocket.close();
  usabilityCheck(composer.disabled && usabilityById('composer-send').disabled, 'disconnect keeps composer locked');
  usabilityCheck(mobileUsabilityErrors.length === 0, 'no JavaScript exceptions');
  sessionStorage.setItem('mobile-mode-restore', 'yes');
  sessionStorage.setItem('mobile-usability-checks', String(usabilityChecks));
  location.reload();
}
runMobileUsability().catch((error) => {
  document.body.append('MOBILE_USABILITY_ERROR:' + error.message);
  document.body.dataset.status = 'error';
});
