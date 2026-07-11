//! 정적 서빙 계층 — 임베드 앱 셸 + 페어링 토큰 게이트.
//!
//! [`crate::ws_api`](P2)와 모듈 경계를 분리해 둔다: 방법 B(클라우드 앱 셸) 이전 시
//! 이 모듈만 정적 호스팅으로 대체되고 ws_api는 데스크톱에 그대로 남는다 (계획 v3.3 §방법 B).

use std::borrow::Cow;

use crate::http::Response;

/// 컴파일 타임 자산 매니페스트: 경로 → (MIME, bytes). 전부 `include_bytes!` 임베드 —
/// 파일 시스템 접근/디렉터리 탐색이 없으므로 이 목록이 곧 경로 화이트리스트다.
const ASSETS: &[(&str, &str, &[u8])] = &[
    (
        "/app.css",
        "text/css; charset=utf-8",
        include_bytes!("../assets/app.css"),
    ),
    (
        "/app.js",
        "text/javascript; charset=utf-8",
        include_bytes!("../assets/app.js"),
    ),
    (
        "/manifest.webmanifest",
        "application/manifest+json",
        include_bytes!("../assets/manifest.webmanifest"),
    ),
    (
        "/sw.js",
        "text/javascript; charset=utf-8",
        include_bytes!("../assets/sw.js"),
    ),
    (
        "/icon.svg",
        "image/svg+xml",
        include_bytes!("../assets/icon.svg"),
    ),
];

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

/// query에서 `token=` 파라미터를 찾아 상수시간 비교한다. 토큰은 hex라 percent 인코딩이 없다.
fn token_param_matches(query: &str, expected: &str) -> bool {
    let Some(provided) = query.split('&').find_map(|kv| kv.strip_prefix("token=")) else {
        return false;
    };
    token_matches(expected, provided.as_bytes())
}

/// 상수 시간 비교 — 토큰 내용의 타이밍 누설 방지 (remote.rs token_matches 관례).
/// 길이 불일치는 즉시 거부 — 토큰 길이(hex 64자)는 공개 정보라 누설이 아니다.
fn token_matches(expected: &str, provided: &[u8]) -> bool {
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
            ("/sw.js", "text/javascript; charset=utf-8"),
            ("/icon.svg", "image/svg+xml"),
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
            "/app.js/", // 정확 일치만
        ] {
            assert_eq!(respond(path, "", TOKEN).status, 404, "{path}");
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
