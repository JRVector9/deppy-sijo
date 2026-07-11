//! WWW-Authenticate 헤더 파서 (PR-H4, RFC 7235 §4.1).
//! 다중 챌린지와 quoted-string 안의 콤마를 처리한다 — 알고리즘은 VS Code
//! oauth.ts `parseWWWAuthenticateHeader`(따옴표 인지 콤마 분할 → scheme/param
//! 재조립)를 따르되, quoted-pair(`\"`) 이스케이프까지 해석한다.
//! H5의 401 사다리가 Bearer 챌린지에서 `resource_metadata`(RFC 9728 §5.1)와
//! `scope`를 읽는 데 쓴다.

/// 파싱된 챌린지 하나: scheme + auth 파라미터 (등장 순서 유지).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthChallenge {
    pub scheme: String,
    /// (키, 값) 목록 — 키는 원문 보존, 조회는 [`AuthChallenge::param`]으로 대소문자 무시.
    pub params: Vec<(String, String)>,
}

impl AuthChallenge {
    /// 파라미터 조회 (키 대소문자 무시 — RFC 7235 auth-param 규칙).
    pub fn param(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// RFC 9728 §5.1 — 보호 리소스 메타데이터 URL.
    pub fn resource_metadata(&self) -> Option<&str> {
        self.param("resource_metadata")
    }

    /// RFC 6750 §3 — 요구 scope.
    pub fn scope(&self) -> Option<&str> {
        self.param("scope")
    }

    /// RFC 6750 §3.1 — 에러 코드 (invalid_token 등).
    pub fn error(&self) -> Option<&str> {
        self.param("error")
    }
}

/// WWW-Authenticate 헤더 값을 챌린지 목록으로 파싱한다.
/// 기형 입력에도 panic 없이 해석 가능한 부분만 돌려준다 (파서는 관대, 판단은 소비자).
pub fn parse_www_authenticate(header_value: &str) -> Vec<AuthChallenge> {
    let mut challenges: Vec<AuthChallenge> = Vec::new();
    let mut current: Option<AuthChallenge> = None;

    for part in split_outside_quotes(header_value) {
        let (first, rest) = match part.split_once(char::is_whitespace) {
            Some((first, rest)) => (first, Some(rest.trim_start())),
            None => (part.as_str(), None),
        };
        // 새 챌린지 시작 판정 (VS Code 방식): 첫 토큰에 '='가 없고, 나머지가
        // 없거나(`Basic`) 나머지가 param 꼴(`realm=..`)일 때. `Negotiate <token68>`
        // 형태의 나머지는 param이 아니라서 이 규칙으로 구분되지 않는다 —
        // Bearer 챌린지에는 없는 형태라 VS Code와 동일하게 버린다.
        let starts_new = !first.contains('=') && rest.is_none_or(|r| r.contains('='));
        if starts_new {
            if let Some(done) = current.take() {
                challenges.push(done);
            }
            let mut challenge = AuthChallenge {
                scheme: first.to_owned(),
                params: Vec::new(),
            };
            if let Some(rest) = rest {
                push_param(&mut challenge.params, rest);
            }
            current = Some(challenge);
        } else if let Some(challenge) = current.as_mut() {
            push_param(&mut challenge.params, &part);
        }
        // current가 없는 param 조각(챌린지 없이 시작)은 기형 헤더 — 버린다
    }
    if let Some(done) = current.take() {
        challenges.push(done);
    }
    challenges
}

/// Bearer 챌린지를 찾는다 (scheme 대소문자 무시 — RFC 7235).
pub fn find_bearer_challenge(challenges: &[AuthChallenge]) -> Option<&AuthChallenge> {
    challenges
        .iter()
        .find(|c| c.scheme.eq_ignore_ascii_case("Bearer"))
}

/// quoted-string 밖의 콤마로만 분할한다. quoted-pair(`\x`)는 통째로 보존.
fn split_outside_quotes(value: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut buf = String::new();
    let mut in_quotes = false;
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                buf.push(c);
            }
            '\\' if in_quotes => {
                buf.push(c);
                if let Some(escaped) = chars.next() {
                    buf.push(escaped);
                }
            }
            ',' if !in_quotes => flush_part(&mut parts, &mut buf),
            _ => buf.push(c),
        }
    }
    flush_part(&mut parts, &mut buf);
    parts
}

fn flush_part(parts: &mut Vec<String>, buf: &mut String) {
    let trimmed = buf.trim();
    if !trimmed.is_empty() {
        parts.push(trimmed.to_owned());
    }
    buf.clear();
}

/// `key=value` 조각 하나를 params에 넣는다. 값의 따옴표 제거 + quoted-pair 복원.
fn push_param(params: &mut Vec<(String, String)>, raw: &str) {
    // '=' 없는 조각(token68 등)은 auth-param이 아니다 — 버린다
    let Some((key, value)) = raw.split_once('=') else {
        return;
    };
    let key = key.trim();
    if key.is_empty() {
        return;
    }
    params.push((key.to_owned(), unquote(value.trim())));
}

/// 양끝 따옴표를 벗기고 quoted-pair(`\x` → `x`)를 복원한다. 따옴표가 없으면 원문 그대로.
fn unquote(value: &str) -> String {
    let Some(inner) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) else {
        return value.to_owned();
    };
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc_7235_다중_챌린지_예제() {
        // RFC 7235 §4.1 예제 — escaped quote 포함
        let header =
            r#"Newauth realm="apps", type=1, title="Login to \"apps\"", Basic realm="simple""#;
        let challenges = parse_www_authenticate(header);
        assert_eq!(challenges.len(), 2, "{challenges:?}");
        assert_eq!(challenges[0].scheme, "Newauth");
        assert_eq!(challenges[0].param("realm"), Some("apps"));
        assert_eq!(challenges[0].param("type"), Some("1"));
        assert_eq!(challenges[0].param("title"), Some(r#"Login to "apps""#));
        assert_eq!(challenges[1].scheme, "Basic");
        assert_eq!(challenges[1].param("realm"), Some("simple"));
    }

    #[test]
    fn bearer_챌린지의_resource_metadata와_scope_추출() {
        let header = r#"Bearer resource_metadata="https://rs.example/.well-known/oauth-protected-resource", scope="mcp.read mcp.write", error="invalid_token""#;
        let challenges = parse_www_authenticate(header);
        let bearer = find_bearer_challenge(&challenges).unwrap();
        assert_eq!(
            bearer.resource_metadata(),
            Some("https://rs.example/.well-known/oauth-protected-resource")
        );
        assert_eq!(bearer.scope(), Some("mcp.read mcp.write"));
        assert_eq!(bearer.error(), Some("invalid_token"));
    }

    #[test]
    fn quoted_string_안의_콤마는_분할하지_않는다() {
        let header = r#"Bearer realm="a,b,c", error_description="expired, please re-authenticate""#;
        let challenges = parse_www_authenticate(header);
        assert_eq!(challenges.len(), 1, "{challenges:?}");
        assert_eq!(challenges[0].param("realm"), Some("a,b,c"));
        assert_eq!(
            challenges[0].param("error_description"),
            Some("expired, please re-authenticate")
        );
    }

    #[test]
    fn bearer가_뒤에_있어도_대소문자_무시로_찾는다() {
        let header = r#"Basic realm="files", bearer scope="x", Digest realm="d""#;
        let challenges = parse_www_authenticate(header);
        assert_eq!(challenges.len(), 3, "{challenges:?}");
        let bearer = find_bearer_challenge(&challenges).unwrap();
        assert_eq!(bearer.scope(), Some("x"));
    }

    #[test]
    fn 파라미터_없는_챌린지와_따옴표_없는_값() {
        let challenges = parse_www_authenticate("Bearer");
        assert_eq!(challenges.len(), 1);
        assert_eq!(challenges[0].scheme, "Bearer");
        assert!(challenges[0].params.is_empty());

        let challenges = parse_www_authenticate("Bearer error=invalid_token, max_age=300");
        assert_eq!(challenges[0].error(), Some("invalid_token"));
        assert_eq!(challenges[0].param("max_age"), Some("300"));
    }

    #[test]
    fn 파라미터_키는_대소문자_무시로_조회() {
        let challenges = parse_www_authenticate(r#"Bearer Resource_Metadata="https://x/prm""#);
        assert_eq!(challenges[0].resource_metadata(), Some("https://x/prm"));
    }

    #[test]
    fn 기형_입력은_panic_없이_무시() {
        assert!(parse_www_authenticate("").is_empty());
        assert!(parse_www_authenticate(", , ,").is_empty());
        // 챌린지 없이 시작하는 param 조각은 버린다
        assert!(parse_www_authenticate(r#"realm="orphan""#).is_empty());
    }
}
