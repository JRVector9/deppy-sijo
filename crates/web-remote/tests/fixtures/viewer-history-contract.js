// Immutable displayed windows continue receiving a complete, hidden wire baseline.
(async () => {
  let checks = 0;
  const check = (condition, message) => { if (!condition) throw Error(message); checks++; };
  const settle = () => new Promise(done => requestAnimationFrame(() => requestAnimationFrame(done)));
  const finish = (status, text) => { document.body.dataset.status = status; document.body.textContent = text; };
  try {
    localStorage.removeItem('deppy-viewer:prefs');
    const sent = [], hostRequests = [], states = [], searchStates = [];
    let request = 100;
    const viewer = createViewer({send: msg => sent.push(msg), hooks: {
      readingChanged: state => states.push(state),
      searchChanged: state => searchStates.push(state),
      resumeHistory: () => {
        const reset = {type: 'scroll', session: viewer.watching, delta: 0, request: ++request, reset: true};
        hostRequests.push(reset); viewer.expectHistoryWindow(reset.request);
      },
    }});
    check(typeof viewer.getHistoryState === 'function', 'retained history state API missing');
    viewer.wrap.style.width = '100px'; viewer.wrap.style.height = '42px';
    viewer.setViewerConnection('connected'); viewer.openViewer('history-A', 'A');
    const frame = (text, {first = '100', total = 10, offset = 0, generation = '1', request, expired = false,
      keyframe = true, row = 0, session = viewer.watching} = {}) => ({
      session, keyframe, cols: 12, rows: 3, offset,
      lines: keyframe ? [{row: 0, runs: [{s: 0, t: text}]}, {row: 1, runs: [{s: 0, t: 'beta'}]},
        {row: 2, runs: [{s: 0, t: 'latest'}]}] : [{row, runs: [{s: 0, t: text}]}],
      cursor: {visible: true, row: 2, col: 11},
      history: {generation, first_line: first, total_lines: total, request, expired},
    });
    const rowText = row => viewer.screen.lines[row].map(run => run.t).join('');
    const selectFirst = () => {
      const row = viewer.textLayer.querySelector('[data-row="0"]');
      const range = document.createRange(); range.selectNodeContents(row.querySelector('.viewer-text-cell'));
      const selection = window.getSelection(); selection.removeAllRanges(); selection.addRange(range);
      document.dispatchEvent(new Event('selectionchange'));
    };
    viewer.handleViewport(frame('KEEP')); await settle();
    const published = viewer.screen;
    selectFirst();
    check(viewer.getHistoryState().retained && !viewer.followingLive, 'selection pins displayed live window');
    const copied = viewer.getSelectedText();
    const top = viewer.wrap.scrollTop, left = viewer.wrap.scrollLeft;
    viewer.handleViewport(frame('ADVANCE', {first: '101', total: 11})); await settle();
    viewer.handleViewport(frame('next2', {first: '101', total: 11, keyframe: false, row: 2})); await settle();
    check(viewer.screen === published && rowText(0) === 'KEEP', 'advancing keyframes must not replace paused source rows');
    check(viewer.getSelectedText() === copied, 'advancing output preserves copied text');
    check(viewer.wrap.scrollTop === top && viewer.wrap.scrollLeft === left, 'advancing output preserves local position');
    check(!viewer.textLayer.textContent.includes('ADVANCE'), 'hidden live baseline must not enter selectable DOM');
    check(viewer.findText('KEEP').total === 1 && viewer.findText('ADVANCE').total === 0, 'search only uses displayed loaded window');
    viewer.resumeFollow();
    const firstReset = hostRequests.at(-1);
    check(firstReset && viewer.getHistoryState().pendingRequest === firstReset.request && rowText(0) === 'KEEP',
      'paused offset-zero display requests absolute reset even passive baseline is already live');
    check(!viewer.scrollNote.hidden && viewer.resumeFollowButton.disabled && viewer.offsetText.textContent.includes('요청'),
      'pending offset-zero reset keeps visible progress and disables duplicate fallback action');
    viewer.resumeFollow(); viewer.resumeFollow();
    check(hostRequests.length === 1 && viewer.getHistoryState().pendingRequest === firstReset.request,
      'repeated resume while reset is pending sends no duplicate host request');
    viewer.handleViewport(frame('ADVANCE', {first: '101', total: 11, request: firstReset.request}));
    viewer.handleViewport(frame('next2', {first: '101', total: 11, keyframe: false, row: 2})); await settle();
    check(rowText(0) === 'ADVANCE' && rowText(2) === 'next2', 'resume publishes complete keyframe plus later delta');
    check(published.lines[0][0].t === 'KEEP' && published.lines[2][0].t === 'latest', 'published snapshot must remain immutable');
    check(!viewer.getHistoryState().retained && viewer.followingLive, 'live resume ends retention');
    check(viewer.getSelectedText() === '', 'resume clears stale selection');
    const liveBeforeStale = viewer.screen;
    const resyncBefore = sent.filter(msg => msg.type === 'request_keyframe').length;
    viewer.handleViewport(frame('STALE_HISTORY', {first: '91', offset: 10, total: 21, request: 9}));
    check(viewer.screen === liveBeforeStale, 'late unmatched historical keyframe is not directly displayed');
    viewer.handleViewport(frame('live-delta1', {first: '101', total: 11, keyframe: false, row: 1})); await settle();
    check(viewer.screen === liveBeforeStale && rowText(0) === 'ADVANCE' && rowText(1) === 'beta',
      'stale historical keyframe cannot contaminate later passive live delta publication');
    viewer.handleViewport(frame('live-delta2', {first: '101', total: 11, keyframe: false, row: 2}));
    viewer.handleViewport(frame('STALE_AGAIN', {first: '91', offset: 10, total: 21, request: 8}));
    check(sent.filter(msg => msg.type === 'request_keyframe').length === resyncBefore + 1,
      'baseline resync requests are deduplicated across deltas and stale replies');
    viewer.handleViewport(frame('RESYNC_NOW', {first: '102', total: 12}));
    viewer.handleViewport(frame('after-resync', {first: '102', total: 12, keyframe: false, row: 1})); await settle();
    check(rowText(0) === 'RESYNC_NOW' && rowText(1) === 'after-resync' && viewer.followingLive,
      'full passive keyframe establishes safe live baseline for later deltas');
    viewer.handleViewport(frame('STALE_AFTER_RESYNC', {first: '91', offset: 10, total: 21, request: 7}));
    check(sent.filter(msg => msg.type === 'request_keyframe').length === resyncBefore + 2,
      'valid full keyframe releases dedup guard for a later independent stale reply');
    viewer.handleViewport(frame('ADVANCE', {first: '101', total: 11})); await settle();
    const readActions = [
      ['pan', () => viewer.wrap.dispatchEvent(new WheelEvent('wheel', {deltaX: 8, bubbles: true}))],
      ['selection', selectFirst],
      ['search', () => viewer.findText('ADVANCE')],
      ['reading wrap', () => viewer.setReadableWrap(true)],
    ];
    for (const [label, action] of readActions) {
      viewer.findText('ADVANCE'); viewer.resumeFollow();
      const pendingReset = hostRequests.at(-1).request;
      const newlyPinned = viewer.screen;
      action(); await settle();
      const selected = viewer.getSelectedText();
      check(viewer.getHistoryState().pendingRequest === null && !viewer.followingLive,
        label + ' supersedes pending resume intent and request');
      viewer.handleViewport(frame('LATE_RESUME', {first: '103', total: 13, request: pendingReset})); await settle();
      check(viewer.screen === newlyPinned && viewer.getSelectedText() === selected,
        label + ' keeps newly pinned source and copy selection under its canceled late response');
      if (viewer.settings().readableWrap) viewer.setReadableWrap(false);
      viewer.handleViewport(frame('ADVANCE', {first: '101', total: 11}));
      viewer.resumeFollow();
      viewer.handleViewport(frame('ADVANCE', {first: '101', total: 11, request: hostRequests.at(-1).request})); await settle();
    }
    viewer.findText('ADVANCE'); viewer.resumeFollow();
    const expiryReset = hostRequests.at(-1).request;
    const resetsBeforeExpiry = hostRequests.length;
    viewer.handleViewport(frame('PASSIVE_EXPIRED', {first: '91', offset: 10, total: 21, expired: true})); await settle();
    check(viewer.getHistoryState().expired && viewer.getHistoryState().pendingRequest === expiryReset && rowText(0) === 'ADVANCE',
      'passive expiry reports loaded state without releasing pending matching reset');
    viewer.resumeFollow();
    check(hostRequests.length === resetsBeforeExpiry && viewer.getHistoryState().pendingRequest === expiryReset,
      'passive expiry preserves reset intent and duplicate-resume guard');
    viewer.handleViewport(frame('ADVANCE', {first: '101', total: 11, request: expiryReset})); await settle();
    check(viewer.followingLive && !viewer.getHistoryState().retained && !viewer.getHistoryState().expired,
      'matching current reset after passive expiry publishes and resumes live');
    viewer.findText('ADVANCE'); viewer.resumeFollow();
    const matchingExpiredReset = hostRequests.at(-1).request;
    viewer.handleViewport(frame('MATCHING_EXPIRED', {first: '91', offset: 10, total: 21, request: matchingExpiredReset, expired: true})); await settle();
    check(viewer.getHistoryState().pendingRequest === null && viewer.getHistoryState().expired && !viewer.followingLive
      && rowText(0) === 'ADVANCE', 'matching expired reset clears only its own intent and preserves loaded source');
    viewer.resumeFollow();
    check(hostRequests.at(-1).request !== matchingExpiredReset, 'matching expiry allows a fresh explicit retry');
    viewer.handleViewport(frame('ADVANCE', {first: '101', total: 11, request: hostRequests.at(-1).request})); await settle();
    check(!viewer.expectHistoryWindow(0) && !viewer.expectHistoryWindow(2 ** 32)
      && !viewer.expectHistoryWindow(NaN), 'history requests must be nonzero u32');
    check(viewer.expectHistoryWindow(10) && viewer.expectHistoryWindow(11), 'manual request registration');
    viewer.handleViewport(frame('OLD_REQUEST', {first: '99', offset: 2, total: 12, request: 10})); await settle();
    check(rowText(0) === 'ADVANCE' && viewer.getHistoryState().pendingRequest === 11, 'older manual response cannot replace pending window');
    viewer.handleViewport(frame('HIST_KEEP', {first: '97', offset: 5, total: 12, request: 11})); await settle();
    check(rowText(0) === 'HIST_KEEP' && viewer.getHistoryState().offset === 5, 'matching manual response explicitly installs history');
    check(viewer.getHistoryState().retained && viewer.getHistoryState().pendingRequest === null, 'history remains locally retained after navigation');
    selectFirst(); viewer.findText('HIST_KEEP'); viewer.expectHistoryWindow(12);
    viewer.handleViewport(frame('HIST_KEEP', {first: '96', offset: 6, total: 12, request: 12})); await settle();
    check(viewer.getSelectedText() === '', 'different source rows clear coincidentally identical copy selection');
    check(searchStates.at(-1).query === '' && searchStates.at(-1).total === 0,
      'different source rows invalidate old search scope despite coincidentally identical text');
    selectFirst();
    viewer.handleViewport(frame('PASSIVE', {first: '96', offset: 7, total: 13})); await settle();
    check(rowText(0) === 'HIST_KEEP' && viewer.getHistoryState().latestOffset === 7,
      'automatic server offset anchoring cannot replace displayed rows');
    check(!viewer.getHistoryState().expired, 'unchanged absolute top row stays valid as offset advances');
    viewer.resumeFollow();
    const reset = hostRequests.at(-1);
    check(reset.reset === true && reset.delta === 0 && reset.session === 'history-A', 'historical resume delegates absolute reset to host');
    check(viewer.getHistoryState().pendingRequest === reset.request && rowText(0) === 'HIST_KEEP', 'resume waits for registered reset response');
    viewer.handleViewport(frame('UNMATCHED_LIVE', {first: '104', total: 16, request: 10})); await settle();
    check(rowText(0) === 'HIST_KEEP' && viewer.getHistoryState().pendingRequest === reset.request, 'unmatched live frame cannot satisfy reset');
    viewer.handleViewport(frame('LIVE_NOW', {first: '105', total: 17, request: reset.request})); await settle();
    check(rowText(0) === 'LIVE_NOW' && !viewer.getHistoryState().retained && viewer.followingLive, 'matching reset publishes latest current frame');
    selectFirst();
    viewer.handleViewport(frame('RING_NEW', {first: '1000', total: 12})); await settle();
    check(viewer.getHistoryState().expired && rowText(0) === 'LIVE_NOW', 'capped history eviction is detected without replacing loaded copy');
    check(viewer.offsetText.textContent.includes('만료'), 'expired loaded snapshot visibly identifies its state');
    check(viewer.getSelectedText() === 'L', 'expired loaded snapshot remains copyable until explicit navigation');
    viewer.handleViewport(frame('GEN_NEW', {generation: '2', first: '0', total: 3})); await settle();
    check(viewer.getHistoryState().expired && viewer.getHistoryState().generation === '1', 'generation reset keeps old loaded display until explicit resume');
    viewer.resumeFollow();
    viewer.handleViewport(frame('GEN_NEW', {generation: '2', first: '0', total: 3, request: hostRequests.at(-1).request})); await settle();
    check(rowText(0) === 'GEN_NEW' && viewer.getHistoryState().generation === '2' && !viewer.getHistoryState().expired,
      'explicit resume can replace expired loaded snapshot with latest live generation');
    check(viewer.expectHistoryWindow(200), 'expired request registration');
    viewer.handleViewport(frame('FALLBACK', {generation: '2', first: '0', total: 10, offset: 7, request: 200, expired: true})); await settle();
    check(rowText(0) === 'GEN_NEW' && viewer.getHistoryState().expired && viewer.getHistoryState().pendingRequest === null,
      'expired response signals state and keeps displayed loaded window');
    viewer.resumeFollow();
    const secondReset = hostRequests.at(-1);
    viewer.handleViewport(frame('NOW2', {generation: '2', first: '50', total: 12, request: secondReset.request})); await settle();
    check(rowText(0) === 'NOW2' && !viewer.getHistoryState().expired, 'absolute reset recovers expired history');
    selectFirst(); viewer.setPrivacyCurtain(true);
    viewer.handleViewport(frame('PRIVATE_NEW', {generation: '2', first: '51', total: 13})); await settle();
    check(viewer.textLayer.inert && viewer.textLayer.getAttribute('aria-hidden') === 'true', 'privacy hides retained DOM');
    check(!viewer.textLayer.textContent.includes('PRIVATE_NEW') && viewer.getSelectedText() === '', 'privacy never exposes hidden latest baseline through DOM/copy');
    check(viewer.findText('NOW2').total === 0, 'privacy guards retained search');
    viewer.setPrivacyCurtain(false);
    check(rowText(0) === 'NOW2', 'unprotect preserves retained loaded source');
    viewer.resumeFollow();
    viewer.handleViewport(frame('PRIVATE_NEW', {generation: '2', first: '51', total: 13, request: hostRequests.at(-1).request})); await settle();
    check(rowText(0) === 'PRIVATE_NEW', 'unprotected explicit resume publishes latest baseline');
    selectFirst(); viewer.expectHistoryWindow(300);
    viewer.setViewerConnection('reconnecting');
    check(viewer.getHistoryState().pendingRequest === null, 'disconnect clears pending request lifetime');
    viewer.setViewerConnection('connected'); viewer.rewatch();
    viewer.handleViewport(frame('REWATCH', {generation: '2', first: '52', total: 14})); await settle();
    check(rowText(0) === 'PRIVATE_NEW', 'rewatch keeps paused displayed copy while refreshing baseline');
    viewer.openViewer('history-B', 'B');
    check(viewer.getHistoryState().pendingRequest === null && viewer.screen === null, 'session switch clears prior displayed/request state');
    viewer.handleViewport(frame('STALE_A', {session: 'history-A', request: 300}));
    check(viewer.screen === null, 'old session response cannot populate new session');
    const newSessionResync = sent.filter(msg => msg.type === 'request_keyframe').length;
    viewer.handleViewport(frame('delta', {keyframe: false, session: 'history-B'}));
    check(sent.at(-1).type === 'request_keyframe', 'new session cannot use old wire baseline');
    viewer.handleViewport(frame('delta-again', {keyframe: false, session: 'history-B'}));
    check(sent.filter(msg => msg.type === 'request_keyframe').length === newSessionResync + 1,
      'new session resets and deduplicates missing-baseline resync request');
    viewer.handleViewport(frame('B_LIVE', {first: '200', total: 3, session: 'history-B'})); await settle();
    check(rowText(0) === 'B_LIVE' && !viewer.getHistoryState().expired, 'new session gets independent source state');
    const horizontal = new WheelEvent('wheel', {deltaX: 8, bubbles: true, cancelable: true});
    viewer.wrap.dispatchEvent(horizontal);
    check(!horizontal.defaultPrevented && viewer.getHistoryState().retained && !viewer.followingLive,
      'native horizontal wheel pins current window without taking browser pan');
    viewer.handleViewport(frame('B_NEXT', {first: '201', total: 4}));
    viewer.handleViewport(frame('wire-delta', {first: '201', total: 4, keyframe: false, row: 1})); await settle();
    check(rowText(0) === 'B_LIVE' && rowText(1) === 'beta', 'local pan retains rows while complete wire delta baseline advances');
    viewer.resumeFollow();
    viewer.handleViewport(frame('B_NEXT', {first: '201', total: 4, request: hostRequests.at(-1).request}));
    viewer.handleViewport(frame('wire-delta', {first: '201', total: 4, keyframe: false, row: 1})); await settle();
    check(rowText(0) === 'B_NEXT' && rowText(1) === 'wire-delta', 'matched reset establishes complete baseline for subsequent deltas');
    check(viewer.findText('NOT_FOUND').total === 0 && viewer.getHistoryState().retained && !viewer.followingLive,
      'nonempty search pins its displayed scope even when there are no matches');
    viewer.handleViewport(frame('SEARCH_NEW', {first: '202', total: 5})); await settle();
    check(rowText(0) === 'B_NEXT', 'unmatched search scope remains stable under new output');
    viewer.resumeFollow();
    viewer.handleViewport(frame('SEARCH_NEW', {first: '202', total: 5, request: hostRequests.at(-1).request})); await settle();
    viewer.expectHistoryWindow(600);
    viewer.handleViewport(frame('BIG_KEEP', {generation: '3', first: '90071992547409930', total: 3, request: 600})); await settle();
    viewer.handleViewport(frame('BIG_NEW', {generation: '3', first: '90071992547409931', total: 3})); await settle();
    check(viewer.getHistoryState().expired && rowText(0) === 'BIG_KEEP', 'absolute row eviction compares decimal identities beyond Number precision');
    viewer.resumeFollow();
    viewer.handleViewport(frame('BIG_NEW', {generation: '3', first: '90071992547409931', total: 3, request: hostRequests.at(-1).request})); await settle();
    viewer.setReadableWrap(true); await settle();
    viewer.handleViewport(frame('WRAP_NEW', {generation: '3', first: '90071992547409932', total: 4})); await settle();
    check(rowText(0) === 'BIG_NEW' && viewer.getHistoryState().retained && !viewer.followingLive,
      'reading wrap pins current rows independently of incoming source-window shifts');
    viewer.setReadableWrap(false); viewer.resumeFollow();
    viewer.handleViewport(frame('WRAP_NEW', {generation: '3', first: '90071992547409932', total: 4, request: hostRequests.at(-1).request})); await settle();
    viewer.expectHistoryWindow(700); viewer.expectHistoryWindow(701);
    check(!viewer.cancelHistoryWindow(700) && viewer.getHistoryState().pendingRequest === 701,
      'old request cancellation cannot release latest pending request');
    check(viewer.cancelHistoryWindow(701) && !viewer.cancelHistoryWindow(701)
      && viewer.getHistoryState().pendingRequest === null && viewer.getHistoryState().retained
      && !viewer.followingLive && !viewer.getHistoryState().expired,
      'matching transport cancellation releases pending state without expiring or replacing loaded display');
    viewer.handleViewport(frame('LATE_CANCELED', {generation: '3', first: '90071992547409930', total: 4, offset: 2, request: 701})); await settle();
    check(rowText(0) === 'WRAP_NEW', 'late canceled response cannot replace loaded window');
    viewer.resumeFollow(); const canceledReset = hostRequests.at(-1).request;
    check(viewer.cancelHistoryWindow(canceledReset) && !viewer.resumeFollowButton.disabled,
      'canceling failed reset makes explicit resume action available again');
    viewer.handleViewport(frame('LATE_RESET', {generation: '3', first: '90071992547409933', total: 5, request: canceledReset})); await settle();
    check(rowText(0) === 'WRAP_NEW' && !viewer.followingLive, 'late canceled reset cannot silently resume');
    viewer.resumeFollow(); const privacyReset = hostRequests.at(-1).request;
    viewer.setPrivacyCurtain(true);
    check(viewer.getHistoryState().pendingRequest === null && !viewer.followingLive,
      'privacy covering cancels pending reset lifetime');
    viewer.handleViewport(frame('PRIVATE_LATE', {generation: '3', first: '90071992547409933', total: 5, request: privacyReset})); await settle();
    viewer.setPrivacyCurtain(false);
    check(rowText(0) === 'WRAP_NEW', 'privacy-canceled response remains unpublished after unprotect');
    const outsideStart = document.createElement('span'), outsideEnd = document.createElement('span');
    outsideStart.textContent = 'outside-before'; outsideEnd.textContent = 'outside-after';
    viewer.wrap.prepend(outsideStart); viewer.wrap.append(outsideEnd);
    const selectAcross = () => {
      const range = document.createRange(); range.setStart(outsideStart.firstChild, 0); range.setEnd(outsideEnd.firstChild, outsideEnd.firstChild.length);
      const selection = window.getSelection(); selection.removeAllRanges(); selection.addRange(range);
    };
    selectAcross(); viewer.setPrivacyCurtain(true);
    check(window.getSelection().rangeCount === 0, 'privacy clears selection intersecting output even endpoints are outside terminal layer');
    selectAcross();
    const protectedCopy = new ClipboardEvent('copy', {clipboardData: new DataTransfer(), bubbles: true, cancelable: true});
    document.dispatchEvent(protectedCopy);
    check(protectedCopy.defaultPrevented && protectedCopy.clipboardData.getData('text/plain') === '',
      'privacy blocks native copy of output inside an outside-ended selection');
    window.getSelection().removeAllRanges(); outsideStart.remove(); outsideEnd.remove(); viewer.setPrivacyCurtain(false);
    viewer.resumeFollow();
    viewer.handleViewport(frame('FINAL_NOW', {generation: '3', first: '90071992547409933', total: 5, request: hostRequests.at(-1).request})); await settle();
    viewer.handleViewport(frame('STALE_SOCKET', {generation: '3', first: '90071992547409931', total: 5, offset: 2, request: 799}));
    viewer.setViewerConnection('reconnecting'); viewer.setViewerConnection('connected'); viewer.rewatch();
    const rewatchResync = sent.filter(msg => msg.type === 'request_keyframe').length;
    viewer.handleViewport(frame('delta-after-rewatch', {keyframe: false}));
    viewer.handleViewport(frame('delta-again-after-rewatch', {keyframe: false}));
    check(sent.filter(msg => msg.type === 'request_keyframe').length === rewatchResync + 1,
      'socket transition and rewatch release old resync guard and deduplicate recovery deltas');
    viewer.handleViewport(frame('FINAL_NOW', {generation: '3', first: '90071992547409933', total: 5})); await settle();
    const snapshot = viewer.getHistoryState(); snapshot.retained = true;
    check(viewer.getHistoryState().retained === false, 'history state API returns a copy');
    check(states.some(state => state.expired) && states.some(state => state.pendingRequest), 'reading hook reports expiration and pending transitions');
    check(sent.every(msg => ['watch', 'unwatch', 'request_keyframe'].includes(msg.type)), 'renderer never sends history/write commands');
    viewer.finishCloseViewer();
    check(!viewer.expectHistoryWindow(400) && viewer.getHistoryState().pendingRequest === null, 'closed viewer cannot register navigation');
    viewer.el.remove();
    const legacy = createViewer({send: msg => sent.push(msg)});
    legacy.setViewerConnection('connected'); legacy.openViewer('legacy-C', 'C');
    const legacyFrame = (text, keyframe = true, row = 0) => {
      const msg = frame(text, {session: 'legacy-C', keyframe, row}); delete msg.history; return msg;
    };
    legacy.handleViewport(legacyFrame('LEGACY_KEEP')); await settle();
    legacy.wrap.dispatchEvent(new WheelEvent('wheel', {deltaX: 4, bubbles: true}));
    legacy.handleViewport(legacyFrame('LEGACY_NEW'));
    legacy.handleViewport(legacyFrame('baseline2', false, 2)); await settle();
    check(legacy.getHistoryState().retained && !legacy.getHistoryState().expired && legacy.getHistoryState().generation === null,
      'legacy loaded snapshot reports unknown identity without false expiration');
    check(legacy.screen.lines[0][0].t === 'LEGACY_KEEP', 'legacy readonly screen also remains pinned under advancing output');
    legacy.resumeFollow(); await settle();
    check(legacy.screen.lines[0][0].t === 'LEGACY_NEW' && legacy.screen.lines[2][0].t === 'baseline2'
      && !legacy.getHistoryState().retained, 'no-hook legacy resume publishes complete hidden keyframe and delta baseline');
    legacy.finishCloseViewer();
    finish('ok', 'VIEWER_HISTORY_OK:' + checks);
  } catch (error) { finish('error', 'VIEWER_HISTORY_ERROR: ' + (error.stack || error)); }
})();
