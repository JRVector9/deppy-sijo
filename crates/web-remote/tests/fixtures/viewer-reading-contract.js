// Selection, search, wrapping, and edge gestures in the actual read-only renderer.
(async () => {
  let checks = 0;
  const check = (condition, message) => { if (!condition) throw Error(message); checks++; };
  const near = (actual, expected, message) => check(Math.abs(actual - expected) < 1, `${message}: ${actual} != ${expected}`);
  const settle = () => new Promise(done => requestAnimationFrame(() => requestAnimationFrame(done)));
  const finish = (status, message) => { document.body.dataset.status = status; document.body.textContent = message; };
  try {
    localStorage.removeItem('deppy-viewer:prefs');
    const sent = [], pans = [], searches = [];
    const viewer = createViewer({send: msg => sent.push(msg), hooks: {
      pan: lines => pans.push(lines), searchChanged: result => searches.push(result),
    }});
    check(typeof viewer.setReadableWrap === 'function', 'reading mode API missing');
    check(viewer.settings().readableWrap === false, 'grid remains default');
    viewer.wrap.style.width = '320px'; viewer.wrap.style.height = '150px';
    viewer.setViewerConnection('connected'); viewer.openViewer('reading-A', 'A');
    const first = {row: 0, runs: [
      {s: 0, t: '한👨‍👩‍👧‍👦', g: ['한', '👨‍👩‍👧‍👦'], w: true, a: 3, fg: '#ffaa00', bg: '#123456'},
      {s: 6, t: 'a\u0301\u0308X', g: ['a\u0301\u0308', 'X']},
      {s: 10, t: '<b>find.+ find.+</b>' + ' readable-output '.repeat(3), fg: '#59d185'},
    ]};
    const second = {row: 1, runs: [{s: 0, t: 'secondline'}]};
    const last = {row: 19, runs: [{s: 0, t: 'latest'}]};
    const frame = (session = viewer.watching) => ({session, keyframe: true, cols: 80, rows: 20,
      cursor: {visible: true, row: 19, col: 79, shape: 'block'}, lines: [first, second, last]});
    let requestedFrame = 1000;
    // Row-restoration tests intentionally refresh the same loaded source window.
    // Passive advancing output retention is separately exercised by viewer-history-contract.js.
    const applyFrame = msg => {
      const request = viewer.screen && viewer.getHistoryState().retained ? ++requestedFrame : undefined;
      if (request) viewer.expectHistoryWindow(request);
      viewer.handleViewport({...msg, history: {generation: '1', first_line: '0', total_lines: 20, request, expired: false}});
    };
    viewer.openViewer('reading-layout', 'layout');
    const layoutFrame = offset => ({...frame(), rows: 2, lines: [first, second], offset,
      cursor: {visible: true, row: 0, col: 0}});
    const firstRowPoint = () => {
      const cell = viewer.textLayer.querySelector('[data-row="0"] .viewer-text-cell');
      const rect = cell.getBoundingClientRect();
      return {x: rect.left + rect.width / 2, y: rect.top + rect.height / 2, cell};
    };
    viewer.el.style.width = '320px'; viewer.wrap.style.width = '100%';
    for (const readable of [false, true]) {
      viewer.setReadableWrap(readable);
      applyFrame(layoutFrame(0)); viewer.resumeFollow(); await settle();
      check(viewer.scrollNote.hidden, 'following starts with hidden note');
      for (const retained of [false, true]) {
        if (retained) {
          window.getSelection().removeAllRanges();
          applyFrame(layoutFrame(5)); await settle();
          check(!viewer.scrollNote.hidden, 'historical note is already visible before selection');
        }
        const before = viewer.wrap.getBoundingClientRect();
        const point = firstRowPoint();
        check(viewer.textLayer.contains(document.elementFromPoint(point.x, point.y)), 'first row is hit-testable before selection');
        const range = document.createRange(); range.selectNodeContents(point.cell);
        window.getSelection().removeAllRanges(); window.getSelection().addRange(range);
        document.dispatchEvent(new Event('selectionchange')); await settle();
        check(!viewer.scrollNote.hidden, 'paused current-screen action remains available');
        const after = viewer.wrap.getBoundingClientRect();
        for (const key of ['top', 'left', 'width', 'height']) near(after[key], before[key], 'note must not change terminal bounds during selection ' + key);
        check(viewer.scrollNote.getBoundingClientRect().bottom <= after.top,
          'status/action rail must never obstruct selected terminal text');
        check(viewer.textLayer.contains(document.elementFromPoint(point.x, point.y)), 'first-row pointer remains on terminal while note is visible');
      }
      window.getSelection().removeAllRanges(); applyFrame(layoutFrame(0)); viewer.resumeFollow(); await settle();
      const expiryPoint = firstRowPoint();
      const expiryRange = document.createRange(); expiryRange.selectNodeContents(expiryPoint.cell);
      window.getSelection().addRange(expiryRange); document.dispatchEvent(new Event('selectionchange')); await settle();
      const expiryText = viewer.getSelectedText(), stage = viewer.wrap.parentElement;
      const stageBeforeExpiry = stage.getBoundingClientRect(), wrapBeforeExpiry = viewer.wrap.getBoundingClientRect();
      const textHeightBeforeExpiry = viewer.textLayer.getBoundingClientRect().height;
      near(stageBeforeExpiry.width, 320, 'narrow terminal stage starts at shell width');
      viewer.handleViewport({...layoutFrame(0), history: {generation: '2', first_line: '100', total_lines: 20, expired: false}}); await settle();
      check(viewer.getHistoryState().expired && viewer.offsetText.textContent.includes('만료'), 'passive generation change shows real long expiry label');
      viewer.offsetText.textContent += ' ' + viewer.offsetText.textContent;
      near(stage.getBoundingClientRect().width, stageBeforeExpiry.width, 'long expiry rail must not widen narrow terminal stage');
      check(viewer.resumeFollowButton.getBoundingClientRect().right <= viewer.el.getBoundingClientRect().right,
        'long expiry label must not clip current-screen action at 320px');
      const wrapAfterExpiry = viewer.wrap.getBoundingClientRect();
      for (const key of ['top', 'left', 'width', 'height']) near(wrapAfterExpiry[key], wrapBeforeExpiry[key], 'expiry must preserve selected terminal bounds ' + key);
      near(viewer.textLayer.getBoundingClientRect().height, textHeightBeforeExpiry, 'expiry label must not reflow selected readable text');
      check(viewer.getSelectedText() === expiryText && viewer.textLayer.contains(window.getSelection().anchorNode)
        && viewer.textLayer.contains(window.getSelection().focusNode), 'expiry label preserves terminal-owned copied selection');
      const point = firstRowPoint();
      viewer.setPrivacyCurtain(true);
      check(viewer.privacy.contains(document.elementFromPoint(point.x, point.y)), 'privacy overlay still covers first terminal row below rail');
      check(viewer.textLayer.inert && viewer.getSelectedText() === '', 'rail layout preserves protected selection guard');
      viewer.setPrivacyCurtain(false); viewer.setViewerConnection('reconnecting'); await settle();
      const reconnectPoint = firstRowPoint();
      check(viewer.overlay.contains(document.elementFromPoint(reconnectPoint.x, reconnectPoint.y)), 'connection overlay still covers first terminal row below rail');
      viewer.setViewerConnection('connected');
    }
    viewer.el.style.width = ''; viewer.wrap.style.width = '320px';
    viewer.setReadableWrap(false); viewer.openViewer('reading-A', 'A');
    applyFrame(frame()); await settle();
    check(viewer.textLayer && viewer.textLayer.parentNode === viewer.wrap, 'selectable text layer missing');
    check(getComputedStyle(viewer.canvas).display !== 'none', 'grid canvas must remain visible');
    const row = () => viewer.textLayer.querySelector('[data-row="0"]');
    check(row().textContent.startsWith('한👨‍👩‍👧‍👦  a\u0301\u0308X  <b>find.+ find.+</b>'), 'owner cell blank gaps or text lost');
    check(!viewer.textLayer.querySelector('b'), 'terminal text must not become HTML');
    const cells = row().querySelectorAll('.viewer-text-cell');
    const metrics = viewer.getCellMetrics();
    near(cells[0].getBoundingClientRect().width, metrics.cellWidth * 2, 'Hangul owner geometry');
    near(cells[1].getBoundingClientRect().width, metrics.cellWidth * 2, 'emoji owner geometry');
    near(cells[3].getBoundingClientRect().left - cells[0].getBoundingClientRect().left, metrics.cellWidth * 6, 'gap advances by terminal columns');
    check(cells[1].textContent === '👨‍👩‍👧‍👦', 'emoji must stay one owner');
    const select = target => {
      const range = document.createRange(); range.selectNodeContents(target);
      const selection = window.getSelection(); selection.removeAllRanges(); selection.addRange(range);
      document.dispatchEvent(new Event('selectionchange'));
    };
    select(cells[1]); await settle();
    check(viewer.getSelectedText() === '👨‍👩‍👧‍👦', 'selected grapheme copy text');
    check(!viewer.followingLive, 'selection must pause live following');
    const selectedTop = viewer.wrap.scrollTop, selectedLeft = viewer.wrap.scrollLeft;
    applyFrame({...frame(), keyframe: false, lines: [last]}); await settle();
    check(row().querySelectorAll('.viewer-text-cell')[1] === cells[1], 'unrelated delta must keep selected node');
    check(viewer.getSelectedText() === '👨‍👩‍👧‍👦', 'delta destroyed selection');
    near(viewer.wrap.scrollTop, selectedTop, 'selected output stays vertically stable');
    near(viewer.wrap.scrollLeft, selectedLeft, 'selected output stays horizontally stable');
    applyFrame(frame()); await settle();
    check(row().querySelectorAll('.viewer-text-cell')[1] === cells[1], 'identical keyframe must retain DOM nodes');
    check(viewer.getSelectedText() === '👨‍👩‍👧‍👦', 'identical frame destroyed selection');
    applyFrame({...frame(), keyframe: false, lines: [{...first, runs: first.runs.map(run => ({...run, fg: '#abcdef'}))}]}); await settle();
    check(viewer.getSelectedText() === '👨‍👩‍👧‍👦', 'style-only delta must restore selection');
    const multiRange = document.createRange();
    multiRange.setStart(row().querySelectorAll('.viewer-text-cell')[1].firstChild, 0);
    const endCell = viewer.textLayer.querySelector('[data-row="1"]').querySelectorAll('.viewer-text-cell')[9];
    multiRange.setEnd(endCell.firstChild, 1);
    window.getSelection().removeAllRanges(); window.getSelection().addRange(multiRange);
    document.dispatchEvent(new Event('selectionchange'));
    const multiText = viewer.getSelectedText();
    check(multiText.includes('\n') && multiText.endsWith('secondline'), 'multiline native selection preserves line boundaries');
    applyFrame({...frame(), keyframe: false, lines: [{...first, runs: first.runs.map(run => ({...run, fg: '#fedcba'}))}]}); await settle();
    check(viewer.getSelectedText() === multiText, 'multiline selection survives selected-row style update');
    const clipboard = new DataTransfer();
    const copy = new ClipboardEvent('copy', {clipboardData: clipboard, cancelable: true});
    document.dispatchEvent(copy);
    check(copy.defaultPrevented && clipboard.getData('text/plain') === multiText, 'native copy exports exact plain terminal text');
    for (const readable of [false, true]) {
      viewer.setReadableWrap(readable); await settle();
      for (const [prefix, changedPrefix, wide] of [['a', 'a\u0301', false], ['한', '한\u0301', true], ['👩', '👩🏽', true]]) {
        const startCol = wide ? 2 : 1;
        const prefixLine = text => ({row: 2, runs: [
          {s: 0, t: text, g: [text], w: wide},
          {s: startCol, t: 'KEEP'},
          {s: startCol + 4, t: '한👨‍👩‍👧‍👦', g: ['한', '👨‍👩‍👧‍👦'], w: true},
          {s: startCol + 8, t: 'a\u0301', g: ['a\u0301']},
        ]});
        applyFrame({...frame(), keyframe: false, lines: [prefixLine(prefix)]}); await settle();
        const owners = viewer.textLayer.querySelector('[data-row="2"]').querySelectorAll('.viewer-text-cell');
        const stableRange = document.createRange();
        stableRange.setStart(owners[1].firstChild, 0); stableRange.setEnd(owners[4].firstChild, owners[4].textContent.length);
        window.getSelection().removeAllRanges(); window.getSelection().addRange(stableRange);
        document.dispatchEvent(new Event('selectionchange'));
        const kept = viewer.getSelectedText();
        check(kept === 'KEEP', 'owner selection test setup');
        applyFrame({...frame(), keyframe: false, lines: [prefixLine(changedPrefix)]}); await settle();
        check(viewer.getSelectedText() === kept, `${readable ? 'reading' : 'grid'} selection survives ${prefix} owner UTF16 growth before it`);
        const changedOwners = viewer.textLayer.querySelector('[data-row="2"]').querySelectorAll('.viewer-text-cell');
        stableRange.setStart(changedOwners[1].firstChild, 0);
        stableRange.setEnd(changedOwners[7].firstChild, changedOwners[7].textContent.length);
        window.getSelection().removeAllRanges(); window.getSelection().addRange(stableRange);
        const mixed = viewer.getSelectedText();
        check(mixed === 'KEEP한👨‍👩‍👧‍👦a\u0301', 'mixed grapheme selection test setup');
        applyFrame({...frame(), keyframe: false, lines: [prefixLine(changedPrefix + '\u0308')]}); await settle();
        check(viewer.getSelectedText() === mixed, 'CJK/emoji/combining selected owners survive prefix growth');
      }
    }
    window.getSelection().removeAllRanges(); viewer.setReadableWrap(false); await settle();
    for (const readable of [false, true]) {
      viewer.setReadableWrap(readable); await settle();
      for (const empty of [false, true]) {
        const boundaryLine = {row: 3, runs: empty ? [] : [{s: 0, t: 'a', g: ['a']}]};
        const keepLine = {row: 4, runs: [{s: 0, t: 'KEEP'}]};
        applyFrame({...frame(), keyframe: false, lines: [boundaryLine, keepLine]}); await settle();
        const boundaryRow = viewer.textLayer.querySelector('[data-row="3"]');
        const keepRow = viewer.textLayer.querySelector('[data-row="4"]');
        const boundaryRange = document.createRange();
        const finalText = boundaryRow.lastElementChild.firstChild;
        boundaryRange.setStart(finalText, finalText.length);
        boundaryRange.setEnd(keepRow.querySelectorAll('.viewer-text-cell')[3].firstChild, 1);
        window.getSelection().removeAllRanges(); window.getSelection().addRange(boundaryRange);
        document.dispatchEvent(new Event('selectionchange'));
        const copiedBoundary = viewer.getSelectedText();
        check(copiedBoundary.endsWith('KEEP') && copiedBoundary.includes('\n'), 'row-end boundary selection setup');
        applyFrame({...frame(), keyframe: false, lines: [{...keepLine, runs: [{s: 0, t: 'KEEP', a: 3}]}]}); await settle();
        check(viewer.getSelectedText() === copiedBoundary, `${readable ? 'reading' : 'grid'} ${empty ? 'empty' : 'text'} row-end selection survives ANSI delta`);
        applyFrame({...frame(), keyframe: false, lines: [{row: 3, runs: [{s: 0, t: 'a\u0301', g: ['a\u0301']}]}]}); await settle();
        check(viewer.getSelectedText() === copiedBoundary, 'row-end boundary does not expand on preceding Unicode growth');
      }
      const emptyLine = {row: 3, runs: []};
      applyFrame({...frame(), keyframe: false, lines: [emptyLine, {row: 4, runs: [{s: 0, t: 'KEEP'}]}]}); await settle();
      const emptyRow = viewer.textLayer.querySelector('[data-row="3"]'); emptyRow.replaceChildren();
      const emptyRange = document.createRange(); emptyRange.setStart(emptyRow, 0);
      emptyRange.setEnd(viewer.textLayer.querySelector('[data-row="4"]').querySelectorAll('.viewer-text-cell')[3].firstChild, 1);
      window.getSelection().removeAllRanges(); window.getSelection().addRange(emptyRange);
      const emptyCopied = viewer.getSelectedText();
      applyFrame({...frame(), keyframe: false, lines: [{row: 4, runs: [{s: 0, t: 'KEEP', a: 1}]}]}); await settle();
      check(viewer.getSelectedText() === emptyCopied && emptyCopied.endsWith('KEEP'), 'DOM row with no text nodes has stable end boundary');
    }
    window.getSelection().removeAllRanges(); viewer.setReadableWrap(false); await settle();
    select(row().querySelectorAll('.viewer-text-cell')[1]);
    let found = viewer.findText('find.+');
    check(found.total === 2 && found.index === 1, 'literal search first match');
    check(found.scope === 'loaded viewport', 'search scope must be explicit');
    check(viewer.getSelectedText() === '👨‍👩‍👧‍👦', 'search must not overwrite user copy selection');
    check(viewer.textLayer.querySelector('.viewer-search-mark'), 'current match needs visible highlight');
    found = viewer.findText('find.+'); check(found.index === 2, 'next match');
    found = viewer.findText('find.+', -1); check(found.index === 1, 'previous match');
    applyFrame({...frame(), keyframe: false, lines: [last]}); await settle();
    check(searches.at(-1).index === 1 && searches.at(-1).total === 2, 'delta must preserve current search match');
    viewer.clearSearch(); check(!viewer.textLayer.querySelector('.viewer-search-mark'), 'clear search highlight');
    const noMatch = viewer.findText('outside-loaded-history');
    check(noMatch.total === 0 && noMatch.index === 0, 'missing search text must report no match');
    viewer.clearSearch(); window.getSelection().removeAllRanges();
    viewer.wrap.scrollTop = 90; viewer.wrap.scrollLeft = 140;
    viewer.wrap.dispatchEvent(new Event('scroll'));
    viewer.setReadableWrap(true); await settle();
    check(viewer.settings().readableWrap === true, 'reading option must apply');
    check(getComputedStyle(viewer.canvas).display === 'none', 'wrapped reading must hide coordinate cursor/canvas');
    check(!viewer.followingLive, 'entering reading mode must pause following');
    check(viewer.wrap.scrollWidth <= viewer.wrap.clientWidth + 1, 'wrapped reading must fit local width');
    check(row().getBoundingClientRect().height > metrics.cellHeight, 'long text must wrap into readable rows');
    check(getComputedStyle(row().querySelector('.viewer-text-cell')).fontWeight === '700', 'reading ANSI bold');
    const longLine = {row: 1, runs: [{s: 0, t: 'long readable text '.repeat(4)}]};
    applyFrame({...frame(), keyframe: false, lines: [longLine]}); await settle();
    const readingRange = document.createRange();
    readingRange.setStart(row().querySelectorAll('.viewer-text-cell')[1].firstChild, 0);
    readingRange.setEnd(viewer.textLayer.querySelector('[data-row="1"]').querySelectorAll('.viewer-text-cell')[3].firstChild, 1);
    window.getSelection().removeAllRanges(); window.getSelection().addRange(readingRange);
    document.dispatchEvent(new Event('selectionchange'));
    const readingSelected = viewer.getSelectedText();
    applyFrame({...frame(), keyframe: false, lines: [{...longLine, runs: longLine.runs.map(run => ({...run, a: 1}))}]}); await settle();
    check(viewer.getSelectedText() === readingSelected, 'wrapped multiline selection survives ANSI update');
    window.getSelection().removeAllRanges();
    viewer.wrap.scrollTop = 50; viewer.wrap.dispatchEvent(new Event('scroll'));
    const readingTop = viewer.wrap.scrollTop;
    select(row().querySelectorAll('.viewer-text-cell')[1]);
    applyFrame({...frame(), keyframe: false, lines: [{row: 19, runs: [{s: 0, t: 'new output'}]}]}); await settle();
    check(viewer.getSelectedText() === '👨‍👩‍👧‍👦', 'wrapped selection must survive new output');
    near(viewer.wrap.scrollTop, readingTop, 'reading position must survive new output');
    const restored = createViewer().settings();
    check(restored.readableWrap === true, 'reading preference must restore');
    viewer.setReadableWrap(false); await settle();
    check(getComputedStyle(viewer.canvas).display !== 'none', 'grid cursor canvas restored');
    near(viewer.getCellMetrics().fontSize, 15, 'grid fixed font restored');
    near(viewer.wrap.scrollTop, 90, 'mode switch restores grid vertical position');
    near(viewer.wrap.scrollLeft, 140, 'mode switch restores grid horizontal position');
    viewer.setReadableWrap(true); await settle();
    near(viewer.wrap.scrollTop, readingTop, 'mode switch restores reading vertical position');
    viewer.setReadableWrap(false); await settle();
    const changedCell = row().querySelectorAll('.viewer-text-cell')[1]; select(changedCell);
    applyFrame({...frame(), keyframe: false, lines: [{row: 0, runs: [{s: 0, t: 'changed'}]}]}); await settle();
    check(viewer.getSelectedText() === '', 'changed selected output must not silently become other copied text');
    applyFrame(frame()); await settle();
    window.getSelection().removeAllRanges();
    const wheel = deltaY => {
      const event = new WheelEvent('wheel', {deltaY, bubbles: true, cancelable: true});
      viewer.textLayer.dispatchEvent(event); return event;
    };
    viewer.wrap.scrollTop = 100; viewer.wrap.dispatchEvent(new Event('scroll'));
    const before = pans.length;
    check(!wheel(-10).defaultPrevented && pans.length === before, 'interior wheel must scroll locally');
    viewer.wrap.scrollTop = 0;
    check(wheel(-42).defaultPrevented && pans.at(-1) === 2, 'top edge wheel requests older connection history');
    viewer.wrap.scrollTop = viewer.wrap.scrollHeight;
    check(wheel(21).defaultPrevented && pans.at(-1) === -1, 'bottom edge wheel requests newer connection history');
    const count = pans.length;
    const pinch = new WheelEvent('wheel', {deltaY: -20, ctrlKey: true, bubbles: true, cancelable: true});
    viewer.textLayer.dispatchEvent(pinch);
    check(!pinch.defaultPrevented && pans.length === count, 'pinch wheel remains native');
    const touch = (type, y, x = 0, touchCount = 1) => {
      const event = new Event(type, {bubbles: true, cancelable: true});
      Object.defineProperty(event, 'touches', {value: Array.from({length: touchCount}, () => ({clientY: y, clientX: x}))});
      viewer.textLayer.dispatchEvent(event); return event;
    };
    viewer.wrap.scrollTop = 0; touch('touchstart', 100);
    check(touch('touchmove', 142).defaultPrevented && pans.at(-1) === 2, 'top edge touch history');
    touch('touchend', 142);
    for (const top of [true, false]) {
      viewer.wrap.scrollTop = top ? 0 : viewer.wrap.scrollHeight;
      const panCount = pans.length; touch('touchstart', 100, 0);
      check(!touch('touchmove', top ? 103 : 97, 60).defaultPrevented, 'horizontal touch drift remains native');
      check(!touch('touchmove', top ? 145 : 55, 75).defaultPrevented && pans.length === panCount, 'horizontal direction stays locked without remote history');
      touch('touchend', 145, 75);
    }
    viewer.wrap.scrollTop = 0; touch('touchstart', 100, 0);
    const panCount = pans.length;
    check(!touch('touchmove', 103).defaultPrevented && !touch('touchmove', 106).defaultPrevented
      && pans.length === panCount, 'small vertical drift waits for gesture threshold');
    check(touch('touchmove', 110).defaultPrevented && Math.abs(pans.at(-1) - 10 / metrics.cellHeight) < 0.01,
      'vertical threshold accumulates initial distance');
    touch('touchmove', 112, 0, 2);
    check(!touch('touchmove', 150, 0, 2).defaultPrevented && pans.length === panCount + 1, 'multi-touch pinch remains native');
    touch('touchend', 150, 0, 0);
    const beforeHorizontal = pans.length;
    const horizontal = new WheelEvent('wheel', {deltaX: 60, deltaY: -10, bubbles: true, cancelable: true});
    viewer.textLayer.dispatchEvent(horizontal);
    check(!horizontal.defaultPrevented && pans.length === beforeHorizontal, 'horizontal movement must not request history');
    viewer.setReadableWrap(true); await settle(); viewer.wrap.scrollTop = 0;
    check(wheel(-21).defaultPrevented && pans.at(-1) === 1, 'reading mode top edge also requests history');
    viewer.setReadableWrap(false); await settle();
    viewer.wrap.scrollTop = 60; viewer.wrap.scrollLeft = 70;
    viewer.openViewer('reading-B', 'B'); applyFrame(frame('reading-B')); await settle();
    viewer.openViewer('reading-A', 'A'); applyFrame(frame('reading-A')); await settle();
    near(viewer.wrap.scrollTop, 60, 'session local history position restored');
    near(viewer.wrap.scrollLeft, 70, 'session horizontal position restored');
    check(!viewer.followingLive, 'session reading state retained');
    check(viewer.getSelectedText() === '', 'session transition clears stale selection');
    for (const readable of [false, true]) {
      viewer.setReadableWrap(readable); await settle();
      const target = row().querySelectorAll('.viewer-text-cell')[1]; select(target);
      viewer.findText('find.+');
      viewer.setPrivacyCurtain(true);
      check(viewer.textLayer.inert && viewer.textLayer.getAttribute('aria-hidden') === 'true', 'privacy curtain hides DOM from interaction and accessibility');
      check(viewer.getSelectedText() === '', 'protected text must not remain copy-facing');
      check(viewer.findText('find.+').total === 0, 'protected text must not remain searchable');
      select(target);
      const protectedClipboard = new DataTransfer();
      const protectedCopy = new ClipboardEvent('copy', {clipboardData: protectedClipboard, cancelable: true});
      document.dispatchEvent(protectedCopy);
      check(protectedCopy.defaultPrevented && protectedClipboard.getData('text/plain') === '', 'protected DOM selection cannot copy output');
      viewer.setPrivacyCurtain(false);
      check(!viewer.textLayer.inert && !viewer.textLayer.hasAttribute('aria-hidden'), 'unprotect restores text accessibility');
      select(target);
      check(viewer.getSelectedText() === '👨‍👩‍👧‍👦', 'unprotect restores selected text normally');
      check(viewer.findText('find.+').total === 2, 'unprotect restores loaded viewport search');
      window.getSelection().removeAllRanges(); viewer.clearSearch();
    }
    check(sent.every(msg => ['watch', 'unwatch', 'request_keyframe'].includes(msg.type)), 'reading core remains read-only');
    viewer.finishCloseViewer(); finish('ok', 'VIEWER_READING_OK:' + checks);
  } catch (error) { finish('error', 'VIEWER_READING_ERROR: ' + (error.stack || error)); }
})();
