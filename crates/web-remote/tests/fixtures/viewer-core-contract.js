// web/shared/viewer-core.js를 **격리된 실제 브라우저**에서 구동하는 계약 러너.
// 같은 <script type="module"> 안에 코어가 먼저 이어 붙으므로 createViewer가 그대로 보인다
// (루프백 셸이 서빙하는 번들과 같은 합성 방식이다).
//
// 확인하는 것: hooks를 하나도 주지 않은 **시청 전용** 셸에서 코어가 스스로 초기화되고,
// 문서화된 API로 watch / request_keyframe / unwatch를 그 순서대로 보낸다.

(() => {
  const done = (status, text) => {
    document.body.dataset.status = status;
    document.body.textContent = text;
  };
  const fail = (message) => done('error', 'VIEWER_CORE_ERROR: ' + message);
  window.addEventListener('error', (event) => fail('uncaught: ' + (event.message || event)));

  const check = (condition, message) => {
    if (!condition) throw new Error(message);
  };

  try {
    const sent = [];
    // hooks 없음 = 시청 전용 셸. 코어 단독으로 완결해야 한다.
    const viewer = createViewer({
      send: (message) => {
        sent.push(message);
        return true;
      },
    });

    check(viewer.watching === null, 'initial watching must be null');
    check(viewer.connection === 'connecting', 'initial connection must be connecting');
    check(viewer.el.hidden === true, 'viewer must start hidden');

    viewer.setViewerConnection('connected');
    check(viewer.connection === 'connected', 'connected state not applied');
    check(viewer.overlay.hidden === true, 'overlay must hide when connected');
    check(viewer.connectionLabel.textContent === '연결됨', 'connected label not applied');

    check(viewer.openViewer('sess-1', '세션 A') === true, 'openViewer must report success');
    check(viewer.watching === 'sess-1', 'watching session not recorded');
    check(viewer.el.hidden === false, 'viewer must be visible after open');
    check(viewer.label.textContent === '세션 A', 'session title not applied');
    check(viewer.openViewer('sess-1', '세션 A') === false, 're-opening same session must no-op');

    // delta 프레임인데 기준 화면이 없다 → 재동기화 요청.
    viewer.handleViewport({ session: 'sess-1', keyframe: false, cols: 4, rows: 2, lines: [] });
    check(viewer.screen === null, 'delta without keyframe must not build a screen');

    viewer.handleViewport({
      session: 'sess-1',
      keyframe: true,
      cols: 4,
      rows: 2,
      offset: 3,
      lines: [{ row: 0, runs: [{ s: 0, t: 'ok' }] }],
      cursor: { visible: true, row: 1, col: 0 },
    });
    check(viewer.screen !== null && viewer.screen.rows === 2, 'keyframe must build the screen');
    check(viewer.screen.offset === 3, 'scrollback offset not carried');
    check(viewer.scrollNote.hidden === false, 'scroll note must show while scrolled back');

    // 다른 세션의 잔여 프레임은 무시한다.
    viewer.handleViewport({ session: 'other', keyframe: true, cols: 1, rows: 1, lines: [] });
    check(viewer.screen.rows === 2, 'frames for another session must be ignored');

    viewer.setPrivacyCurtain(true);
    check(viewer.privacy.hidden === false, 'privacy curtain must cover a watched session');
    viewer.setPrivacyCurtain(false);
    check(viewer.privacy.hidden === true, 'privacy curtain must lift');

    viewer.setViewerConnection('reconnecting');
    check(viewer.overlay.hidden === false, 'overlay must show while reconnecting');
    check(viewer.connectionLabel.textContent === '재연결 중', 'reconnecting label not applied');
    check(viewer.back.disabled === false, 'reconnect must not lock Back');

    viewer.finishCloseViewer();
    check(viewer.watching === null, 'close must clear the watched session');
    check(viewer.el.hidden === true, 'close must hide the viewer');
    check(viewer.closing === false, 'close must release the closing lock');

    const types = sent.map((message) => message.type).join(',');
    check(types === 'watch,request_keyframe,unwatch', 'unexpected wire sequence: ' + types);
    check(sent[0].session === 'sess-1', 'watch must name the session');

    // 렌더 프레임까지 돌려 canvas 경로에서 예외가 나지 않는지 확인한 뒤에만 성공을 알린다.
    requestAnimationFrame(() => {
      requestAnimationFrame(() => {
        if (document.body.dataset.status === 'error') return;
        done('ok', 'VIEWER_CORE_OK:' + types);
      });
    });
  } catch (error) {
    fail((error && error.message) || String(error));
  }
})();
