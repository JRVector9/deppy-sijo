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

        let mut outgoing = match request.method().as_str() {
            "POST" => self.agent.post(&request.uri().to_string()),
            "GET" => self.agent.get(&request.uri().to_string()),
            _ => return Err(BoundedOAuthHttpError::InvalidRequest),
        };
        for (name, value) in request.headers() {
            let value = value
                .to_str()
                .map_err(|_| BoundedOAuthHttpError::InvalidRequest)?;
            outgoing = outgoing.set(name.as_str(), value);
        }

        let received = match request.method().as_str() {
            "POST" => outgoing.send_bytes(request.body()),
            "GET" => outgoing.call(),
            _ => unreachable!("method validated above"),
        };
        // Preserve OAuth error status responses for oauth2's typed error parser. Transport errors
        // are deliberately collapsed so URL/request bodies cannot leak through Error formatting.
        let response = match received {
            Ok(response) | Err(ureq::Error::Status(_, response)) => response,
            Err(_) => return Err(BoundedOAuthHttpError::RequestFailed),
        };
        let status = response.status();
        let content_type = response.header("Content-Type").map(str::to_owned);
        let body = read_bounded(response.into_reader())?;

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
    response: ureq::Response,
) -> Result<Vec<u8>, BoundedOAuthHttpError> {
    read_bounded(response.into_reader())
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
