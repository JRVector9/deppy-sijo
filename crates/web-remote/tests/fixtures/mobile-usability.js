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

async function runDirectInputChecks() {
  const direct = () => usabilityById('direct-text');
  const directMode = usabilityById('viewer-mode-direct');
  const composerMode = usabilityById('viewer-mode-composer');
  const composer = usabilityById('composer-text');
  usabilityCheck(direct() && directMode && composerMode, 'explicit direct and long-instruction modes must exist');
  usabilityCheck(!usabilityById('direct-controls').hidden && document.querySelector('.composer').hidden,
    'new-open session defaults to direct terminal mode');
  usabilityCheck(mobileUsabilitySent.some((frame) => frame.type === 'auth' && frame.v === 5), 'direct input uses protocol v5');
  const inputContext = direct().value;
  usabilityCheck(inputContext.length > 0 && direct().selectionStart === inputContext.length,
    'idle input retains editable context for a soft backspace');
  const frames = () => mobileUsabilitySent.filter((frame) => frame.type === 'direct_input' || frame.type === 'direct_key');
  let checkpoint = frames().length;
  const take = () => { const result = frames().slice(checkpoint); checkpoint = frames().length; return result; };
  const tick = () => new Promise((resolve) => setTimeout(resolve, 20));
  const edit = (value, type = 'insertText', data = value, composing = false, force = false) => {
    const before = new InputEvent('beforeinput', { bubbles: true, cancelable: !composing,
      inputType: type, data, isComposing: composing });
    direct().dispatchEvent(before);
    if (!before.defaultPrevented || force) {
      direct().value = inputContext + value;
      direct().setSelectionRange(direct().value.length, direct().value.length);
      direct().dispatchEvent(new InputEvent('input', { bubbles: true, inputType: type, data, isComposing: composing }));
    }
    return before;
  };
  const key = (name, modifiers = {}) => {
    const event = new KeyboardEvent('keydown', { bubbles: true, cancelable: true, key: name, ...modifiers });
    direct().dispatchEvent(event);
    return event;
  };
  const start = () => direct().dispatchEvent(new CompositionEvent('compositionstart', { bubbles: true }));
  const end = (data) => direct().dispatchEvent(new CompositionEvent('compositionend', { bubbles: true, data }));
  const expectText = (text, paste = false, message = text) => {
    const sent = take();
    usabilityCheck(sent.length === 1 && sent[0].type === 'direct_input' && sent[0].session === 's-1'
      && sent[0].text === text && sent[0].paste === paste, `${message}: confirmed text exactly once`);
  };

  direct().focus();
  document.execCommand('delete');
  usabilityCheck(take().length === 1 && direct().value === inputContext,
    'actual browser deletion of idle context sends one backspace and restores editable context');
  await tick();
  document.execCommand('insertText', false, 'z'); expectText('z', false, 'actual browser text edit');
  key('a'); edit('a'); expectText('a', false, 'printable hardware keydown/input');
  edit('  한글🙂 '); expectText('  한글🙂 ', false, 'Unicode and spaces');
  start(); edit('ㅎ', 'insertCompositionText', 'ㅎ', true);
  edit('한', 'insertCompositionText', '한', true);
  usabilityCheck(take().length === 0, 'unconfirmed Korean candidates stay local');
  end('한'); await tick(); expectText('한', false, 'input before compositionend');
  start(); edit('하', 'insertCompositionText', '하', true);
  end('하'); edit('한', 'insertCompositionText', '한');
  await tick(); expectText('한', false, 'compositionend before final input uses settled DOM');
  key('Unidentified', { keyCode: 229 });
  edit('가', 'insertCompositionText', '가'); expectText('가', false, 'Android confirmed insertCompositionText without compositionstart');
  edit('ㄱ', 'insertCompositionText', 'ㄱ', true);
  usabilityCheck(take().length === 0, 'implicit mobile composition retains unconfirmed text');
  edit('기', 'insertCompositionText', '기'); expectText('기', false, 'implicit mobile composition finalizes when isComposing becomes false');
  direct().setSelectionRange(0, direct().value.length); start();
  direct().value = '選'; direct().setSelectionRange(1, 1);
  direct().dispatchEvent(new InputEvent('input', { bubbles: true, inputType: 'insertCompositionText', data: '選', isComposing: true }));
  end('選'); await tick(); expectText('選', false, 'composition replacing the selected context preserves its first character');
  start(); edit('삭제할 조합', 'insertCompositionText', '삭제할 조합', true);
  edit('', 'deleteCompositionText', null, true); end(''); await tick();
  usabilityCheck(take().length === 0, 'canceled composition sends nothing');
  start(); edit('한', 'insertCompositionText', '한', true); end('한');
  direct().value = inputContext + '하'; direct().setSelectionRange(direct().value.length, direct().value.length); start();
  edit('하나', 'insertCompositionText', '나', true); end('나'); await tick();
  const transfer = take();
  usabilityCheck(transfer.length > 0 && transfer.every((frame) => frame.type === 'direct_input' && frame.session === 's-1')
    && transfer.map((frame) => frame.text).join('') === '하나', 'Korean consonant transfer commits final DOM ranges once');
  start(); edit('한', 'insertCompositionText', '한', true);
  edit('하', 'deleteContentBackward', null, true);
  usabilityCheck(take().length === 0, 'IME candidate deletion never sends terminal backspace');
  end('하'); await tick(); expectText('하', false, 'candidate deletion finalization');
  start(); edit('명령', 'insertCompositionText', '명령', true); end('명령'); key('Enter'); await tick();
  const ordered = take();
  usabilityCheck(ordered.length === 2 && ordered[0].text === '명령' && ordered[1].key === 'enter',
    'confirmed composition is sent before immediately following hardware Enter');
  key('Unidentified', { keyCode: 229 }); edit('!', 'insertText'); expectText('!', false, '229 key permits later confirmed text');
  edit('a'); expectText('a'); edit('a'); expectText('a', false, 'legitimate repeated text is not deduplicated');

  const escButton = document.querySelector('[data-direct-key="esc"]');
  const activeImeInput = direct();
  start(); edit('ㅎ', 'insertCompositionText', 'ㅎ', true);
  escButton.dispatchEvent(new PointerEvent('pointerdown', { bubbles: true, cancelable: true }));
  activeImeInput.value = inputContext + '한';
  activeImeInput.dispatchEvent(new CompositionEvent('compositionend', { bubbles: true, data: '한' }));
  await tick();
  usabilityCheck(take().length === 0, 'held Esc cancels preedit before an independent composition timer can commit it');
  escButton.dispatchEvent(new PointerEvent('pointerup', { bubbles: true })); escButton.click();
  const imeEsc = take();
  usabilityCheck(imeEsc.length === 1 && imeEsc[0].type === 'direct_key' && imeEsc[0].key === 'esc'
    && direct().value === inputContext, 'explicit accessory Esc cancels active preedit and sends exactly one terminal key');
  activeImeInput.value = inputContext + '한';
  activeImeInput.dispatchEvent(new CompositionEvent('compositionend', { bubbles: true, data: '한' }));
  activeImeInput.dispatchEvent(new InputEvent('input', { bubbles: true, inputType: 'insertText', data: '한' }));
  await tick(); usabilityCheck(take().length === 0, 'late IME finalization cannot replay after explicit Esc');
  usabilityById('viewer-menu-keys').click();
  for (const [interrupt, expectedKey, ctrl] of [['ctrl_c', 'c', true], ['ctrl_d', 'd', true], ['esc', 'esc', false]]) {
    const retired = direct();
    retired.focus();
    start(); edit('ㅎ', 'insertCompositionText', 'ㅎ', true);
    const button = document.querySelector(`[data-key="${interrupt}"]`);
    const down = new PointerEvent('pointerdown', { bubbles: true, cancelable: true });
    button.dispatchEvent(down);
    if (!down.defaultPrevented) button.focus(); // model native pointer focus before release.
    retired.value = inputContext + '한';
    retired.dispatchEvent(new CompositionEvent('compositionend', { bubbles: true, data: '한' }));
    await tick();
    usabilityCheck(down.defaultPrevented && document.activeElement === direct() && take().length === 0,
      `held quick ${interrupt} preserves focus and cancels blur/timer preedit before click`);
    button.dispatchEvent(new PointerEvent('pointerup', { bubbles: true })); button.click();
    const sent = take();
    usabilityCheck(sent.length === 1 && sent[0].key === expectedKey && sent[0].ctrl === ctrl && direct().value === inputContext,
      `explicit quick ${interrupt} cancels active preedit and sends exactly one interrupt`);
    retired.dispatchEvent(new CompositionEvent('compositionend', { bubbles: true, data: '한' }));
    retired.value = inputContext + '한';
    retired.dispatchEvent(new InputEvent('input', { bubbles: true, inputType: 'insertCompositionText', data: '한' }));
    await tick(); usabilityCheck(take().length === 0, `late IME commit after quick ${interrupt} stays canceled`);
  }
  usabilityById('viewer-menu-keys').click();
  start(); edit('명령', 'insertCompositionText', '명령', true); end('명령');
  escButton.dispatchEvent(new PointerEvent('pointerdown', { bubbles: true, cancelable: true })); await tick();
  usabilityCheck(take().length === 0, 'held interrupt cancels an already deferred composition timer at pointerdown');
  escButton.dispatchEvent(new PointerEvent('pointerup', { bubbles: true })); escButton.click(); await tick();
  const pendingEsc = take();
  usabilityCheck(pendingEsc.length === 1 && pendingEsc[0].key === 'esc', 'explicit interrupt cancels a deferred commit before forwarding the key');
  start(); edit('취소', 'insertCompositionText', '취소', true);
  escButton.dispatchEvent(new PointerEvent('pointerdown', { bubbles: true, cancelable: true }));
  escButton.dispatchEvent(new PointerEvent('pointercancel', { bubbles: true })); escButton.click(); await tick();
  usabilityCheck(take().length === 0 && direct().value === inputContext,
    'canceled interrupt press discards preedit without sending a terminal key');
  start(); edit('이전 세션', 'insertCompositionText', '이전 세션', true);
  escButton.dispatchEvent(new PointerEvent('pointerdown', { bubbles: true, cancelable: true }));
  escButton.dispatchEvent(new PointerEvent('pointerup', { bubbles: true }));
  usabilityById('viewer-session-chip').click(); usabilityById('viewer-session-chip').click();
  escButton.click(); await tick();
  usabilityCheck(take().length === 0 && direct().value === inputContext,
    'session transition after release invalidates a captured interrupt before its click');
  const focusInput = direct();
  start(); edit('포커스 조합', 'insertCompositionText', '포커스 조합', true);
  const focusRelease = key('M', { ctrlKey: true, shiftKey: true, isComposing: true, keyCode: 229 });
  usabilityCheck(focusRelease.defaultPrevented && document.activeElement === usabilityById('viewer-menu-button')
    && take().length === 0, 'focus-release shortcut reaches surrounding menu control even during composition');
  usabilityCheck(usabilityById(direct().getAttribute('aria-describedby'))?.textContent.includes('Ctrl+Shift+M'),
    'focus-release shortcut is visibly documented and described on the input');
  document.activeElement.click();
  usabilityCheck(!usabilityById('viewer-menu').hidden, 'keyboard-reached menu control opens the terminal menu');
  usabilityById('viewer-menu-button').click();
  focusInput.value = inputContext + '포커스 조합';
  focusInput.dispatchEvent(new CompositionEvent('compositionend', { bubbles: true, data: '포커스 조합' }));
  focusInput.dispatchEvent(new InputEvent('input', { bubbles: true, inputType: 'insertText', data: '포커스 조합' }));
  await tick(); usabilityCheck(take().length === 0, 'focus-release cancellation never commits late preedit');
  direct().focus();

  for (const [browserKey, wireKey] of [['Enter', 'enter'], ['Backspace', 'backspace'], ['Tab', 'tab'],
    ['Escape', 'esc'], ['ArrowUp', 'up'], ['ArrowDown', 'down'], ['ArrowLeft', 'left'], ['ArrowRight', 'right'],
    ['Home', 'home'], ['End', 'end'], ['Delete', 'delete'], ['Insert', 'insert'], ['PageUp', 'page_up'], ['PageDown', 'page_down']]) {
    usabilityCheck(key(browserKey).defaultPrevented, `${browserKey} prevents browser editing/navigation`);
    const sent = take();
    usabilityCheck(sent.length === 1 && sent[0].type === 'direct_key' && sent[0].key === wireKey
      && sent[0].session === 's-1' && !sent[0].meta, `${browserKey} has one semantic key frame`);
  }
  key('Tab', { shiftKey: true });
  usabilityCheck(take()[0]?.shift === true, 'hardware Shift-Tab preserves shift modifier');
  key('ArrowUp', { ctrlKey: true, altKey: true, shiftKey: true });
  const modifiedArrow = take()[0];
  usabilityCheck(modifiedArrow?.ctrl && modifiedArrow.alt && modifiedArrow.shift, 'hardware named-key modifiers are retained');
  await tick();
  for (const [type, wireKey] of [['deleteContentBackward', 'backspace'], ['deleteContentForward', 'delete'], ['insertLineBreak', 'enter']]) {
    edit('', type, null, false, true);
    const sent = take();
    usabilityCheck(sent.length === 1 && sent[0].key === wireKey, `${type} beforeinput/input sends one soft key`);
    await tick();
  }
  key('Backspace'); edit('', 'deleteContentBackward', null, false, true);
  usabilityCheck(take().length === 1, 'hardware backspace and trailing edit do not duplicate');
  await tick();
  key('Enter'); edit('\n', 'insertLineBreak', null, false, true);
  usabilityCheck(take().length === 1, 'hardware Enter and trailing line break do not duplicate');
  await tick();
  direct().value = '';
  direct().dispatchEvent(new InputEvent('input', { bubbles: true, inputType: 'deleteContentBackward' }));
  usabilityCheck(take()[0]?.key === 'backspace' && direct().value === inputContext,
    'mobile-only input deletion has a fallback and restores idle context');
  key('c', { ctrlKey: true }); edit('c', 'insertText', 'c', false, true);
  usabilityCheck(take().length === 1, 'hardware Ctrl-C with trailing text edit does not duplicate');
  await tick();
  usabilityById('direct-ctrl').click(); edit('c');
  const ctrl = take()[0];
  usabilityCheck(ctrl?.type === 'direct_key' && ctrl.key === 'c' && ctrl.ctrl, 'Ctrl accessory modifies confirmed ASCII');
  usabilityCheck(usabilityById('direct-ctrl').getAttribute('aria-pressed') === 'false', 'Ctrl accessory clears after use');
  usabilityById('direct-alt').click(); key('ArrowLeft');
  usabilityCheck(take()[0]?.alt === true, 'Alt accessory modifies named keys');
  for (const wireKey of ['enter', 'backspace', 'tab', 'esc']) {
    document.querySelector(`[data-direct-key="${wireKey}"]`).click();
    usabilityCheck(take()[0]?.key === wireKey, `touch ${wireKey} dispatches a semantic key`);
  }
  const repeatUp = document.querySelector('[data-direct-key="up"]');
  repeatUp.dispatchEvent(new PointerEvent('pointerdown', { bubbles: true, cancelable: true }));
  await new Promise((resolve) => setTimeout(resolve, 570));
  repeatUp.dispatchEvent(new PointerEvent('pointerup', { bubbles: true })); repeatUp.click();
  const repeatedUp = take();
  usabilityCheck(repeatedUp.length >= 1 && repeatedUp.every((frame) => frame.session === 's-1' && frame.key === 'up'),
    'held touch arrows repeat on captured session and consume release click');
  key('d', { ctrlKey: true });
  usabilityCheck(take()[0]?.ctrl === true, 'hardware Ctrl ASCII reaches terminal');
  const altGraph = new KeyboardEvent('keydown', { bubbles: true, cancelable: true, key: '@', ctrlKey: true, altKey: true });
  Object.defineProperty(altGraph, 'getModifierState', { value: (name) => name === 'AltGraph' });
  direct().dispatchEvent(altGraph); edit('@'); expectText('@', false, 'AltGraph remains printable input');
  usabilityCheck(!key('c', { metaKey: true }).defaultPrevented && take().length === 0, 'Meta shortcuts remain browser-local');
  usabilityCheck(!key('v', { ctrlKey: true }).defaultPrevented && take().length === 0, 'Ctrl-V remains browser paste intent');
  const clipboard = new DataTransfer(); clipboard.setData('text/plain', 'x');
  const pasteEvent = new ClipboardEvent('paste', { bubbles: true, cancelable: true, clipboardData: clipboard });
  direct().dispatchEvent(pasteEvent);
  edit('x', 'insertFromPaste', 'x', false, true); expectText('x', true, 'tiny real paste and trailing edit');
  usabilityCheck(pasteEvent.defaultPrevented, 'real paste prevents duplicate native insertion');
  await tick();
  edit('두 줄\r\n  공백\t끝', 'insertFromPaste', '두 줄\r\n  공백\t끝', false, true);
  expectText('두 줄\r\n  공백\t끝', true, 'paste fallback preserves raw text for backend normalization');
  await tick();

  composerMode.click(); composer.value = '긴 지시 초안'; composer.dispatchEvent(new Event('input', { bubbles: true }));
  directMode.click(); edit('터미널'); expectText('터미널'); composerMode.click();
  usabilityCheck(composer.value === '긴 지시 초안', 'mode switching preserves independent long-instruction draft');
  usabilityCheck(!document.querySelector('.composer').hidden && usabilityById('direct-controls').hidden, 'composer mode is explicit');
  directMode.click();
  const retiredModeInput = direct();
  start(); edit('중단', 'insertCompositionText', '중단', true); end('중단'); composerMode.click(); await tick();
  usabilityCheck(take().length === 0 && composer.value === '긴 지시 초안', 'mode change invalidates pending direct commit without draft replay');
  directMode.click();
  retiredModeInput.value = inputContext + '중단';
  retiredModeInput.dispatchEvent(new InputEvent('input', { bubbles: true, inputType: 'insertText', data: '중단' }));
  usabilityCheck(take().length === 0, 'late Safari-style input on retired mode target cannot replay after returning to direct mode');
  const retiredSessionInput = direct();
  start(); edit('이전 세션', 'insertCompositionText', '이전 세션', true); end('이전 세션');
  repeatUp.dispatchEvent(new PointerEvent('pointerdown', { bubbles: true, cancelable: true }));
  usabilityById('viewer-session-chip').click(); await tick();
  repeatUp.dispatchEvent(new PointerEvent('pointerup', { bubbles: true })); repeatUp.click();
  usabilityCheck(take().length === 0, 'session transition invalidates pending direct commit');
  retiredSessionInput.value = inputContext + '이전 세션';
  retiredSessionInput.dispatchEvent(new InputEvent('input', { bubbles: true, inputType: 'insertCompositionText', data: '이전 세션' }));
  usabilityCheck(take().length === 0, 'late native composition input cannot target a different watched UUID');
  edit('B'); const sessionB = take();
  usabilityCheck(sessionB.length === 1 && sessionB[0].session === 's-2' && sessionB[0].text === 'B', 'direct text targets current session UUID');
  usabilityById('viewer-session-chip').click();
  const oldSocket = mobileUsabilitySocket;
  const retiredSocketInput = direct();
  start(); edit('연결 유실', 'insertCompositionText', '연결 유실', true); end('연결 유실'); oldSocket.close(); await tick();
  usabilityCheck(take().length === 0 && direct().disabled, 'disconnect invalidates pending direct commit');
  await usabilityWait(() => mobileUsabilitySocket !== oldSocket && !direct().disabled);
  usabilityCheck(take().length === 0 && direct().value === inputContext, 'reconnect never replays stale direct text');
  retiredSocketInput.value = inputContext + '연결 유실';
  retiredSocketInput.dispatchEvent(new InputEvent('input', { bubbles: true, inputType: 'insertText', data: '연결 유실' }));
  usabilityCheck(take().length === 0, 'late input on retired native target cannot write to a fresh socket');
  oldSocket.message({ type: 'input_pressure', session: 's-1', reason: 'unavailable', queued: 1 });
  usabilityCheck(!direct().disabled, 'retired socket cannot lock fresh connection');
  mobileUsabilitySocket.readyState = 0; edit('닫힌 전송'); mobileUsabilitySocket.readyState = 1;
  usabilityCheck(take().length === 0, 'live socket readiness is required in addition to connection label');
  start(); edit('페이지 유실', 'insertCompositionText', '페이지 유실', true); end('페이지 유실');
  window.dispatchEvent(new PageTransitionEvent('pagehide')); await tick();
  usabilityCheck(take().length === 0 && direct().value === inputContext, 'pagehide invalidates pending direct commit');
  mobileUsabilitySocket.message({ type: 'input_pressure', session: 's-1', reason: 'admission_denied', queued: 0 });
  usabilityCheck(!direct().disabled && !usabilityById('composer-note').hidden, 'transient mode denial reports status without disabling direct input');
  key('c', { ctrlKey: true }); key('d', { ctrlKey: true }); key('Escape'); edit('続');
  const interruptFrames = take();
  usabilityCheck(interruptFrames.length === 4 && interruptFrames[0].key === 'c' && interruptFrames[1].key === 'd'
    && interruptFrames[2].key === 'esc' && interruptFrames[3].text === '続', 'mode-refresh denial keeps interrupts and ordinary text usable');
  mobileUsabilitySocket.message({ type: 'input_pressure', session: 's-1', reason: 'queue_full', queued: 1 });
  mobileUsabilitySocket.message({ type: 'input_pressure', session: 's-1', reason: 'admission_denied', queued: 0 });
  usabilityCheck(direct().disabled && !usabilityById('composer-note').hidden, 'pressure locks direct input and status stays visible');
  composerMode.click(); directMode.click(); edit('압박 중 입력'); key('ArrowUp'); usabilityById('direct-ctrl').click();
  usabilityCheck(take().length === 0 && !usabilityById('composer-note').hidden, 'mode changes never bypass pressure lock or hide status');
  mobileUsabilitySocket.message({ type: 'input_pressure', session: 's-1', reason: 'queue_full', queued: 0 });
  edit('복귀'); expectText('복귀', false, 'pressure resolution accepts new text only');
  start(); edit('대기열 이전 조합', 'insertCompositionText', '대기열 이전 조합', true);
  mobileUsabilitySocket.message({ type: 'input_pressure', session: 's-1', reason: 'queue_full', queued: 1 });
  mobileUsabilitySocket.message({ type: 'input_pressure', session: 's-1', reason: 'queue_full', queued: 0 });
  end('대기열 이전 조합'); await tick();
  usabilityCheck(take().length === 0 && direct().value === inputContext, 'queue lock invalidates active composition without replay after resolution');
  usabilityById('viewer-session-chip').click();
  mobileUsabilitySocket.message({ type: 'input_pressure', session: 's-2', reason: 'closed', queued: 0 });
  edit('종료 세션'); key('Enter');
  usabilityCheck(take().length === 0 && direct().disabled, 'terminal closed state blocks direct text and keys');
  usabilityById('viewer-session-chip').click();
  composerMode.click();
}

async function runMobileUsability() {
  await usabilityWait(() => document.querySelector('.ws-open'));
  document.querySelector('.ws-open').click();
  await usabilityWait(() => usabilityById('viewer-canvas').width > 0);
  await usabilitySettle();
  if (sessionStorage.getItem('mobile-mode-restore') === 'yes') {
    usabilityCheck(usabilityById('viewer-mode-composer').getAttribute('aria-pressed') === 'true'
      && !document.querySelector('.composer').hidden && usabilityById('direct-controls').hidden,
      'device input mode preference survives reload');
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
  await runDirectInputChecks();
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
  sessionStorage.setItem('mobile-mode-restore', 'yes');
  sessionStorage.setItem('mobile-usability-checks', String(usabilityChecks));
  location.reload();
}
runMobileUsability().catch((error) => {
  document.body.append('MOBILE_USABILITY_ERROR:' + error.message);
  document.body.dataset.status = 'error';
});
