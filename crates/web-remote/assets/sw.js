// Deppy Sijo 서비스 워커 (P3) — 앱 셸 cache-first + 오프라인 폴백 + 버전드 캐시.
//
// 버전 키(__SHELL_VERSION__)는 static_srv가 서빙 시점에 셸 자산 내용 해시로 치환한다.
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
