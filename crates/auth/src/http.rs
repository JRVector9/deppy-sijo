//! Bounded synchronous OAuth HTTP transport.
//!
//! `oauth2`'s built-in `ureq::Agent` adapter reads token responses to EOF without a ceiling. This
//! adapter keeps the same blocking/redirect/timeout model while enforcing a fixed response budget
//! before handing bytes to `oauth2`.

use std::io::Read as _;
use std::time::Duration;

use oauth2::{HttpRequest, HttpResponse, SyncHttpClient};

#[cfg(test)]
thread_local! {
    static HTTP_CALL_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn reset_http_call_count() {
    HTTP_CALL_COUNT.with(|count| count.set(0));
}

#[cfg(test)]
pub(crate) fn http_call_count() -> usize {
    HTTP_CALL_COUNT.with(std::cell::Cell::get)
}

/// Maximum authorization-code, refresh, and dynamic-registration response body size.
pub const OAUTH_HTTP_RESPONSE_MAX_BYTES: usize = 1024 * 1024;

pub(crate) struct BoundedOAuthHttpClient {
    agent: ureq::Agent,
}

impl BoundedOAuthHttpClient {
    pub(crate) fn new(timeout: Duration) -> Self {
        Self {
            agent: crate::oauth_http_agent(timeout),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BoundedOAuthHttpError {
    InvalidRequest,
    RequestFailed,
    InvalidResponse,
    ResponseLimitExceeded,
}

impl std::fmt::Display for BoundedOAuthHttpError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::InvalidRequest => "OAuth HTTP request is invalid",
            Self::RequestFailed => "OAuth HTTP request failed",
            Self::InvalidResponse => "OAuth HTTP response is invalid",
            Self::ResponseLimitExceeded => "OAuth HTTP response exceeds its byte limit",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for BoundedOAuthHttpError {}

impl SyncHttpClient for BoundedOAuthHttpClient {
    type Error = BoundedOAuthHttpError;

    fn call(&self, request: HttpRequest) -> Result<HttpResponse, Self::Error> {
        #[cfg(test)]
        HTTP_CALL_COUNT.with(|count| count.set(count.get().saturating_add(1)));

        for value in request.headers().values() {
            value
                .to_str()
                .map_err(|_| BoundedOAuthHttpError::InvalidRequest)?;
        }
        // Status responses remain available to oauth2's typed error parser. The agent
        // disables redirects/status-as-error, and transport errors stay secret-free.
        let response = match request.method().as_str() {
            "POST" => self.agent.run(request),
            "GET" => self.agent.run(request.map(|_| ())),
            _ => return Err(BoundedOAuthHttpError::InvalidRequest),
        }
        .map_err(|_| BoundedOAuthHttpError::RequestFailed)?;
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get("Content-Type")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = read_bounded(response.into_body().into_reader())?;

        let mut builder = oauth2::http::Response::builder().status(status);
        if let Some(content_type) = content_type {
            builder = builder.header("Content-Type", content_type);
        }
        builder
            .body(body)
            .map_err(|_| BoundedOAuthHttpError::InvalidResponse)
    }
}

pub(crate) fn read_ureq_body_bounded(
    response: ureq::http::Response<ureq::Body>,
) -> Result<Vec<u8>, BoundedOAuthHttpError> {
    read_bounded(response.into_body().into_reader())
}

fn read_bounded(reader: impl std::io::Read) -> Result<Vec<u8>, BoundedOAuthHttpError> {
    let probe = OAUTH_HTTP_RESPONSE_MAX_BYTES
        .checked_add(1)
        .ok_or(BoundedOAuthHttpError::ResponseLimitExceeded)?;
    let mut body = Vec::new();
    reader
        .take(probe as u64)
        .read_to_end(&mut body)
        .map_err(|_| BoundedOAuthHttpError::InvalidResponse)?;
    if body.len() > OAUTH_HTTP_RESPONSE_MAX_BYTES {
        return Err(BoundedOAuthHttpError::ResponseLimitExceeded);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oauth_http_keeps_error_status_and_body_for_typed_parser() {
        use std::io::Write as _;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/token", listener.local_addr().unwrap());
        let body = br#"{"error":"invalid_grant"}"#;
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
                assert!(request.len() < 4096);
            }
            write!(stream, "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
            stream.write_all(body).unwrap();
        });
        let client = BoundedOAuthHttpClient::new(Duration::from_secs(2));
        let result = client.call(oauth2::http::Request::get(url).body(Vec::new()).unwrap());
        server.join().unwrap();
        let response = result.unwrap();
        assert_eq!(response.status().as_u16(), 400);
        assert_eq!(response.body(), body);
    }

    #[test]
    fn response_body_accepts_exact_limit_and_rejects_plus_one() {
        let exact = vec![b'x'; OAUTH_HTTP_RESPONSE_MAX_BYTES];
        assert_eq!(read_bounded(exact.as_slice()).unwrap().len(), exact.len());

        let plus_one = vec![b'x'; OAUTH_HTTP_RESPONSE_MAX_BYTES + 1];
        assert_eq!(
            read_bounded(plus_one.as_slice()).unwrap_err(),
            BoundedOAuthHttpError::ResponseLimitExceeded
        );
    }

    #[test]
    fn error_formatting_contains_no_dynamic_data() {
        for error in [
            BoundedOAuthHttpError::InvalidRequest,
            BoundedOAuthHttpError::RequestFailed,
            BoundedOAuthHttpError::InvalidResponse,
            BoundedOAuthHttpError::ResponseLimitExceeded,
        ] {
            let display = error.to_string();
            let debug = format!("{error:?}");
            assert!(!display.contains("http://"));
            assert!(!debug.contains("token-value"));
        }
    }
}
