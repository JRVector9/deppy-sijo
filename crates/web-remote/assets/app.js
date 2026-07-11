// Deppy Sijo 모바일 대시보드 (P2) — 프레임워크 없음, CSP(default-src 'self') 준수.
// WS로 승인/상태를 받아 렌더하고, Allow/Deny를 되보낸다. 신뢰경계: 서버가 보낸 문자열
// (preview/title/server/tool)은 전부 textContent로만 삽입한다 — innerHTML 절대 금지.
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
      // 재방문(URL에 토큰 없음) — 저장된 페어링으로 자동 복구
      location.replace('/?token=' + encodeURIComponent(saved));
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

  const token = localStorage.getItem(TOKEN_KEY);

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

  function setStatus(cls, text) {
    dot.className = 'dot ' + cls;
    statusText.textContent = text;
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
    let socket;
    try {
      socket = new WebSocket(wsUrl());
    } catch (e) {
      scheduleReconnect();
      return;
    }
    ws = socket;

    socket.addEventListener('open', () => {
      reconnectDelay = 1000;
      socket.send(JSON.stringify({ type: 'auth', v: 1, token }));
    });

    socket.addEventListener('message', (event) => {
      let msg;
      try {
        msg = JSON.parse(event.data);
      } catch {
        return;
      }
      handleMessage(msg);
    });

    socket.addEventListener('close', () => {
      if (ws === socket) ws = null;
      if (!manualClose) {
        setStatus('bad', '연결 끊김 — 재연결 중…');
        scheduleReconnect();
      }
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
    manualClose = true;
    if (reconnectTimer) {
      clearTimeout(reconnectTimer);
      reconnectTimer = null;
    }
    if (ws) {
      try { ws.close(); } catch {}
      ws = null;
    }
  }

  function handleMessage(msg) {
    switch (msg && msg.type) {
      case 'welcome':
        setStatus('ok', '연결됨');
        break;
      case 'dashboard':
        // 오프라인 폴백 화면이 "마지막 상태 시각"을 보여줄 수 있게 수신 시각을 저장한다.
        localStorage.setItem(LAST_DASHBOARD_KEY, String(Date.now()));
        renderSessions(msg.sessions || [], msg.resource || null);
        break;
      case 'approvals':
        renderApprovals(msg.pending || []);
        break;
      case 'error':
        setStatus('bad', '오류: ' + (msg.message || ''));
        break;
    }
  }

  function renderApprovals(pending) {
    approvalsCount.textContent = String(pending.length);
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

  function renderSessions(sessions, resource) {
    sessionsEmpty.hidden = sessions.length > 0;
    sessionsEl.textContent = '';
    for (const s of sessions) {
      const li = document.createElement('li');
      li.className = 'session';

      const title = document.createElement('span');
      title.className = 'title';
      title.textContent = s.title || ('세션 ' + s.id);
      li.appendChild(title);

      const badge = document.createElement('span');
      const status = s.exited ? 'done' : (s.status || 'running');
      badge.className = 'badge ' + status;
      badge.textContent = STATUS_LABEL[status] || status;
      li.appendChild(badge);
      sessionsEl.appendChild(li);
    }
    if (resource) {
      const cpu = resource.cpu != null ? resource.cpu.toFixed(0) + '%' : '—';
      resourceEl.textContent = 'CPU ' + cpu + ' · RAM ' + (resource.rss_mb || 0) + 'MB';
    } else {
      resourceEl.textContent = '';
    }
  }

  // P3: iOS 설치 안내 — iOS Safari이고 아직 설치(standalone) 전일 때만 노출. 닫으면 억제한다.
  function maybeShowIosInstallHint() {
    const el = document.getElementById('ios-install');
    if (!el) return;
    const isIos = /iphone|ipad|ipod/i.test(navigator.userAgent);
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

  // P3: 알림 권한 유도 — 앱 아이콘 뱃지(iOS 설치형)·푸시(P4)는 알림 권한 승인 후에만 동작한다.
  // 권한이 아직 미결정(default)일 때만 버튼을 노출한다. 지금은 Notification.requestPermission만
  // 호출하지만, P4에서 웹푸시 구독(pushManager.subscribe)과 같은 클릭 제스처로 통합될 자리다.
  function maybeShowNotifyButton() {
    const btn = document.getElementById('notify-enable');
    if (!btn) return;
    if (!('Notification' in window) || Notification.permission !== 'default') return;
    btn.hidden = false;
    btn.addEventListener('click', () => {
      // iOS/WebKit은 사용자 제스처(클릭 핸들러) 안에서만 권한 요청을 허용한다.
      let result;
      try {
        result = Notification.requestPermission();
      } catch {
        btn.hidden = true;
        return;
      }
      // 구형 Safari는 콜백형(반환 undefined), iOS 16.4+는 Promise형 — 둘 다 처리한다.
      if (result && typeof result.finally === 'function') {
        result.finally(() => { btn.hidden = true; });
      } else {
        btn.hidden = true;
      }
    });
  }

  // 탭 백그라운드 시 스트림 정지(서버 접속 종료 → 0연결 예산 준수). 포그라운드 복귀 시 재연결.
  document.addEventListener('visibilitychange', () => {
    if (document.hidden) {
      disconnect();
      setStatus('', '일시정지(백그라운드)');
    } else {
      connect();
    }
  });

  connect();
})();
