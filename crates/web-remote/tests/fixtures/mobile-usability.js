(() => {
  localStorage.setItem('deppy.webToken', 'usability-test-token');
  globalThis.mobileUsabilitySent = [];
  globalThis.mobileUsabilityErrors = [];
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
    message(frame) { this.emit('message', { data: JSON.stringify(frame) }); }
    send(raw) {
      const frame = JSON.parse(raw);
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

async function runMobileUsability() {
  await usabilityWait(() => document.querySelector('.ws-open'));
  document.querySelector('.ws-open').click();
  await usabilityWait(() => usabilityById('viewer-canvas').width > 0);
  await usabilitySettle();
  const smaller = usabilityById('viewer-font-smaller');
  const larger = usabilityById('viewer-font-larger');
  const fontLabel = usabilityById('viewer-font-size');
  const overview = usabilityById('viewer-overview');
  usabilityCheck(smaller && larger && fontLabel && overview, 'readability controls must exist');
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
  usabilityCheck(menu.scrollHeight > menu.clientHeight && getComputedStyle(menu).overflowY === 'auto',
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
    cols: 180, rows: 40, lines: [], cursor: { visible: true, row: 35, col: 170 }, offset: 0 });
  await usabilitySettle();
  usabilityCheck(wrap.scrollLeft === pausedPosition.left && wrap.scrollTop === pausedPosition.top,
    'live output does not steal local reading position');
  usabilityById('viewer-bottom').click();
  await usabilitySettle();
  usabilityCheck(wrap.scrollLeft > pausedPosition.left && wrap.scrollTop > pausedPosition.top,
    'return-to-current resumes following the live cursor');
  mobileUsabilitySocket.message({ type: 'viewport', session: 's-1', keyframe: false,
    cols: 180, rows: 40, lines: [], cursor: { visible: true, row: 1, col: 1 }, offset: 0 });
  await usabilitySettle();
  usabilityCheck(wrap.scrollLeft < pausedPosition.left && wrap.scrollTop < pausedPosition.top,
    'later live frames continue following after return-to-current');
  const composer = usabilityById('composer-text');
  composer.value = '세션 A 긴 지시 초안';
  composer.dispatchEvent(new Event('input', { bubbles: true }));
  larger.click();
  usabilityById('viewer-session-chip').click();
  composer.value = '세션 B 초안';
  usabilityById('viewer-session-chip').click();
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
  document.body.append(`MOBILE_USABILITY_OK (${usabilityChecks} assertions including persisted reload)`);
  document.body.dataset.status = 'ok';
}
runMobileUsability().catch((error) => {
  document.body.append('MOBILE_USABILITY_ERROR:' + error.message);
  document.body.dataset.status = 'error';
});
