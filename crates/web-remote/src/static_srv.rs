//! 정적 서빙 계층 — 임베드 앱 셸 + 페어링 토큰 게이트.
//!
//! [`crate::ws_api`](P2)와 모듈 경계를 분리해 둔다: 방법 B(클라우드 앱 셸) 이전 시
//! 이 모듈만 정적 호스팅으로 대체되고 ws_api는 데스크톱에 그대로 남는다 (계획 v3.3 §방법 B).

use std::borrow::Cow;
use std::sync::OnceLock;

use crate::http::Response;

// 임베드 자산 바이트 — 한 번만 `include_bytes!`하고 ASSETS/SHELL에서 재사용한다(중복 임베드 방지).
const APP_CSS: &[u8] = include_bytes!("../assets/app.css");
const APP_JS: &[u8] = include_bytes!("../assets/app.js");
const MANIFEST: &[u8] = include_bytes!("../assets/manifest.webmanifest");
const OFFLINE_HTML: &[u8] = include_bytes!("../assets/offline.html");
const ICON_192: &[u8] = include_bytes!("../assets/icon-192.png");
const ICON_512: &[u8] = include_bytes!("../assets/icon-512.png");
const ICON_MASKABLE_512: &[u8] = include_bytes!("../assets/icon-maskable-512.png");
const APPLE_TOUCH_ICON: &[u8] = include_bytes!("../assets/apple-touch-icon.png");

/// 컴파일 타임 자산 매니페스트: 경로 → (MIME, bytes). 전부 임베드 — 파일 시스템 접근/디렉터리
/// 탐색이 없으므로 이 목록이 곧 경로 화이트리스트다. `/sw.js`는 버전 키를 서빙 시점에 주입하므로
/// 여기 두지 않고 [`respond`]가 별도 처리한다.
const ASSETS: &[(&str, &str, &[u8])] = &[
    ("/app.css", "text/css; charset=utf-8", APP_CSS),
    ("/app.js", "text/javascript; charset=utf-8", APP_JS),
    (
        "/manifest.webmanifest",
        "application/manifest+json",
        MANIFEST,
    ),
    ("/offline.html", "text/html; charset=utf-8", OFFLINE_HTML),
    ("/icon-192.png", "image/png", ICON_192),
    ("/icon-512.png", "image/png", ICON_512),
    ("/icon-maskable-512.png", "image/png", ICON_MASKABLE_512),
    ("/apple-touch-icon.png", "image/png", APPLE_TOUCH_ICON),
];

/// SW가 프리캐시하는 셸 = (경로, 내용). 이 내용들의 해시가 곧 캐시 버전 키다 — 셸 바이트가
/// 바뀌면 키가 바뀌어 SW가 새 셸을 설치하고 구 캐시를 지운다. `assets/sw.js`의 SHELL 배열과
/// 구성이 일치해야 한다(drift 방지 테스트가 강제).
const SHELL: &[(&str, &[u8])] = &[
    ("/app.css", APP_CSS),
    ("/app.js", APP_JS),
    ("/icon-192.png", ICON_192),
    ("/manifest.webmanifest", MANIFEST),
    ("/offline.html", OFFLINE_HTML),
];

/// 서비스 워커 템플릿 — [`respond`]가 [`SW_VERSION_PLACEHOLDER`]를 셸 해시로 치환해 서빙한다.
const SW_JS_TEMPLATE: &str = include_str!("../assets/sw.js");
/// sw.js 안에서 서빙 시점에 셸 버전 해시로 치환되는 자리표시자.
const SW_VERSION_PLACEHOLDER: &str = "__SHELL_VERSION__";

/// 앱 셸 문서 — `/?token=` 게이트 통과 시에만 서빙.
const INDEX_HTML: &[u8] = include_bytes!("../assets/index.html");
/// 게이트 실패(401) 본문 — 저장된 토큰으로 자동 복구를 시도하는 페어링 안내 페이지.
const PAIRING_HTML: &[u8] = include_bytes!("../assets/pairing.html");

/// GET 요청 하나를 정적 계층에서 라우팅한다. 화이트리스트 밖 경로는 404.
///
/// 토큰 게이트는 문서("/")에만 건다: 서브 자산은 비밀 없는 셸 코드이고(방법 B에서는
/// 아예 공개 호스팅으로 이동), 데이터/행위 권한은 WS API(P2)가 접속마다 첫 프레임
/// 인증으로 지킨다. 401 본문은 localStorage 토큰으로 자동 재시도하는 페어링 페이지다.
pub fn respond(path: &str, query: &str, expected_token: &str) -> Response {
    match path {
        "/" => {
            if token_param_matches(query, expected_token) {
                html(200, INDEX_HTML)
            } else {
                html(401, PAIRING_HTML)
            }
        }
        // 셸 JS의 연결 상태 폴링용 — 살아있음 외 아무것도 노출하지 않는다.
        "/healthz" => Response {
            status: 200,
            content_type: "application/json",
            body: Cow::Borrowed(br#"{"status":"ok"}"#),
        },
        // 서비스 워커 — 버전 키를 서빙 시점에 셸 해시로 주입한다(토큰 게이트 없음, 비밀 없음).
        "/sw.js" => sw_js_response(),
        _ => ASSETS
            .iter()
            .find(|(asset_path, _, _)| *asset_path == path)
            .map(|(_, mime, bytes)| Response {
                status: 200,
                content_type: mime,
                body: Cow::Borrowed(*bytes),
            })
            .unwrap_or_else(|| Response::plain(404, "not found")),
    }
}

/// `/sw.js` 응답 — 버전 자리표시자를 셸 해시로 치환해 서빙한다. sw.js는 저빈도 요청이라
/// 요청마다 치환해도 비용이 무시할 수준이다. 응답 헤더는 http.rs 전역 `Cache-Control: no-cache`라
/// 브라우저가 매 방문 재검증한다 → 셸이 바뀌면 재방문 2회 내 새 SW가 반영된다.
fn sw_js_response() -> Response {
    let body = SW_JS_TEMPLATE.replace(SW_VERSION_PLACEHOLDER, shell_version());
    Response {
        status: 200,
        content_type: "text/javascript; charset=utf-8",
        body: Cow::Owned(body.into_bytes()),
    }
}

/// SW 캐시 버전 키 — 프리캐시 셸 자산 바이트의 FNV-1a 해시(16진 16자리). 보안용이 아니라
/// 캐시 무효화용이다. lazy 1회 계산 후 재사용 — 서빙마다 재계산하지 않는다.
fn shell_version() -> &'static str {
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION.get_or_init(|| {
        let chunks: Vec<&[u8]> = SHELL.iter().map(|&(_, bytes)| bytes).collect();
        fnv1a_hex(&chunks)
    })
}

/// FNV-1a 64비트 해시를 청크들에 걸쳐 누적하고 16진 문자열로 돌려준다. 의존성 없는 결정적
/// 해시 — 청크 경계와 무관하게 이어붙인 내용만으로 정해진다(같은 내용 → 같은 키).
fn fnv1a_hex(chunks: &[&[u8]]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325; // FNV-1a offset basis
    for chunk in chunks {
        for &byte in *chunk {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3); // FNV prime
        }
    }
    format!("{hash:016x}")
}

/// query에서 `token=` 파라미터를 찾아 상수시간 비교한다. 토큰은 hex라 percent 인코딩이 없다.
fn token_param_matches(query: &str, expected: &str) -> bool {
    let Some(provided) = query.split('&').find_map(|kv| kv.strip_prefix("token=")) else {
        return false;
    };
    token_matches(expected, provided.as_bytes())
}

/// 상수 시간 비교 — 토큰 내용의 타이밍 누설 방지 (remote.rs token_matches 관례).
/// 길이 불일치는 즉시 거부 — 토큰 길이(hex 64자)는 공개 정보라 누설이 아니다.
/// WS 첫 프레임 인증(ws_api)도 같은 비교를 재사용한다.
pub(crate) fn token_matches(expected: &str, provided: &[u8]) -> bool {
    let expected = expected.as_bytes();
    if expected.len() != provided.len() {
        return false;
    }
    expected
        .iter()
        .zip(provided)
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

fn html(status: u16, body: &'static [u8]) -> Response {
    Response {
        status,
        content_type: "text/html; charset=utf-8",
        body: Cow::Borrowed(body),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn 토큰_일치는_앱셸_200() {
        let response = respond("/", &format!("token={TOKEN}"), TOKEN);
        assert_eq!(response.status, 200);
        assert!(
            String::from_utf8_lossy(&response.body).contains(r#"data-view="shell""#),
            "앱 셸 문서가 아님"
        );
    }

    #[test]
    fn 토큰_불일치나_누락은_401_페어링_페이지() {
        for query in ["", "token=", "token=wrong", "foo=bar", "tokens=xx"] {
            let response = respond("/", query, TOKEN);
            assert_eq!(response.status, 401, "query={query}");
            assert!(
                String::from_utf8_lossy(&response.body).contains(r#"data-view="pairing""#),
                "페어링 안내 페이지가 아님"
            );
        }
    }

    #[test]
    fn 다른_파라미터_사이의_토큰도_찾는다() {
        let response = respond("/", &format!("a=b&token={TOKEN}&c=d"), TOKEN);
        assert_eq!(response.status, 200);
    }

    #[test]
    fn 화이트리스트_자산은_mime과_함께_200() {
        for (path, mime) in [
            ("/app.css", "text/css; charset=utf-8"),
            ("/app.js", "text/javascript; charset=utf-8"),
            ("/manifest.webmanifest", "application/manifest+json"),
            ("/offline.html", "text/html; charset=utf-8"),
            ("/icon-192.png", "image/png"),
            ("/icon-512.png", "image/png"),
            ("/icon-maskable-512.png", "image/png"),
            ("/apple-touch-icon.png", "image/png"),
        ] {
            let response = respond(path, "", TOKEN);
            assert_eq!(response.status, 200, "{path}");
            assert_eq!(response.content_type, mime, "{path}");
            assert!(!response.body.is_empty(), "{path}");
        }
    }

    #[test]
    fn 화이트리스트_밖_경로는_404() {
        for path in [
            "/index.html", // 문서는 "/"만 — 별칭 없음
            "/unknown.js",
            "/../Cargo.toml",
            "/assets/app.js",
            "/app.js/",  // 정확 일치만
            "/icon.svg", // P3에서 sijobird PNG로 교체 — 더 이상 서빙하지 않음
        ] {
            assert_eq!(respond(path, "", TOKEN).status, 404, "{path}");
        }
    }

    #[test]
    fn sw_js는_버전_자리표시자를_해시로_치환해_서빙() {
        let r1 = respond("/sw.js", "", TOKEN);
        assert_eq!(r1.status, 200);
        assert_eq!(r1.content_type, "text/javascript; charset=utf-8");
        let body1 = String::from_utf8(r1.body.to_vec()).unwrap();
        assert!(
            !body1.contains(SW_VERSION_PLACEHOLDER),
            "자리표시자가 치환되지 않음"
        );
        assert!(
            body1.contains(&format!("deppy-shell-{}", shell_version())),
            "캐시 키에 버전 해시가 없음"
        );
        // 두 번 요청해도 동일한 버전(결정적) — 재방문 안정성.
        let r2 = respond("/sw.js", "", TOKEN);
        assert_eq!(r1.body, r2.body);
    }

    #[test]
    fn 셸_버전키는_16진수_16자리() {
        let v = shell_version();
        assert_eq!(v.len(), 16, "{v}");
        assert!(v.chars().all(|c| c.is_ascii_hexdigit()), "{v}");
    }

    #[test]
    fn fnv1a는_내용이_바뀌면_키가_바뀐다() {
        // 같은 내용 → 같은 키
        assert_eq!(fnv1a_hex(&[&b"deppy"[..]]), fnv1a_hex(&[&b"deppy"[..]]));
        // 한 바이트만 달라도 → 다른 키(자산 변경 시 캐시 무효화 보장)
        assert_ne!(fnv1a_hex(&[&b"deppy"[..]]), fnv1a_hex(&[&b"deppz"[..]]));
        // 청크 경계는 무관 — 이어붙인 내용만으로 정해진다
        assert_eq!(
            fnv1a_hex(&[&b"de"[..], &b"ppy"[..]]),
            fnv1a_hex(&[&b"deppy"[..]])
        );
    }

    #[test]
    fn 프리캐시_셸은_모두_서빙되고_sw에_명시된다() {
        for &(path, _) in SHELL {
            assert_eq!(respond(path, "", TOKEN).status, 200, "미서빙 셸: {path}");
            assert!(
                SW_JS_TEMPLATE.contains(&format!("'{path}'")),
                "sw.js SHELL에 누락: {path}"
            );
        }
    }

    #[test]
    fn manifest는_유효_json이고_아이콘이_서빙된다() {
        let resp = respond("/manifest.webmanifest", "", TOKEN);
        let json: serde_json::Value =
            serde_json::from_slice(&resp.body).expect("manifest JSON 파싱 실패");
        let icons = json["icons"].as_array().expect("icons 배열 없음");
        assert!(!icons.is_empty(), "아이콘 없음");
        for icon in icons {
            let src = icon["src"].as_str().expect("icon src 없음");
            assert_eq!(respond(src, "", TOKEN).status, 200, "아이콘 미서빙: {src}");
        }
    }

    #[test]
    fn healthz는_토큰_없이_200_json() {
        let response = respond("/healthz", "", TOKEN);
        assert_eq!(response.status, 200);
        assert_eq!(response.content_type, "application/json");
    }

    #[test]
    fn 상수시간_비교는_길이와_내용을_본다() {
        assert!(token_matches("abc", b"abc"));
        assert!(!token_matches("abc", b"abd"));
        assert!(!token_matches("abc", b"ab"));
        assert!(!token_matches("abc", b"abcd"));
        assert!(!token_matches("", b"a"));
        assert!(token_matches("", b""));
    }
}
