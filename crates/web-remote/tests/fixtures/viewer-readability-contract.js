// Real browser assertions for readable grid geometry; joined after viewer-core.js.
(async () => {
  const check = (condition, message) => {
    if (!condition) throw new Error(message);
    checks += 1;
  };
  let checks = 0;
  const near = (actual, expected, message) => check(Math.abs(actual - expected) < 0.05, `${message}: ${actual} != ${expected}`);
  const settle = () => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)));
  const calls = [];
  const originalText = CanvasRenderingContext2D.prototype.fillText;
  const originalRect = CanvasRenderingContext2D.prototype.fillRect;
  CanvasRenderingContext2D.prototype.fillText = function(text, x, y, width) {
    calls.push({kind: 'text', text, x, y, width, font: this.font, color: this.fillStyle});
    return originalText.call(this, text, x, y, width);
  };
  CanvasRenderingContext2D.prototype.fillRect = function(x, y, width, height) {
    calls.push({kind: 'rect', x, y, width, height, color: this.fillStyle});
    return originalRect.call(this, x, y, width, height);
  };
  const finish = (status, message) => {
    document.body.dataset.status = status;
    document.body.textContent = message;
  };
  try {
    localStorage.removeItem('deppy-viewer:prefs');
    const sent = [];
    const layouts = [];
    let pans = 0;
    const viewer = createViewer({
      send: message => sent.push(message),
      hooks: {layoutChanged: metrics => layouts.push(metrics), pan: () => pans++},
    });
    check(typeof viewer.settings === 'function', 'readable settings API missing');
    check(viewer.settings().fontSize === 15, 'default font must be 15px');
    check(viewer.settings().overview === false, 'default must preserve readable grid');
    viewer.setViewerConnection('connected');
    viewer.openViewer('readable-A', 'A');
    await settle();
    check(layouts.length > 0, 'layout hook must fire before first snapshot');
    check(layouts.at(-1).stageWidth > 0 && layouts.at(-1).stageHeight > 0, 'layout hook needs actual stage dimensions');
    const frame = (session = viewer.watching, cursor = {visible: false}) => ({
      session, keyframe: true, cols: 180, rows: 40, cursor,
      lines: [{row: 0, runs: [
        {s: 0, t: '한👨‍👩‍👧‍👦', g: ['한', '👨‍👩‍👧‍👦'], w: true, fg: '#ffaa00', bg: '#123456', a: 31},
        {s: 4, t: 'a\u0301\u0308Z', g: ['a\u0301\u0308', 'Z'], fg: '#aabbcc'},
        {s: 8, t: '┌─┬─┐'},
      ]}],
    });
    viewer.handleViewport(frame());
    await settle();
    const expectedWidth = (() => {
      const ctx = document.createElement('canvas').getContext('2d');
      ctx.font = '15px ui-monospace, Menlo, monospace';
      return ctx.measureText('M'.repeat(32)).width / 32;
    })();
    const metrics = viewer.getCellMetrics();
    near(metrics.cellWidth, expectedWidth, 'cell width must be measured from selected font');
    near(metrics.fontSize, 15, 'normal font size');
    check(metrics.cellHeight >= metrics.fontSize, 'cell height must contain selected font');
    for (const [width, height] of [[320, 480], [390, 620], [430, 700], [844, 305], [390, 270]]) {
      viewer.wrap.style.width = width + 'px';
      viewer.wrap.style.height = height + 'px';
      viewer.scheduleViewerRender();
      await settle();
      const current = viewer.getCellMetrics();
      near(current.fontSize, 15, `font at ${width}x${height}`);
      near(parseFloat(viewer.canvas.style.width), current.cellWidth * 180, 'full grid width');
      near(parseFloat(viewer.canvas.style.height), current.cellHeight * 40, 'full grid height');
      check(viewer.wrap.scrollWidth > viewer.wrap.clientWidth, 'horizontal local overflow missing');
      check(viewer.wrap.scrollHeight > viewer.wrap.clientHeight, 'vertical local overflow missing');
      check(getComputedStyle(viewer.wrap).overflowX === 'auto', 'grid must use native local scroll');
      viewer.wrap.scrollLeft = 0; viewer.wrap.scrollTop = 0;
      const canvasRect = viewer.canvas.getBoundingClientRect();
      const wrapRect = viewer.wrap.getBoundingClientRect();
      near(canvasRect.left, wrapRect.left, 'grid starts at left');
      near(canvasRect.top, wrapRect.top, 'grid starts at top');
      near(layouts.at(-1).stageWidth, viewer.wrap.clientWidth, 'layout width after geometry update');
      near(layouts.at(-1).stageHeight, viewer.wrap.clientHeight, 'layout height after geometry update');
    }
    const glyphs = calls.filter(call => call.kind === 'text');
    const first = glyphs.findIndex(call => call.text === '한');
    const owners = glyphs.slice(first, first + 4);
    check(owners.map(call => call.text).join('|') === '한|👨‍👩‍👧‍👦|a\u0301\u0308|Z', 'owner graphemes were split or dropped');
    near(owners[1].x - owners[0].x, metrics.cellWidth * 2, 'wide owner advance');
    near(owners[3].x - owners[2].x, metrics.cellWidth, 'narrow owner advance');
    check(owners[0].font.includes('italic') && /bold|700/.test(owners[0].font), 'ANSI bold and italic lost');
    check(owners[0].color === '#996600', 'ANSI dim color lost');
    check(calls.some(call => call.kind === 'rect' && call.color === '#123456' && Math.abs(call.width - metrics.cellWidth * 4) < 0.05), 'wide ANSI background geometry');
    for (const shape of ['block', 'underline', 'beam']) {
      calls.length = 0;
      viewer.handleViewport(frame(undefined, {visible: true, row: 0, col: 1, shape}));
      await settle();
      const cursor = calls.findLast(call => call.kind === 'rect' && call.color.startsWith('rgba'));
      check(!!cursor, `${shape} cursor missing`);
      near(cursor.x, 0, `${shape} cursor uses wide owner column`);
      if (shape === 'beam') {
        check(cursor.width <= 2 && cursor.height === metrics.cellHeight, 'beam must be narrow and full height');
      } else {
        near(cursor.width, metrics.cellWidth * 2, `${shape} cursor wide owner span`);
        if (shape === 'block') near(cursor.height, metrics.cellHeight, 'block height');
        else check(cursor.height <= 2 && cursor.y > metrics.cellHeight - 3, 'underline must stay on cell bottom');
      }
    }
    for (const [boundary, col, previousCol] of [['right', 170, 0], ['left', 101, 179]]) {
      viewer.handleViewport(frame(undefined, {visible: true, row: 0, col: previousCol}));
      await settle();
      calls.length = 0;
      const wideFrame = frame(undefined, {visible: true, row: 0, col, shape: 'block'});
      wideFrame.lines = [{row: 0, runs: [{s: boundary === 'right' ? 170 : 100, t: '한', g: ['한'], w: true}]}];
      viewer.handleViewport(wideFrame);
      await settle();
      const cursor = calls.findLast(call => call.kind === 'rect' && call.color.startsWith('rgba'));
      check(cursor.x >= viewer.wrap.scrollLeft - 1, `${boundary} wide cursor owner left edge must be visible`);
      check(cursor.x + cursor.width <= viewer.wrap.scrollLeft + viewer.wrap.clientWidth + 1,
        `${boundary} wide cursor complete span must be visible`);
    }
    viewer.handleViewport(frame(undefined, {visible: true, row: 39, col: 175, shape: 'block'}));
    await settle();
    check(viewer.followingLive === true, 'initial grid follows live output');
    check(viewer.wrap.scrollLeft > 1000 && viewer.wrap.scrollTop > 500, 'live cursor must come into view');
    const wheel = new WheelEvent('wheel', {deltaY: -50, bubbles: true, cancelable: true});
    viewer.canvas.dispatchEvent(wheel);
    check(!wheel.defaultPrevented && pans === 0, 'normal scrolling must stay local');
    check(viewer.followingLive === false, 'user wheel disables live following');
    check(!viewer.scrollNote.hidden && viewer.offsetText.textContent.includes('일시정지'), 'local paused follow needs visible notice');
    viewer.wrap.scrollLeft = 155; viewer.wrap.scrollTop = 210;
    viewer.wrap.dispatchEvent(new Event('scroll'));
    viewer.handleViewport({...frame(), keyframe: false});
    await settle();
    near(viewer.wrap.scrollLeft, 155, 'new output preserves horizontal reading position');
    near(viewer.wrap.scrollTop, 210, 'new output preserves vertical reading position');
    viewer.openViewer('readable-B', 'B');
    viewer.handleViewport(frame('readable-B'));
    await settle();
    near(viewer.wrap.scrollLeft, 0, 'new session horizontal position');
    near(viewer.wrap.scrollTop, 0, 'new session vertical position');
    viewer.wrap.scrollLeft = 70; viewer.wrap.scrollTop = 80;
    viewer.openViewer('readable-A', 'A');
    viewer.handleViewport(frame('readable-A'));
    await settle();
    near(viewer.wrap.scrollLeft, 155, 'restored session horizontal position');
    near(viewer.wrap.scrollTop, 210, 'restored session vertical position');
    check(viewer.followingLive === false, 'session must restore reading state');
    viewer.handleViewport(frame(undefined, {visible: true, row: 39, col: 175}));
    await settle();
    near(viewer.wrap.scrollLeft, 155, 'reading position ignores moving cursor');
    near(viewer.wrap.scrollTop, 210, 'reading position ignores latest row');
    viewer.resumeFollowButton.click();
    await settle();
    check(viewer.followingLive === true, 'explicit follow resumes');
    check(viewer.wrap.scrollLeft > 1000 && viewer.wrap.scrollTop > 500, 'resume must reveal current live cursor');
    check(viewer.scrollNote.hidden, 'live follow hides paused notice');
    viewer.setFontSize(6);
    await settle();
    check(viewer.settings().fontSize === 12, 'font minimum must be 12px');
    check(viewer.followingLive === true, 'font shrink clamping must preserve live following');
    viewer.wrap.style.height = '500px';
    await settle();
    check(viewer.followingLive === true, 'larger stage clamping must preserve live following');
    viewer.wrap.style.height = '270px';
    await settle();
    check(viewer.followingLive === true, 'smaller stage must preserve live following');
    viewer.setFontSize(100);
    await settle();
    check(viewer.settings().fontSize === 24, 'font maximum must be 24px');
    near(viewer.getCellMetrics().fontSize, 24, 'font setting must reach renderer');
    viewer.setFontSize(NaN);
    check(viewer.settings().fontSize === 24, 'invalid font must preserve current value');
    viewer.setOverview(true);
    await settle();
    check(viewer.settings().overview === true, 'explicit overview setting');
    check(viewer.followingLive === true, 'overview layout clamping must preserve live following');
    check(viewer.getCellMetrics().fontSize < 12, 'overview may shrink independently of readable setting');
    check(viewer.canvas.getBoundingClientRect().width <= viewer.wrap.clientWidth + 1, 'overview width must fit');
    check(viewer.canvas.getBoundingClientRect().height <= viewer.wrap.clientHeight + 1, 'overview height must fit');
    const restored = createViewer().settings();
    check(restored.fontSize === 24 && restored.overview === true, 'device settings must restore from storage');
    viewer.setOverview(false);
    await settle();
    check(viewer.followingLive === true, 'exit overview must preserve live following');
    near(viewer.getCellMetrics().fontSize, 24, 'exit overview restores configured size');
    const snapshot = viewer.settings(); snapshot.fontSize = 3;
    check(viewer.settings().fontSize === 24, 'settings must be a copy');
    check(sent.every(message => ['watch', 'unwatch', 'request_keyframe'].includes(message.type)), 'renderer must remain read-only');
    viewer.finishCloseViewer();
    finish('ok', `VIEWER_READABILITY_OK:${checks}`);
  } catch (error) {
    finish('error', 'VIEWER_READABILITY_ERROR: ' + (error.stack || error));
  }
})();
