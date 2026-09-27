//! Volatile public-client OAuth authorization code + S256 PKCE.
//! Only the local Deppy owner can approve a request. No browser auto-consent.
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use web_remote::http::{RequestHead, Response};
use zeroize::Zeroizing;

#[derive(Clone)]
pub struct Approval {
    pub id: String,
    pub client_name: String,
    pub input: bool,
    pub redirect_uri: String,
}
struct Client {
    name: String,
    redirects: Vec<String>,
    expires: u64,
    connection_expires: u64,
    approved: bool,
}
struct Flow {
    client: String,
    redirect: String,
    resource: String,
    state: String,
    challenge: String,
    input: bool,
    expires: u64,
}
struct Pending {
    flow: Flow,
    approved: Option<Zeroizing<String>>,
    denied: bool,
}
struct Access {
    input: bool,
    expires: u64,
    _lease: std::sync::Arc<secret::RedactionLease>,
}
struct Refresh {
    client: String,
    resource: String,
    input: bool,
    expires: u64,
    access_key: String,
    _lease: std::sync::Arc<secret::RedactionLease>,
}
pub(crate) struct OAuth {
    clients: HashMap<String, Client>,
    pending: HashMap<String, Pending>,
    codes: HashMap<String, Flow>,
    access: HashMap<String, Access>,
    refresh: HashMap<String, Refresh>,
    redaction: secret::RedactionService,
}
impl OAuth {
    pub fn new(redaction: secret::RedactionService) -> Self {
        Self {
            clients: HashMap::new(),
            pending: HashMap::new(),
            codes: HashMap::new(),
            access: HashMap::new(),
            refresh: HashMap::new(),
            redaction,
        }
    }
    pub fn clear(&mut self) {
        self.clients.clear();
        self.pending.clear();
        self.codes.clear();
        self.access.clear();
        self.refresh.clear();
    }
    fn prune(&mut self, now: u64) {
        self.pending.retain(|_, p| now < p.flow.expires);
        self.clients
            .retain(|id, c| now < c.expires || self.pending.values().any(|p| &p.flow.client == id));
        self.codes.retain(|_, f| now < f.expires);
        self.access.retain(|_, t| now < t.expires);
        self.refresh.retain(|_, t| now < t.expires);
    }
    #[cfg(test)]
    pub fn authenticate(&self, token: &str, now: u64) -> Option<bool> {
        self.authenticate_key(&hash(token), now)
    }
    pub fn authenticate_key(&self, key: &str, now: u64) -> Option<bool> {
        self.access
            .get(key)
            .filter(|a| now < a.expires)
            .map(|a| a.input)
    }
    pub fn approvals(&mut self, now: u64) -> Vec<Approval> {
        self.prune(now);
        self.pending
            .iter()
            .filter(|(_, p)| !p.denied && p.approved.is_none())
            .map(|(id, p)| Approval {
                id: id.clone(),
                client_name: self
                    .clients
                    .get(&p.flow.client)
                    .map(|c| c.name.clone())
                    .unwrap_or_default(),
                input: p.flow.input,
                redirect_uri: p.flow.redirect.clone(),
            })
            .collect()
    }
    pub fn approve(&mut self, id: &str, allow: bool, now: u64) -> bool {
        self.prune(now);
        let Some(p) = self
            .pending
            .get_mut(id)
            .filter(|p| p.approved.is_none() && !p.denied)
        else {
            return false;
        };
        if !allow {
            p.denied = true;
            p.flow.expires = (now + 60).min(
                self.clients
                    .get(&p.flow.client)
                    .map_or(now, |c| c.connection_expires),
            );
            return true;
        }
        if self.codes.len() >= 32 {
            return false;
        }
        let Some(client) = self.clients.get_mut(&p.flow.client) else {
            return false;
        };
        client.approved = true;
        client.expires = client.connection_expires;
        let code = token();
        p.flow.expires = (now + 60).min(client.expires);
        let flow = Flow {
            client: p.flow.client.clone(),
            redirect: p.flow.redirect.clone(),
            resource: p.flow.resource.clone(),
            state: String::new(),
            challenge: p.flow.challenge.clone(),
            input: p.flow.input,
            expires: p.flow.expires,
        };
        self.codes.insert(hash(&code), flow);
        p.approved = Some(code);
        true
    }
    pub fn route(
        &mut self,
        h: &RequestHead,
        body: &[u8],
        base: &str,
        now: u64,
        expires: u64,
        headers: &mut Vec<(String, String)>,
        wake: &dyn Fn(),
    ) -> Response {
        headers.push(("Cache-Control".into(), "no-store".into()));
        headers.push(("Referrer-Policy".into(), "no-referrer".into()));
        let resource = format!("{base}/mcp");
        if h.method == "GET"
            && matches!(
                h.path.as_str(),
                "/.well-known/oauth-protected-resource"
                    | "/.well-known/oauth-protected-resource/mcp"
            )
        {
            return response(
                json!({"resource":resource,"authorization_servers":[base],"scopes_supported":["deppy.read","deppy.input"],"bearer_methods_supported":["header"]}),
            );
        }
        if h.method == "GET" && h.path == "/.well-known/oauth-authorization-server" {
            return response(
                json!({"issuer":base,"authorization_endpoint":format!("{base}/oauth/authorize"),"token_endpoint":format!("{base}/oauth/token"),"registration_endpoint":format!("{base}/oauth/register"),"response_types_supported":["code"],"grant_types_supported":["authorization_code","refresh_token"],"token_endpoint_auth_methods_supported":["none"],"code_challenge_methods_supported":["S256"],"scopes_supported":["deppy.read","deppy.input"]}),
            );
        }
        self.prune(now);
        if now >= expires {
            return error(401, "connection_expired");
        }
        match (h.method.as_str(), h.path.as_str()) {
            ("POST", "/oauth/register") => {
                if h.header("content-type").and_then(|s| s.split(';').next())
                    != Some("application/json")
                {
                    return error(415, "invalid_client_metadata");
                }
                let Ok(v) = serde_json::from_slice::<Value>(body) else {
                    return error(400, "invalid_client_metadata");
                };
                if v["token_endpoint_auth_method"].as_str().unwrap_or("none") != "none"
                    || v.get("grant_types").is_some_and(|v| {
                        !v.as_array().is_some_and(|a| {
                            !a.is_empty()
                                && a.iter().all(|g| {
                                    matches!(
                                        g.as_str(),
                                        Some("authorization_code" | "refresh_token")
                                    )
                                })
                        })
                    })
                    || v.get("response_types")
                        .is_some_and(|v| v != &json!(["code"]))
                {
                    return error(400, "invalid_client_metadata");
                }
                let Some(uris) = v["redirect_uris"]
                    .as_array()
                    .filter(|a| !a.is_empty() && a.len() <= 4)
                else {
                    return error(400, "invalid_redirect_uri");
                };
                let Some(redirects) = uris
                    .iter()
                    .map(|v| v.as_str().filter(|s| valid_redirect(s)).map(str::to_owned))
                    .collect::<Option<Vec<_>>>()
                else {
                    return error(400, "invalid_redirect_uri");
                };
                if self.clients.len() >= 128 {
                    // Unused public registrations cannot monopolize the listener.
                    // Never evict approved clients or an in-progress consent flow.
                    let evict = self
                        .clients
                        .iter()
                        .filter(|(id, c)| {
                            !c.approved && !self.pending.values().any(|p| &p.flow.client == *id)
                        })
                        .min_by_key(|(_, c)| c.expires)
                        .map(|(id, _)| id.clone());
                    if let Some(id) = evict {
                        self.clients.remove(&id);
                    } else {
                        return error(503, "registration_capacity");
                    }
                }
                let name = v["client_name"].as_str().unwrap_or("MCP client");
                if name.is_empty() || name.len() > 128 || name.chars().any(char::is_control) {
                    return error(400, "invalid_client_metadata");
                }
                let id = uuid::Uuid::new_v4().to_string();
                self.clients.insert(
                    id.clone(),
                    Client {
                        name: name.into(),
                        redirects: redirects.clone(),
                        expires: (now + 300).min(expires),
                        connection_expires: expires,
                        approved: false,
                    },
                );
                let mut r = response(
                    json!({"client_id":id,"client_name":name,"redirect_uris":redirects,"token_endpoint_auth_method":"none","grant_types":["authorization_code","refresh_token"],"response_types":["code"]}),
                );
                r.status = 201;
                r
            }
            ("GET", "/oauth/authorize") => {
                let Ok(q) = form(h.query.as_bytes()) else {
                    return error(400, "invalid_request");
                };
                if let Some(id) = q.get("request") {
                    if q.len() != 1 {
                        return error(400, "invalid_request");
                    }
                    let Some(p) = self.pending.get(id) else {
                        return error(400, "authorization_expired");
                    };
                    if p.denied || p.approved.is_some() {
                        let location = redirect_location(
                            &p.flow.redirect,
                            &p.flow.state,
                            p.approved.as_deref().map(String::as_str),
                        );
                        if location.len() > web_remote::http::MAX_RESPONSE_HEADER_VALUE_BYTES {
                            return error(400, "invalid_redirect_size");
                        }
                        self.pending.remove(id);
                        headers.push(("Location".into(), location));
                        return Response::plain(303, "");
                    }
                    return waiting(id);
                }
                let Some(client) = q.get("client_id").and_then(|id| self.clients.get(id)) else {
                    return error(400, "invalid_client");
                };
                let redirect = q.get("redirect_uri").cloned().unwrap_or_default();
                let challenge = q.get("code_challenge").cloned().unwrap_or_default();
                if !client.redirects.contains(&redirect)
                    || q.get("response_type").map(String::as_str) != Some("code")
                    || q.get("resource") != Some(&resource)
                    || q.get("code_challenge_method").map(String::as_str) != Some("S256")
                    || challenge.len() != 43
                    || URL_SAFE_NO_PAD
                        .decode(&challenge)
                        .map_or(true, |b| b.len() != 32)
                {
                    return error(400, "invalid_request");
                }
                let scope = q.get("scope").map(String::as_str).unwrap_or("deppy.read");
                let scopes: Vec<_> = scope.split_whitespace().collect();
                if !scopes.contains(&"deppy.read")
                    || scopes
                        .iter()
                        .any(|s| !matches!(*s, "deppy.read" | "deppy.input"))
                {
                    return error(400, "invalid_scope");
                }
                let state = q.get("state").cloned().unwrap_or_default();
                // Codes are two simple UUIDs (64 ASCII bytes). Validate the final
                // encoded Location before asking the owner to approve this flow.
                if redirect_location(&redirect, &state, Some(&"0".repeat(64))).len()
                    > web_remote::http::MAX_RESPONSE_HEADER_VALUE_BYTES
                {
                    return error(400, "invalid_redirect_size");
                }
                if self.pending.len() >= 16 {
                    return error(503, "approval_capacity");
                }
                let id = token().to_string();
                self.pending.insert(
                    id.clone(),
                    Pending {
                        flow: Flow {
                            client: q["client_id"].clone(),
                            redirect,
                            resource,
                            state,
                            challenge,
                            input: scopes.contains(&"deppy.input"),
                            expires: (now + 300).min(expires),
                        },
                        approved: None,
                        denied: false,
                    },
                );
                wake();
                waiting(&id)
            }
            ("POST", "/oauth/token") => {
                if h.header("content-type").and_then(|s| s.split(';').next())
                    != Some("application/x-www-form-urlencoded")
                {
                    return error(415, "invalid_request");
                }
                let Ok(q) = form(body) else {
                    return error(400, "invalid_request");
                };
                let client = q.get("client_id").cloned().unwrap_or_default();
                if !self.clients.contains_key(&client) || q.get("resource") != Some(&resource) {
                    return error(400, "invalid_grant");
                }
                let (input, refresh_input, expiry, code_key, refresh_key, replaced_access) = match q
                    .get("grant_type")
                    .map(String::as_str)
                {
                    Some("authorization_code") => {
                        let key = hash(q.get("code").map(String::as_str).unwrap_or(""));
                        let Some(f) = self.codes.get(&key) else {
                            return error(400, "invalid_grant");
                        };
                        let verifier = q.get("code_verifier").map(String::as_str).unwrap_or("");
                        if !(43..=128).contains(&verifier.len())
                            || !verifier.bytes().all(|b| {
                                b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')
                            })
                            || URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
                                != f.challenge
                            || f.client != client
                            || q.get("redirect_uri") != Some(&f.redirect)
                            || f.resource != resource
                        {
                            return error(400, "invalid_grant");
                        }
                        if self.access.len() >= 32 || self.refresh.len() >= 32 {
                            return error(503, "token_capacity");
                        }
                        (f.input, f.input, expires, Some(key), None, None)
                    }
                    Some("refresh_token") => {
                        let key = hash(q.get("refresh_token").map(String::as_str).unwrap_or(""));
                        let Some(r) = self.refresh.get(&key) else {
                            return error(400, "invalid_grant");
                        };
                        if r.client != client || r.resource != resource {
                            return error(400, "invalid_grant");
                        }
                        let input = match q.get("scope") {
                            None => r.input,
                            Some(scope) => {
                                let scopes: Vec<_> = scope.split_whitespace().collect();
                                if !scopes.contains(&"deppy.read")
                                    || scopes
                                        .iter()
                                        .any(|s| !matches!(*s, "deppy.read" | "deppy.input"))
                                    || (!r.input && scopes.contains(&"deppy.input"))
                                {
                                    return error(400, "invalid_scope");
                                }
                                scopes.contains(&"deppy.input")
                            }
                        };
                        if self.access.len() - usize::from(self.access.contains_key(&r.access_key))
                            >= 32
                        {
                            return error(503, "token_capacity");
                        }
                        (
                            input,
                            r.input,
                            r.expires,
                            None,
                            Some(key),
                            Some(r.access_key.clone()),
                        )
                    }
                    _ => return error(400, "unsupported_grant_type"),
                };
                let access = token();
                let refresh = token();
                let access_secret = secret::SecretString::new(access.to_string());
                let refresh_secret = secret::SecretString::new(refresh.to_string());
                // Register both tokens atomically before consuming any live grant.
                // On capacity failure the same code/refresh token remains retryable.
                let Ok(lease) = self
                    .redaction
                    .acquire_execution_lease(&[&access_secret, &refresh_secret])
                else {
                    return error(503, "token_capacity");
                };
                let lease = std::sync::Arc::new(lease);
                if let Some(key) = code_key {
                    self.codes.remove(&key);
                }
                if let Some(key) = refresh_key {
                    self.refresh.remove(&key);
                }
                if let Some(key) = replaced_access {
                    self.access.remove(&key);
                }
                let ttl = 3600.min(expiry.saturating_sub(now));
                self.access.insert(
                    hash(&access),
                    Access {
                        input,
                        expires: now + ttl,
                        _lease: lease.clone(),
                    },
                );
                self.refresh.insert(
                    hash(&refresh),
                    Refresh {
                        client,
                        resource,
                        input: refresh_input,
                        expires: expiry,
                        access_key: hash(&access),
                        _lease: lease,
                    },
                );
                response(
                    json!({"access_token":access.as_str(),"token_type":"Bearer","expires_in":ttl,"refresh_token":refresh.as_str(),"scope":if input {"deppy.read deppy.input"}else{"deppy.read"}}),
                )
            }
            _ => error(404, "not_found"),
        }
    }
}
fn redirect_location(redirect: &str, state: &str, code: Option<&str>) -> String {
    let mut url = url::Url::parse(redirect).expect("registered redirect");
    {
        let mut query = url.query_pairs_mut();
        query.append_pair(
            if code.is_some() { "code" } else { "error" },
            code.unwrap_or("access_denied"),
        );
        query.append_pair("state", state);
    }
    url.into()
}
fn token() -> Zeroizing<String> {
    Zeroizing::new(format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    ))
}
pub(crate) fn hash(s: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(s.as_bytes()))
}
fn valid_redirect(s: &str) -> bool {
    s.len() <= 2048
        && url::Url::parse(s).is_ok_and(|u| {
            u.fragment().is_none()
                && u.username().is_empty()
                && u.password().is_none()
                && u.host_str().is_some()
                && (u.scheme() == "https"
                    || (u.scheme() == "http"
                        && matches!(u.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))))
        })
}
fn form(b: &[u8]) -> Result<HashMap<String, String>, ()> {
    if b.len() > 8192 {
        return Err(());
    }
    let mut map = HashMap::new();
    for (k, v) in url::form_urlencoded::parse(b) {
        if k.len() > 64
            || v.len() > 2048
            || map.len() >= 16
            || map.insert(k.into_owned(), v.into_owned()).is_some()
        {
            return Err(());
        }
    }
    Ok(map)
}
fn response(v: Value) -> Response {
    Response {
        status: 200,
        content_type: "application/json",
        body: std::borrow::Cow::Owned(v.to_string().into_bytes()),
    }
}
fn error(status: u16, code: &str) -> Response {
    let mut r = response(json!({"error":code}));
    r.status = status;
    r
}
fn waiting(id: &str) -> Response {
    Response {status:200,content_type:"text/html; charset=utf-8",body:std::borrow::Cow::Owned(format!("<!doctype html><meta charset=utf-8><meta http-equiv=refresh content=\"2;url=/oauth/authorize?request={id}\"><title>Deppy authorization</title><h1>Approve in Deppy</h1><p>Open Settings → Cloud agents on your Mac and approve or deny this connector. This page will continue after your decision.</p>").into_bytes())}
}

#[cfg(test)]
mod tests {
    use super::*;
    fn head(method: &str, path: &str, query: &str, content_type: &str) -> RequestHead {
        let raw = format!(
            "{method} {path}{} HTTP/1.1\r\nHost: localhost\r\nContent-Type: {content_type}\r\n\r\n",
            if query.is_empty() {
                String::new()
            } else {
                format!("?{query}")
            }
        );
        web_remote::http::read_request_head(&mut std::io::BufReader::new(raw.as_bytes())).unwrap()
    }
    fn route(
        o: &mut OAuth,
        method: &str,
        path: &str,
        query: &str,
        body: &str,
        now: u64,
    ) -> (Response, Vec<(String, String)>) {
        let mut h = Vec::new();
        let r = o.route(
            &head(
                method,
                path,
                query,
                if path == "/oauth/register" {
                    "application/json"
                } else {
                    "application/x-www-form-urlencoded"
                },
            ),
            body.as_bytes(),
            "https://deppy.example",
            now,
            100000,
            &mut h,
            &|| {},
        );
        (r, h)
    }
    fn json_body(r: Response) -> Value {
        serde_json::from_slice(&r.body).unwrap()
    }
    fn issue(o: &mut OAuth, client: &str, id: &str) -> Value {
        issue_scoped(o, client, id, false)
    }
    fn issue_scoped(o: &mut OAuth, client: &str, id: &str, input: bool) -> Value {
        let verifier = "x".repeat(43);
        o.codes.insert(
            hash(id),
            Flow {
                client: client.into(),
                redirect: "https://client.example/callback".into(),
                resource: "https://deppy.example/mcp".into(),
                state: String::new(),
                challenge: URL_SAFE_NO_PAD.encode(Sha256::digest(&verifier)),
                input,
                expires: 200,
            },
        );
        let f = format!(
            "grant_type=authorization_code&client_id={client}&code={id}&redirect_uri=https%3A%2F%2Fclient.example%2Fcallback&resource=https%3A%2F%2Fdeppy.example%2Fmcp&code_verifier={verifier}"
        );
        json_body(route(o, "POST", "/oauth/token", "", &f, 102).0)
    }
    fn registered(o: &mut OAuth) -> String {
        json_body(
            route(
                o,
                "POST",
                "/oauth/register",
                "",
                r#"{"redirect_uris":["https://client.example/callback"]}"#,
                100,
            )
            .0,
        )["client_id"]
            .as_str()
            .unwrap()
            .to_owned()
    }
    #[test]
    fn redirect_bounds_reject_before_consent_and_allow_encoded_state() {
        let mut o = OAuth::new(secret::RedactionService::new());
        let prefix = "https://client.example/";
        let redirect = format!("{prefix}{}", "x".repeat(2048 - prefix.len()));
        let registration =
            json!({"redirect_uris":[redirect,"https://client.example/callback"]}).to_string();
        let client = json_body(route(&mut o, "POST", "/oauth/register", "", &registration, 100).0)
            ["client_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest("x".repeat(43)));
        let state = "~".repeat(2048);
        let query = |redirect: &str| {
            format!(
                "client_id={client}&redirect_uri={redirect}&resource=https://deppy.example/mcp&response_type=code&code_challenge_method=S256&code_challenge={challenge}&state={state}"
            )
        };
        assert_eq!(
            route(
                &mut o,
                "GET",
                "/oauth/authorize",
                &query(&redirect),
                "",
                101
            )
            .0
            .status,
            400
        );
        assert!(
            o.approvals(101).is_empty(),
            "oversized redirects must not solicit consent"
        );
        for (allow, time) in [(true, 102), (false, 104)] {
            assert_eq!(
                route(
                    &mut o,
                    "GET",
                    "/oauth/authorize",
                    &query("https://client.example/callback"),
                    "",
                    time
                )
                .0
                .status,
                200
            );
            let approval = o.approvals(time).pop().unwrap();
            assert!(o.approve(&approval.id, allow, time));
            let (r, h) = route(
                &mut o,
                "GET",
                "/oauth/authorize",
                &format!("request={}", approval.id),
                "",
                time + 1,
            );
            assert_eq!(r.status, 303);
            let location = &h.iter().find(|(k, _)| k == "Location").unwrap().1;
            let parsed = url::Url::parse(location).unwrap();
            assert!(
                parsed
                    .query_pairs()
                    .any(|(k, v)| k == "state" && v == state)
            );
            assert!(
                parsed
                    .query_pairs()
                    .any(|(k, _)| k == if allow { "code" } else { "error" })
            );
            let mut response = Vec::new();
            web_remote::http::write_response_with_headers(&mut response, &r, &h).unwrap();
            assert!(response.starts_with(b"HTTP/1.1 303"));
        }
    }
    #[test]
    fn refresh_scope_order_and_subset_preserve_original_refresh_grant() {
        let mut o = OAuth::new(secret::RedactionService::new());
        let client = registered(&mut o);
        let mut tokens = issue_scoped(&mut o, &client, "input-code", true);
        for (scope, time, input) in [
            (Some("deppy.input deppy.read"), 103, true),
            (Some("deppy.read"), 104, false),
            (None, 105, true),
        ] {
            let mut form = url::form_urlencoded::Serializer::new(String::new());
            form.extend_pairs([
                ("grant_type", "refresh_token"),
                ("client_id", &client),
                ("resource", "https://deppy.example/mcp"),
                ("refresh_token", tokens["refresh_token"].as_str().unwrap()),
            ]);
            if let Some(scope) = scope {
                form.append_pair("scope", scope);
            }
            let (r, _) = route(&mut o, "POST", "/oauth/token", "", &form.finish(), time);
            assert_eq!(r.status, 200, "valid refresh scope {scope:?}");
            tokens = json_body(r);
            assert_eq!(
                o.authenticate(tokens["access_token"].as_str().unwrap(), time),
                Some(input)
            );
            assert_eq!(
                tokens["scope"],
                if input {
                    "deppy.read deppy.input"
                } else {
                    "deppy.read"
                }
            );
        }
    }
    #[test]
    fn refresh_scope_rejects_expansion_and_unknown_without_consuming_grant() {
        let mut o = OAuth::new(secret::RedactionService::new());
        let client = registered(&mut o);
        let tokens = issue(&mut o, &client, "read-code");
        for scope in [
            "deppy.read deppy.input",
            "deppy.read unknown",
            "deppy.input",
            "",
        ] {
            let form = url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs([
                    ("grant_type", "refresh_token"),
                    ("client_id", &client),
                    ("resource", "https://deppy.example/mcp"),
                    ("refresh_token", tokens["refresh_token"].as_str().unwrap()),
                    ("scope", scope),
                ])
                .finish();
            assert_eq!(
                route(&mut o, "POST", "/oauth/token", "", &form, 103)
                    .0
                    .status,
                400
            );
            assert!(
                o.refresh
                    .contains_key(&hash(tokens["refresh_token"].as_str().unwrap()))
            );
        }
    }
    #[test]
    fn refresh_replaces_its_access_slot_at_full_capacity() {
        let mut o = OAuth::new(secret::RedactionService::new());
        let client = registered(&mut o);
        let mut tokens = Value::Null;
        for n in 0..32 {
            tokens = issue(&mut o, &client, &format!("code-{n}"));
            assert!(tokens["access_token"].is_string());
        }
        assert_eq!(o.access.len(), 32);
        let f = format!(
            "grant_type=refresh_token&client_id={client}&refresh_token={}&resource=https%3A%2F%2Fdeppy.example%2Fmcp",
            tokens["refresh_token"].as_str().unwrap()
        );
        assert_eq!(
            route(&mut o, "POST", "/oauth/token", "", &f, 103).0.status,
            200
        );
        assert_eq!(o.access.len(), 32);
        assert_eq!(o.refresh.len(), 32);
    }
    #[test]
    fn failed_secret_registration_preserves_code_refresh_and_existing_access() {
        struct Clock;
        impl secret::RedactionClock for Clock {
            fn now(&self) -> std::time::Duration {
                std::time::Duration::ZERO
            }
        }
        let mut o = OAuth::new(secret::RedactionService::new());
        let client = registered(&mut o);
        let tokens = issue(&mut o, &client, "first");
        let access = tokens["access_token"].as_str().unwrap();
        let refresh = tokens["refresh_token"].as_str().unwrap();
        o.redaction = secret::RedactionService::with_clock(
            secret::RedactionCorpusLimits {
                max_items: 1,
                max_bytes: 1,
            },
            std::time::Duration::ZERO,
            std::sync::Arc::new(Clock),
        )
        .unwrap();
        let f = format!(
            "grant_type=refresh_token&client_id={client}&refresh_token={refresh}&resource=https%3A%2F%2Fdeppy.example%2Fmcp"
        );
        assert_eq!(
            route(&mut o, "POST", "/oauth/token", "", &f, 103).0.status,
            503
        );
        assert!(o.refresh.contains_key(&hash(refresh)));
        assert_eq!(o.authenticate(access, 103), Some(false));
        assert!(issue(&mut o, &client, "second")["error"].is_string());
        assert!(o.codes.contains_key(&hash("second")));
        o.redaction = secret::RedactionService::new();
        assert_eq!(
            route(&mut o, "POST", "/oauth/token", "", &f, 104).0.status,
            200
        );
    }
    #[test]
    fn late_authorize_preserves_registration_and_denial_redirect_grace() {
        let mut o = OAuth::new(secret::RedactionService::new());
        let client = json_body(
            route(
                &mut o,
                "POST",
                "/oauth/register",
                "",
                r#"{"redirect_uris":["https://client.example/callback"]}"#,
                100,
            )
            .0,
        )["client_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest("x".repeat(43)));
        let q = format!(
            "client_id={client}&redirect_uri=https%3A%2F%2Fclient.example%2Fcallback&resource=https%3A%2F%2Fdeppy.example%2Fmcp&response_type=code&code_challenge_method=S256&code_challenge={challenge}"
        );
        route(&mut o, "GET", "/oauth/authorize", &q, "", 399);
        let id = o.approvals(401)[0].id.clone();
        assert!(o.approve(&id, true, 402));
        assert_eq!(
            route(
                &mut o,
                "GET",
                "/oauth/authorize",
                &format!("request={id}"),
                "",
                404
            )
            .0
            .status,
            303
        );
        route(&mut o, "GET", "/oauth/authorize", &q, "", 405);
        let id = o.approvals(405)[0].id.clone();
        assert!(o.approve(&id, false, 704));
        let (r, h) = route(
            &mut o,
            "GET",
            "/oauth/authorize",
            &format!("request={id}"),
            "",
            706,
        );
        assert_eq!(r.status, 303);
        assert!(
            h.iter()
                .any(|(k, v)| k == "Location" && v.contains("access_denied"))
        );
    }
    #[test]
    fn approval_near_deadline_denial_and_registration_flood_remain_bounded() {
        let mut o = OAuth::new(secret::RedactionService::new());
        let register =
            r#"{"client_name":"Bot","redirect_uris":["https://client.example/callback"]}"#;
        let client =
            json_body(route(&mut o, "POST", "/oauth/register", "", register, 100).0)["client_id"]
                .as_str()
                .unwrap()
                .to_owned();
        let verifier = "x".repeat(43);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(&verifier));
        let query = format!(
            "client_id={client}&redirect_uri=https%3A%2F%2Fclient.example%2Fcallback&resource=https%3A%2F%2Fdeppy.example%2Fmcp&response_type=code&code_challenge_method=S256&code_challenge={challenge}"
        );
        route(&mut o, "GET", "/oauth/authorize", &query, "", 100);
        let id = o.approvals(100)[0].id.clone();
        assert_eq!(
            o.approvals(100)[0].redirect_uri,
            "https://client.example/callback"
        );
        assert!(o.approve(&id, true, 399));
        assert_eq!(
            route(
                &mut o,
                "GET",
                "/oauth/authorize",
                &format!("request={id}"),
                "",
                401
            )
            .0
            .status,
            303
        );
        route(&mut o, "GET", "/oauth/authorize", &query, "", 402);
        let denied = o.approvals(402)[0].id.clone();
        assert!(o.approve(&denied, false, 402));
        let (r, h) = route(
            &mut o,
            "GET",
            "/oauth/authorize",
            &format!("request={denied}"),
            "",
            403,
        );
        assert_eq!(r.status, 303);
        assert!(
            h.iter()
                .any(|(k, v)| k == "Location" && v.contains("access_denied"))
        );
        for _ in 0..200 {
            assert_eq!(
                route(&mut o, "POST", "/oauth/register", "", register, 404)
                    .0
                    .status,
                201
            );
        }
        assert_eq!(o.clients.len(), 128);
        assert!(o.clients.contains_key(&client));
        o.prune(705);
        assert_eq!(o.clients.len(), 1);
        assert!(o.approvals(705).is_empty());
        assert!(!valid_redirect("https://client.example/callback#fragment"));
        assert!(!valid_redirect("http://evil.example/callback"));
        assert!(form(b"state=a&state=b").is_err());
    }
    #[test]
    fn oauth_requires_owner_consent_pkce_binding_single_use_refresh_and_expiry() {
        let mut o = OAuth::new(secret::RedactionService::new());
        let client = json_body(
            route(
                &mut o,
                "POST",
                "/oauth/register",
                "",
                r#"{"client_name":"Test Bot","redirect_uris":["https://client.example/callback"]}"#,
                100,
            )
            .0,
        )["client_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let verifier = "x".repeat(43);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(&verifier));
        let q = format!(
            "client_id={client}&redirect_uri=https%3A%2F%2Fclient.example%2Fcallback&resource=https%3A%2F%2Fdeppy.example%2Fmcp&response_type=code&code_challenge_method=S256&code_challenge={challenge}&state=return-state&scope=deppy.read%20deppy.input"
        );
        assert_eq!(
            route(
                &mut o,
                "GET",
                "/oauth/authorize",
                &q.replace("S256", "plain"),
                "",
                100
            )
            .0
            .status,
            400
        );
        assert_eq!(
            route(
                &mut o,
                "GET",
                "/oauth/authorize",
                &q.replace("client.example", "evil.example"),
                "",
                100
            )
            .0
            .status,
            400
        );
        assert_eq!(
            route(&mut o, "GET", "/oauth/authorize", &q, "", 100)
                .0
                .status,
            200
        );
        let pending = o.approvals(100);
        assert_eq!(pending.len(), 1);
        assert!(pending[0].input);
        assert_eq!(
            route(
                &mut o,
                "GET",
                "/oauth/authorize",
                &format!("request={}", pending[0].id),
                "",
                100
            )
            .0
            .status,
            200
        );
        assert!(o.codes.is_empty());
        assert!(o.approve(&pending[0].id, true, 101));
        let (r, h) = route(
            &mut o,
            "GET",
            "/oauth/authorize",
            &format!("request={}", pending[0].id),
            "",
            102,
        );
        assert_eq!(r.status, 303);
        let redirect =
            url::Url::parse(&h.iter().find(|(k, _)| k == "Location").unwrap().1).unwrap();
        let pairs: HashMap<_, _> = redirect.query_pairs().into_owned().collect();
        assert_eq!(pairs["state"], "return-state");
        let f = format!(
            "grant_type=authorization_code&client_id={client}&code={}&redirect_uri=https%3A%2F%2Fclient.example%2Fcallback&resource=https%3A%2F%2Fdeppy.example%2Fmcp&code_verifier={verifier}",
            pairs["code"]
        );
        assert_eq!(
            route(
                &mut o,
                "POST",
                "/oauth/token",
                "",
                &f.replace(&verifier, &"y".repeat(43)),
                102
            )
            .0
            .status,
            400
        );
        let tokens = json_body(route(&mut o, "POST", "/oauth/token", "", &f, 102).0);
        assert_eq!(
            o.authenticate(tokens["access_token"].as_str().unwrap(), 103),
            Some(true)
        );
        assert_eq!(
            route(&mut o, "POST", "/oauth/token", "", &f, 102).0.status,
            400
        );
        let refresh = format!(
            "grant_type=refresh_token&client_id={client}&refresh_token={}&resource=https%3A%2F%2Fdeppy.example%2Fmcp",
            tokens["refresh_token"].as_str().unwrap()
        );
        assert!(
            json_body(route(&mut o, "POST", "/oauth/token", "", &refresh, 103).0)["access_token"]
                .is_string()
        );
        assert_eq!(
            route(&mut o, "POST", "/oauth/token", "", &refresh, 103)
                .0
                .status,
            400
        );
        assert!(
            o.authenticate(tokens["access_token"].as_str().unwrap(), 3702)
                .is_none()
        );
        o.clear();
        assert!(
            o.authenticate(tokens["access_token"].as_str().unwrap(), 104)
                .is_none()
        );
    }
}
