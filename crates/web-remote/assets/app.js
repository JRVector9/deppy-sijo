// Deppy Sijo 모바일 셸 (P1 스캐폴드) — 프레임워크 없음, CSP(default-src 'self') 준수.
// shell 뷰: 토큰 저장/URL 정리 + 서비스 워커 등록 + /healthz 상태 폴링.
// pairing 뷰(401 본문): 저장된 토큰으로 1회 자동 재시도, 무효 토큰은 폐기(루프 방지).
(() => {
  'use strict';
  const TOKEN_KEY = 'deppy.webToken';
  const view = document.body.dataset.view;
  const params = new URLSearchParams(location.search);
  const urlToken = params.get('token');

  if (view === 'pairing') {
    const saved = localStorage.getItem(TOKEN_KEY);
    if (!urlToken && saved) {
      // 재방문(URL에 토큰 없음) — 저장된 페어링으로 자동 복구
      document.getElementById('retry').classList.remove('hidden');
      location.replace('/?token=' + encodeURIComponent(saved));
    } else if (urlToken) {
      // 토큰을 제시했는데도 401 — 재발급 등으로 무효. 저장분을 폐기해 리다이렉트 루프를 막는다.
      localStorage.removeItem(TOKEN_KEY);
    }
    return;
  }

  // shell 뷰 — 토큰을 저장하고 주소창/히스토리에서 제거(위생). 재방문 복구는 401 페이지가 한다.
  if (urlToken) {
    localStorage.setItem(TOKEN_KEY, urlToken);
    history.replaceState(null, '', location.pathname);
  }

  document.getElementById('host').textContent = location.host;

  if ('serviceWorker' in navigator) {
    navigator.serviceWorker.register('/sw.js').catch(() => {});
  }

  const dot = document.getElementById('dot');
  const statusText = document.getElementById('status-text');
  const lastCheck = document.getElementById('last-check');
  async function ping() {
    try {
      const res = await fetch('/healthz', { cache: 'no-store' });
      if (!res.ok) throw new Error(String(res.status));
      dot.className = 'dot ok';
      statusText.textContent = '연결됨';
      lastCheck.textContent = new Date().toLocaleTimeString();
    } catch {
      dot.className = 'dot bad';
      statusText.textContent = '연결 끊김';
    }
  }
  ping();
  setInterval(ping, 15000);
})();
