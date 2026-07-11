// Deppy Sijo 서비스 워커 (P1 최소) — 공개 셸 자산만 cache-first.
// '/'(문서)는 토큰 게이트가 있어 사전 캐시하지 않는다 — 오프라인 셸/버전 키 갱신은 P3.
const CACHE = 'deppy-shell-v1';
const SHELL = ['/app.css', '/app.js', '/icon.svg', '/manifest.webmanifest'];

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
  const url = new URL(event.request.url);
  if (event.request.method !== 'GET' || url.origin !== location.origin) return;
  if (!SHELL.includes(url.pathname)) return; // 문서·healthz는 네트워크 직행
  event.respondWith(
    caches.match(event.request, { ignoreSearch: true }).then((hit) => hit || fetch(event.request))
  );
});
