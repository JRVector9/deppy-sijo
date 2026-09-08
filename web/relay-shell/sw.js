// Deppy Relay 신뢰 셸의 서비스워커 — **정적 자산만** 캐시한다.
//
// 절대 캐시하지 않는 것:
// - 페어링 링크·입장 핸들·페어링 비밀. 애초에 조각(fragment)은 요청에 실리지 않고, 질의
//   문자열이 붙은 요청은 아래에서 통째로 무시한다.
// - 암호문. 애플리케이션 트래픽은 WebSocket으로만 흐르며 fetch 이벤트를 만들지 않는다.
// - 세션 데이터·대시보드·뷰포트. 이 워커는 그런 것이 있는지도 모른다.
// - 방문 기록. 네비게이션 요청 자체를 캐시에 넣지 않고, 미리 담아 둔 셸 문서 하나로만 답한다.
//
// 프로토콜 최소 버전 판정은 **이 파일의 일이 아니다**. relay-shell.js가 한다. 서비스워커가
// 버전을 판정하면 낡은 캐시가 새 계약을 흉내 내는 다운그레이드 경로가 생긴다.
//
// 캐시 키의 `__SHELL_VERSION__`은 build.sh가 릴리스 내용 해시로 바꾼다. 자산이 한 바이트라도
// 바뀌면 키가 바뀌고, activate가 옛 캐시를 지운다.

const CACHE = "deppy-relay-shell-__SHELL_VERSION__";

// 셸이 실행 중에 불러오는 것은 **전부** 여기 있어야 한다. 하나라도 빠지면 셸은 정상 부팅한
// 뒤 세션 화면에서만 실패한다 — 페어링 링크는 1회용이라 그 시점의 실패는 복구가 비싸다.
const STATIC = [
  "./index.html",
  "./relay-shell.js",
  "./relay-terminal.js",
  "./relay-crypto.js",
  "./relay-shell.css",
  "./mobile-theme.css",
  "./manifest.webmanifest",
];

const SHELL_DOCUMENT = "./index.html";

self.addEventListener("install", (event) => {
  event.waitUntil(
    caches
      .open(CACHE)
      .then((cache) => cache.addAll(STATIC))
      .then(() => self.skipWaiting()),
  );
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    caches
      .keys()
      .then((keys) => Promise.all(keys.filter((key) => key !== CACHE).map((key) => caches.delete(key))))
      .then(() => self.clients.claim()),
  );
});

self.addEventListener("fetch", (event) => {
  const request = event.request;
  if (request.method !== "GET") return;

  const url = new URL(request.url);
  if (url.origin !== self.location.origin) return;
  // 질의 문자열이 붙은 요청은 손대지 않는다. 셸의 정적 자산에는 질의가 붙지 않는다.
  if (url.search !== "") return;

  if (request.mode === "navigate") {
    // 네비게이션 요청 객체는 캐시에 넣지 않는다 — 미리 담아 둔 셸 문서로만 답한다.
    event.respondWith(
      caches
        .open(CACHE)
        .then((cache) => cache.match(SHELL_DOCUMENT))
        .then((hit) => hit ?? fetch(request)),
    );
    return;
  }

  const path = url.pathname.replace(/^.*\//, "./");
  if (!STATIC.includes(path)) return;
  event.respondWith(
    caches
      .open(CACHE)
      .then((cache) => cache.match(path))
      .then((hit) => hit ?? fetch(request)),
  );
});
