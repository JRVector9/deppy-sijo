// Deppy Sijo 서비스 워커 (P3) — 앱 셸 cache-first + 오프라인 폴백 + 버전드 캐시 + 웹푸시(P4).
//
// 버전 키는 static_srv가 서빙 시점에 셸 자산 내용 해시로 치환한다(아래 CACHE 상수의 자리표시자).
// 자산은 include_bytes! 임베드라 런타임 파일시스템이 없어, 컴파일된 바이트에서 해시를
// 만든다(FNV-1a). 셸 자산이 바뀌면 해시가 바뀌어 install이 새 캐시를 채우고 activate가
// 구 캐시를 지운다 — 서버 응답은 Cache-Control: no-cache라 재방문 2회 내 새 셸이 반영된다.
//
// 토큰 게이트 문서('/')·'/ws'·'/healthz'·동적 API는 절대 캐시하지 않는다.
const CACHE = 'deppy-shell-__SHELL_VERSION__';

// 오프라인 렌더에 필요한 최소 셸만 프리캐시한다. 큰 아이콘(512/maskable)은 설치 시점에
// 브라우저가 직접 받으므로 오프라인 캐시에 넣지 않는다. 이 목록은 static_srv의 SHELL
// (해시 대상)과 일치해야 한다 — drift 방지 테스트가 강제한다.
const SHELL = [
  '/app.css',
  '/app.js',
  '/icon-192.png',
  '/manifest.webmanifest',
  '/offline.html',
];

self.addEventListener('install', (event) => {
  event.waitUntil(
    caches.open(CACHE).then((cache) => cache.addAll(SHELL)).then(() => self.skipWaiting())
  );
});

self.addEventListener('activate', (event) => {
  event.waitUntil(
    caches.keys()
      .then((keys) => Promise.all(keys.filter((key) => key !== CACHE).map((key) => caches.delete(key))))
      .then(() => self.clients.claim())
  );
});

self.addEventListener('fetch', (event) => {
  const req = event.request;
  if (req.method !== 'GET') return;
  const url = new URL(req.url);
  if (url.origin !== location.origin) return;

  // 문서 네비게이션은 항상 네트워크 우선 — 토큰 게이트 '/'를 캐시하지 않기 위함.
  // 오프라인이면 캐시된 오프라인 셸로 폴백한다(마지막 상태 시각을 보여준다).
  if (req.mode === 'navigate') {
    event.respondWith(fetch(req).catch(() => caches.match('/offline.html')));
    return;
  }

  // 셸 자산만 cache-first. 그 외(/ws·/healthz·동적 API)는 SW 미개입 = 네트워크 직행.
  if (SHELL.includes(url.pathname)) {
    event.respondWith(
      caches.match(req, { ignoreSearch: true }).then((hit) => hit || fetch(req))
    );
  }
});

// ── 웹푸시(P4) — 앱(브라우저)이 닫혀 있어도 승인/상태 알림을 띄운다 ──────────────
// 페이로드는 종류/제목/개수만 담는다(도구 인자·로그 없음 — web-remote push.rs가 최소화).
// RFC 8291로 E2E 암호화돼 전달되며, 브라우저가 복호해 event.data로 준다.
self.addEventListener('push', (event) => {
  let data = {};
  try {
    data = event.data ? event.data.json() : {};
  } catch {
    data = {};
  }
  const title = typeof data.title === 'string' ? data.title : '시조새';
  event.waitUntil(
    self.registration.showNotification(title, {
      body: notificationBody(data),
      // 같은 종류 알림을 합쳐 스택을 막는다(승인은 최신 개수로 갱신).
      tag: typeof data.tag === 'string' ? data.tag : 'deppy',
      renotify: true,
      icon: '/icon-192.png',
      badge: '/icon-192.png',
      // 딥링크: 셸 루트로 포커스/열기 — 셸 단일 화면의 최상단이 승인 패널이라, 열면 곧바로
      // 승인 대기가 보인다(WS 재연결로 즉시 목록 수신). P3 셸 라우팅에 맞춰 확정한 스킴.
      data: { url: '/' },
    })
  );
});

// 알림 클릭 — 이미 열린 셸 탭이 있으면 포커스, 없으면 새로 연다.
self.addEventListener('notificationclick', (event) => {
  event.notification.close();
  const target = (event.notification.data && event.notification.data.url) || '/';
  event.waitUntil(
    self.clients
      .matchAll({ type: 'window', includeUncontrolled: true })
      .then((clients) => {
        for (const client of clients) {
          // 같은 출처의 열린 창이 있으면 포커스(토큰은 localStorage에 있어 재페어링 불필요).
          if ('focus' in client) return client.focus();
        }
        return self.clients.openWindow ? self.clients.openWindow(target) : undefined;
      })
  );
});

// 알림 본문 — 종류별 고정 문자열(민감정보 없음). count는 숫자로만 쓴다.
function notificationBody(data) {
  switch (data.kind) {
    case 'approval': {
      const n = Number(data.count) || 0;
      return n > 0 ? n + '건의 승인 요청이 있습니다' : '승인 요청이 있습니다';
    }
    case 'done':
      return '에이전트 세션이 완료되었습니다';
    case 'waiting':
      return '에이전트가 입력을 기다리고 있습니다';
    default:
      return '새 알림이 있습니다';
  }
}
