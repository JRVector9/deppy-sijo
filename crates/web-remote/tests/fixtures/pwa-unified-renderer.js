// Bounded source windows fill the pane and join without moving the source row being read.
(async () => {
  let checks = 0;
  const check = (condition, message) => { if (!condition) throw Error(message); checks++; };
  const near = (actual, expected, message) => check(Math.abs(actual - expected) < 2, `${message}: ${actual} != ${expected}`);
  const settle = () => new Promise(done => requestAnimationFrame(() => requestAnimationFrame(done)));
  const finish = (status, text) => { document.body.dataset.status = status; document.body.textContent = text; };
  try {
    localStorage.removeItem('deppy-viewer:prefs');
    const sent = [], layouts = [], navigations = [], reading = [], searches = [];
    let request = 20, autoNavigate = false;
    const viewer = createViewer({ send: msg => sent.push(msg), hooks: {
      historyRowsChanged: (rows, metrics) => layouts.push({ rows, metrics }),
      historyNavigate: value => {
        navigations.push(value);
        if (autoNavigate) viewer.expectHistoryWindow(++request, { ...value, preservePosition: true });
      },
      readingChanged: state => reading.push(state),
      searchChanged: state => searches.push(state),
      resumeHistory: () => viewer.expectHistoryWindow(++request, { reason: 'reset', rows: viewer.getDesiredHistoryRows() }),
    } });
    check(layouts.length === 0, 'row hook must not run synchronously before the host owns viewer');
    viewer.wrap.style.width = '390px'; viewer.wrap.style.height = '600px';
    viewer.setViewerConnection('connected'); viewer.openViewer('unified-A', 'A');
    const frame = (first, rows, options = {}) => {
      const end = options.end ?? 1000;
      return { session: options.session ?? viewer.watching, cols: options.cols ?? 80, rows,
        keyframe: options.keyframe !== false, alt: !!options.alt, offset: end - first - rows,
        lines: options.lines ?? Array.from({ length: rows }, (_, row) => ({ row, runs: [{ s: 0, t: `line:${first + row}` }] })),
        cursor: { row: end - first - 1, col: 0, visible: end - first <= rows },
        history: { generation: options.generation ?? '1', first_line: String(first), total_lines: options.total ?? end,
          native_rows: options.nativeRows ?? 8, live_first_line: String(end - (options.nativeRows ?? 8)),
          request: options.request, expired: !!options.expired } };
    };
    const applyWindow = async (first, rows, options = {}) => {
      const id = ++request;
      viewer.expectHistoryWindow(id, { preservePosition: true, ...options });
      viewer.handleViewport(frame(first, rows, { ...options, request: id })); await settle();
    };
    const row = identity => viewer.textLayer.querySelector(`[data-identity="${identity}"]`);
    const visible = () => {
      const top = viewer.wrap.scrollTop;
      const node = [...viewer.textLayer.querySelectorAll('.viewer-text-row')]
        .find(node => node.offsetTop + node.offsetHeight > top);
      return node && { identity: node.dataset.identity, pixel: node.offsetTop - top };
    };
    viewer.handleViewport(frame(992, 8)); await settle();
    const desired = viewer.getDesiredHistoryRows();
    check(desired > 8 && desired >= Math.ceil(600 / viewer.getCellMetrics().cellHeight), 'native eight rows must request enough actual rows to fill tall pane');
    check(layouts.at(-1).rows === desired && layouts.at(-1).metrics.nativeRows === 8, 'layout separates native rows from display rows');
    check(viewer.scrollNote.parentNode === viewer.wrap.parentNode, 'bottom action belongs inside terminal stage');
    check(!viewer.el.classList.contains('viewer-has-scroll-note'), 'no reserved rail class');
    check(viewer.resumeFollowButton.textContent === '↓ 맨 아래로', 'approved bottom action label');
    check(getComputedStyle(viewer.scrollNote).position === 'absolute', 'bottom action must not consume grid height');
    await applyWindow(1000 - desired, desired, { follow: true, reason: 'layout' });
    check(viewer.screen.rows === desired && viewer.screen.rows * viewer.getCellMetrics().cellHeight >= 600,
      'expanded source rows fill the pane without stretching cell height');
    check(viewer.screen.history.native_rows === 8 && viewer.screen.cursor.row === desired - 1,
      'native cursor projects into wider source window');
    check(viewer.followingLive && viewer.scrollNote.hidden, 'latest tail follows with no overlay');
    const first = 1000 - desired;
    viewer.openViewer('unified-cursor', 'cursor above tail');
    viewer.handleViewport({ ...frame(first, desired), cursor: { row: desired - 8, col: 0, visible: true } }); await settle();
    check(viewer.followingLive && viewer.screen.offset === 0
      && viewer.wrap.scrollTop < viewer.wrap.scrollHeight - viewer.wrap.clientHeight - 2,
      'latest native cursor can remain visible above blank native tail rows');
    check(viewer.scrollNote.hidden && !viewer.getHistoryState().offLatest,
      'latest cursor following must not expose a past-output action');
    viewer.openViewer('unified-A', 'A'); viewer.handleViewport(frame(first, desired)); await settle();
    const wheelLayout = ++request;
    viewer.expectHistoryWindow(wheelLayout, { reason: 'layout', rows: desired, preservePosition: true, follow: true });
    viewer.wrap.scrollTop = 110;
    viewer.wrap.dispatchEvent(new WheelEvent('wheel', { deltaY: -21, bubbles: true }));
    const wheelAnchor = visible();
    check(!viewer.followingLive && viewer.getHistoryState().pendingRequest === wheelLayout,
      'wheel pauses live intent while keeping useful layout query correlated');
    viewer.handleViewport(frame(first, desired, { request: wheelLayout })); await settle();
    check(!viewer.followingLive && visible().identity === wheelAnchor.identity,
      'late layout response cannot resume following or replace wheel reading anchor');
    near(visible().pixel, wheelAnchor.pixel, 'late layout response preserves wheel anchor pixel');
    viewer.resumeFollow(); viewer.handleViewport(frame(first, desired, { request: viewer.getHistoryState().pendingRequest })); await settle();
    const selectionLayout = ++request;
    viewer.expectHistoryWindow(selectionLayout, { reason: 'layout', rows: desired, preservePosition: true, follow: true });
    const pendingOwner = row(first + 10).firstElementChild, pendingRange = document.createRange();
    pendingRange.selectNodeContents(pendingOwner); window.getSelection().removeAllRanges(); window.getSelection().addRange(pendingRange);
    document.dispatchEvent(new Event('selectionchange'));
    const pendingCopy = viewer.getSelectedText(), selectionAnchor = visible();
    check(!viewer.followingLive && viewer.getHistoryState().pendingRequest === selectionLayout,
      'selection pauses live intent while keeping layout request identity');
    viewer.handleViewport(frame(first, desired, { request: selectionLayout })); await settle();
    check(!viewer.followingLive && viewer.getSelectedText() === pendingCopy
      && row(first + 10).firstElementChild === pendingOwner && visible().identity === selectionAnchor.identity,
      'late layout response preserves selected DOM owner and source reading anchor');
    window.getSelection().removeAllRanges(); viewer.resumeFollow();
    viewer.handleViewport(frame(first, desired, { request: viewer.getHistoryState().pendingRequest })); await settle();
    viewer.openViewer('unified-disjoint', 'distant live output');
    viewer.handleViewport(frame(200, desired, { end: 200 + desired, generation: '5' })); await settle();
    const distantOwner = row(210).firstElementChild, distantRange = document.createRange();
    distantRange.selectNodeContents(distantOwner); window.getSelection().removeAllRanges(); window.getSelection().addRange(distantRange);
    document.dispatchEvent(new Event('selectionchange')); viewer.findText('line:210'); await settle();
    const retainedWindow = viewer.screen, distantAnchor = visible(), distantCopy = viewer.getSelectedText();
    viewer.handleViewport(frame(800, desired, { end: 800 + desired, generation: '5' })); await settle();
    check(viewer.screen === retainedWindow && viewer.getSelectedText() === distantCopy
      && row(210).firstElementChild === distantOwner, 'disjoint passive live range cannot replace retained source or selection');
    check(visible().identity === distantAnchor.identity && !viewer.getHistoryState().expired,
      'disjoint still-available history keeps reading anchor without false expiry');
    near(visible().pixel, distantAnchor.pixel, 'disjoint passive range preserves reading anchor pixel');
    viewer.handleViewport(frame(1200, desired, { end: 1200 + desired, total: 100, generation: '5' })); await settle();
    check(viewer.screen === retainedWindow && viewer.getHistoryState().expired && viewer.getSelectedText() === distantCopy,
      'disjoint ring trim reports expiry while preserving locally selected source');
    window.getSelection().removeAllRanges(); viewer.clearSearch();
    viewer.openViewer('unified-layout-disjoint', 'paused distant layout response');
    viewer.handleViewport(frame(200, desired, { end: 200 + desired, generation: '5' })); await settle();
    const lateLayout = ++request;
    viewer.expectHistoryWindow(lateLayout, { reason: 'layout', rows: desired, preservePosition: true, follow: true });
    const pausedOwner = row(210).firstElementChild, pausedRange = document.createRange();
    pausedRange.selectNodeContents(pausedOwner); window.getSelection().removeAllRanges(); window.getSelection().addRange(pausedRange);
    document.dispatchEvent(new Event('selectionchange')); viewer.findText('line:210'); await settle();
    const pausedWindow = viewer.screen, pausedAnchor = visible(), pausedCopy = viewer.getSelectedText();
    viewer.handleViewport(frame(800, desired, { end: 800 + desired, generation: '5', request: lateLayout })); await settle();
    check(viewer.screen === pausedWindow && !viewer.followingLive && viewer.getHistoryState().pendingRequest === null,
      'paused matching latest-layout disjoint response settles request without replacing source');
    check(viewer.getSelectedText() === pausedCopy && row(210).firstElementChild === pausedOwner
      && visible().identity === pausedAnchor.identity, 'paused distant layout response keeps selection and source anchor');
    viewer.resumeFollow();
    viewer.handleViewport(frame(800, desired, { end: 800 + desired, generation: '5', request: viewer.getHistoryState().pendingRequest })); await settle();
    check(viewer.followingLive && viewer.screen.history.first_line === '800', 'explicit bottom reset still installs a disjoint latest range');
    viewer.openViewer('unified-rewatch', 'reconnect before initial fill');
    viewer.handleViewport(frame(992, 8)); await settle();
    const beforeRewatch = layouts.length, heldFill = ++request;
    viewer.expectHistoryWindow(heldFill, { reason: 'layout', rows: desired, preservePosition: true, follow: true });
    viewer.setViewerConnection('reconnecting'); viewer.setViewerConnection('connected'); viewer.rewatch();
    viewer.handleViewport(frame(992, 8)); await settle();
    check(layouts.length === beforeRewatch + 1 && layouts.at(-1).rows === desired,
      'unchanged geometry rewatch notifies fresh desired rows after interrupted initial fill');
    check(viewer.getHistoryState().pendingRequest === null && sent.at(-1).type === 'watch',
      'rewatch retires held request and establishes a fresh read-only watch');
    await applyWindow(1000 - desired, desired, { reason: 'layout', follow: true });
    check(viewer.screen.rows * viewer.getCellMetrics().cellHeight >= viewer.wrap.clientHeight,
      'rewatch fresh correlated fill restores whole-pane actual source rows');
    viewer.openViewer('unified-A', 'A'); viewer.handleViewport(frame(first, desired)); await settle();
    viewer.wrap.scrollTop = 100; viewer.wrap.dispatchEvent(new Event('scroll')); await settle();
    check(!viewer.followingLive && !viewer.scrollNote.hidden, 'off-tail local scroll exposes bottom action');
    const selectedRow = row(first + 10), selectedCell = selectedRow.firstElementChild;
    const selection = window.getSelection(), range = document.createRange();
    range.selectNodeContents(selectedCell); selection.removeAllRanges(); selection.addRange(range);
    document.dispatchEvent(new Event('selectionchange'));
    viewer.findText(`line:${first + 10}`); await settle();
    const before = visible(), copy = viewer.getSelectedText(), search = searches.at(-1);
    const extra = Math.floor(desired / 2);
    await applyWindow(first - extra, desired, { reason: 'scroll' });
    const after = visible();
    check(viewer.screen.rows === desired + extra && viewer.getHistoryState().firstLine === String(first - extra), 'overlapping windows prepend into contiguous cache');
    check(row(first + 10) === selectedRow && row(first + 10).firstElementChild === selectedCell, 'prepend keeps selected source DOM owner');
    check(viewer.getSelectedText() === copy && copy, 'prepend preserves selected Unicode owner text');
    check(after.identity === before.identity, 'prepend keeps first visible identity');
    near(after.pixel, before.pixel, 'prepend keeps row at same screen pixel');
    check(searches.at(-1).query === search.query && searches.at(-1).total === 1, 'prepend preserves loaded-row search scope');
    selection.removeAllRanges(); viewer.clearSearch();
    viewer.wrap.scrollTop = 0; viewer.wrap.dispatchEvent(new Event('scroll')); await settle();
    check(navigations.some(value => value.delta > 0 && value.rows === desired), 'scrolling near cache head preloads older overlapping source window');
    const retryCount = navigations.length, held = ++request;
    viewer.expectHistoryWindow(held, { preservePosition: true });
    viewer.wrap.dispatchEvent(new Event('scroll'));
    check(navigations.length === retryCount, 'pending navigation suppresses overlapping edge requests');
    viewer.handleViewport(frame(Number(viewer.screen.history.first_line), desired, { request: held })); await settle();
    check(navigations.length > retryCount, 'completed contained response rechecks edge even without changed scroll geometry');
    autoNavigate = true;
    await applyWindow(first - extra - 10, desired, { reason: 'scroll' });
    autoNavigate = false;
    if (viewer.getHistoryState().pendingRequest !== null) viewer.cancelHistoryWindow(viewer.getHistoryState().pendingRequest);
    for (let pass = 0; pass < 12; pass++) {
      const cacheFirst = Number(viewer.screen.history.first_line);
      const anchor = row(cacheFirst + 2);
      viewer.wrap.scrollTop = anchor.offsetTop + 5; viewer.wrap.dispatchEvent(new Event('scroll'));
      const pinned = visible();
      await applyWindow(cacheFirst - 40, 96, { reason: 'scroll' });
      const current = visible();
      check(current.identity === pinned.identity, 'bounded cache trimming preserves source anchor');
      near(current.pixel, pinned.pixel, 'bounded cache trimming preserves anchor pixel');
      check(viewer.screen.rows <= 512 && viewer.screen.rows * viewer.screen.cols <= 65536, 'row and cell cache caps');
    }
    check(viewer.screen.rows === 512, 'cache reaches and stays at actual row cap');
    check(viewer.canvas.width * viewer.canvas.height <= 8 * 1024 * 1024, 'expanded cache obeys backing canvas pixel cap');
    const fontAnchor = visible(); viewer.setFontSize(20); await settle();
    check(visible().identity === fontAnchor.identity, 'font change preserves source reading row');
    const keyboardAnchor = visible(); viewer.wrap.style.height = '290px'; viewer.scheduleViewerRender(); await settle();
    check(visible().identity === keyboardAnchor.identity, 'keyboard/controls height change preserves source reading row');
    check(viewer.getDesiredHistoryRows() < desired && layouts.at(-1).rows === viewer.getDesiredHistoryRows(), 'keyboard changes bounded row request rather than native PTY');
    const wrapAnchor = visible(); viewer.setReadableWrap(true); await settle();
    check(visible().identity === wrapAnchor.identity, 'reading presentation starts at same source row');
    check(getComputedStyle(viewer.canvas).display === 'none', 'reading presentation still hides coordinate cursor');
    const readingAnchor = visible(); viewer.wrap.style.width = '200px'; viewer.scheduleViewerRender(); await settle();
    check(visible().identity === readingAnchor.identity, 'reading reflow keeps source row');
    viewer.setReadableWrap(false); await settle();
    check(visible().identity === wrapAnchor.identity, 'returning grid restores independent grid source anchor');
    const retained = viewer.screen, resync = sent.filter(msg => msg.type === 'request_keyframe').length;
    viewer.handleViewport(frame(992, 8, { keyframe: false, lines: [{ row: 0, runs: [{ s: 0, t: 'NATIVE_DELTA' }] }] })); await settle();
    check(viewer.screen === retained && !viewer.textLayer.textContent.includes('NATIVE_DELTA'), 'native delta cannot use widened cache baseline');
    check(sent.filter(msg => msg.type === 'request_keyframe').length === resync + 1, 'baseline mismatch requests one keyframe');
    const sourceFirst = Number(retained.history.first_line);
    const id = ++request; viewer.expectHistoryWindow(id, { preservePosition: true });
    viewer.handleViewport(frame(sourceFirst, 96, { request: id - 1 }));
    check(viewer.screen === retained && viewer.getHistoryState().pendingRequest === id, 'late request cannot satisfy current navigation or change loaded source');
    viewer.handleViewport(frame(sourceFirst, 96, { request: id })); await settle();
    const readBeforeGeneration = viewer.screen;
    viewer.handleViewport(frame(0, 8, { generation: '2', end: 8 })); await settle();
    check(viewer.screen === readBeforeGeneration && viewer.getHistoryState().expired, 'generation reset retains loaded source and reports expiry');
    check(viewer.getHistoryState().historyMessage.includes('만료'), 'nonblocking settings history state explains expiry');
    viewer.resumeFollow(); const reset = viewer.getHistoryState().pendingRequest;
    viewer.handleViewport(frame(0, 8, { generation: '2', end: 8, request: reset })); await settle();
    check(viewer.followingLive && !viewer.getHistoryState().expired && viewer.screen.history.generation === '2', 'explicit bottom action resumes current generation');
    check(viewer.screen.rows === 8 && viewer.scrollNote.hidden, 'insufficient stored history stays actual rows without fake padding');
    viewer.findText('line:7'); await settle();
    check(viewer.scrollNote.hidden, 'pausing a latest short pane does not show a past-output action');
    viewer.handleViewport(frame(0, 8, { generation: '2', end: 8, keyframe: false,
      lines: [{ row: 7, runs: [{ s: 0, t: 'editable tail advanced' }] }] })); await settle();
    check(!viewer.scrollNote.hidden && row(7).textContent.includes('line:7'),
      'new output inside same native row identity exposes resume while preserving retained content');
    viewer.resumeFollow();
    viewer.handleViewport(frame(0, 8, { generation: '2', end: 8, request: viewer.getHistoryState().pendingRequest })); await settle();
    viewer.openViewer('unified-growth', 'native rows become history');
    const originalLines = [{ row: 0, runs: [{ s: 0, t: 'KEEP' }] }, { row: 1, runs: [{ s: 0, t: 'old' }] },
      { row: 2, runs: [] }, { row: 3, runs: [] }];
    viewer.handleViewport(frame(0, 4, { end: 4, nativeRows: 2, lines: originalLines })); await settle();
    const keptOwner = row(0).firstElementChild, keptRange = document.createRange();
    keptRange.selectNodeContents(keptOwner); selection.removeAllRanges(); selection.addRange(keptRange);
    document.dispatchEvent(new Event('selectionchange')); viewer.findText('KEEP');
    const written = [
      { row: 0, runs: [{ s: 0, t: 'old' }] }, { row: 1, runs: [{ s: 0, t: 'WRITTEN' }] },
      { row: 2, runs: [{ s: 0, t: 'next' }] }, { row: 3, runs: [{ s: 0, t: 'last' }] },
    ];
    viewer.handleViewport(frame(1, 4, { end: 5, nativeRows: 2, lines: written })); await settle();
    check(viewer.screen.lines[2].length === 0 && viewer.getSelectedText() === 'K',
      'passive output keeps pinned source snapshot even when native rows acquire text');
    const freshWindow = [originalLines[0], originalLines[1], { row: 2, runs: [{ s: 0, t: 'WRITTEN' }] },
      { row: 3, runs: [{ s: 0, t: 'next' }] }];
    await applyWindow(0, 4, { end: 5, nativeRows: 2, reason: 'layout', lines: freshWindow });
    check(viewer.screen.lines[2].length === 0 && viewer.getSelectedText() === 'K', 'layout requery preserves pinned content and selection');
    const growthAnchor = visible();
    await applyWindow(0, 4, { end: 5, nativeRows: 2, reason: 'scroll', lines: freshWindow });
    check(viewer.screen.lines[2][0].t === 'WRITTEN' && row(2).textContent.includes('WRITTEN'),
      'explicit overlapping history navigation refreshes former editable native rows');
    check(viewer.getSelectedText() === 'K' && row(0).firstElementChild === keptOwner
      && visible().identity === growthAnchor.identity, 'fresh manual history keeps unchanged selected owner and source anchor');
    selection.removeAllRanges(); viewer.clearSearch(); viewer.openViewer('unified-A', 'A'); await settle();
    await applyWindow(936, 64, { follow: true, reason: 'layout', generation: '3' });
    viewer.wrap.style.height = '290px'; viewer.wrap.scrollTop = 120; viewer.wrap.dispatchEvent(new Event('scroll')); await settle();
    const sessionAnchor = visible();
    viewer.openViewer('unified-B', 'B'); viewer.handleViewport(frame(992, 8)); await settle();
    viewer.openViewer('unified-A', 'A'); await settle();
    check(visible().identity === sessionAnchor.identity && !viewer.followingLive, 'session switch restores bounded cached source and local reading anchor');
    viewer.handleViewport(frame(940, 60, { generation: '3' })); await settle();
    check(visible().identity === sessionAnchor.identity, 'rewatch passive overlap does not replace session reading source');
    viewer.resumeFollow(); const altReset = viewer.getHistoryState().pendingRequest;
    viewer.handleViewport(frame(992, 8, { generation: '4', alt: true, request: altReset })); await settle();
    check(viewer.screen.alt && viewer.screen.rows === 8, 'alternate screen remains native-only and never joins primary cache');
    viewer.wrap.style.height = '100000px'; viewer.scheduleViewerRender(); await settle();
    check(viewer.getDesiredHistoryRows() <= 256, 'oversized layout still uses bounded requested rows');
    viewer.openViewer('unified-wide', 'wide');
    viewer.handleViewport(frame(872, 128, { cols: 512 })); await settle();
    check(viewer.getDesiredHistoryRows() <= 128, 'request rows honor cell cap on wide terminals');
    check(viewer.canvas.width * viewer.canvas.height <= 8 * 1024 * 1024, 'wide high-DPR frame obeys exact canvas pixel cap');
    for (const [width, height] of [[320, 568], [390, 844], [844, 390]]) {
      viewer.el.style.width = width + 'px'; viewer.el.style.height = height + 'px';
      viewer.wrap.style.width = '100%'; viewer.wrap.style.height = '100%';
      viewer.openViewer(`unified-${width}`, 'viewport'); viewer.handleViewport(frame(992, 8)); await settle();
      const rows = viewer.getDesiredHistoryRows();
      await applyWindow(1000 - rows, rows, { follow: true, reason: 'layout' });
      const header = viewer.el.querySelector('.viewer-header').getBoundingClientRect();
      near(viewer.wrap.clientHeight + header.height, height, 'header and terminal fill available height without history rail');
      check(viewer.screen.rows * viewer.getCellMetrics().cellHeight >= viewer.wrap.clientHeight,
        `${width}x${height} displays actual output throughout pane`);
      check(viewer.scrollNote.hidden && rows <= 256 && rows * viewer.screen.cols <= 65536,
        `${width}x${height} latest state stays bounded and has no floating action`);
    }
    check(sent.every(msg => ['watch', 'unwatch', 'request_keyframe'].includes(msg.type)), 'renderer sends only read-only lifecycle messages');
    viewer.finishCloseViewer();
    finish('ok', 'PWA_UNIFIED_RENDERER_OK:' + checks);
  } catch (error) { finish('error', 'PWA_UNIFIED_RENDERER_ERROR: ' + (error.stack || error)); }
})();
