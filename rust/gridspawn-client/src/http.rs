//! The one request path: headers, envelopes and tracing. Retries are decided by
//! the callers (most client calls are single-use and never retried).

use std::{sync::Arc, time::Duration};

use reqwest::{
    header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE, RETRY_AFTER},
    Method, StatusCode,
};
use serde::de::DeserializeOwned;
use serde_json::Value;
use tracing::Instrument;
use url::Url;
use zeroize::Zeroizing;

use crate::error::{ApiError, Error, ErrorCode, Service};

const USER_AGENT: &str = concat!("gridspawn-client-rust/", env!("CARGO_PKG_VERSION"));

pub(crate) enum Body<'a> {
    None,
    Json(&'a Value),
    /// `application/x-www-form-urlencoded` pairs (OAuth endpoints).
    Form(&'a [(&'a str, &'a str)]),
}

pub(crate) struct Call<'a> {
    pub service: Service,
    pub method: Method,
    pub url: Url,
    /// A route template for logs (never ids, tokens or bodies).
    pub route: &'static str,
    pub bearer: Option<&'a str>,
    pub body: Body<'a>,
}

#[derive(Clone)]
pub(crate) struct Http {
    // The crate's own client: no default headers, so never an Origin header.
    client: reqwest::Client,
}

impl Http {
    pub fn new(timeout: Duration) -> Result<Self, Error> {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| Error::Network(Arc::new(error)))?;
        Ok(Self { client })
    }

    /// One attempt; a 2xx body is parsed as `T`.
    pub async fn send<T: DeserializeOwned>(&self, call: Call<'_>) -> Result<T, Error> {
        let body = self.send_raw(call).await?;
        serde_json::from_slice(&body)
            .map_err(|_| Error::InvalidResponse("the response body is not the expected JSON"))
    }

    /// One attempt; any 2xx is success and the body is ignored.
    pub async fn send_ignoring_body(&self, call: Call<'_>) -> Result<(), Error> {
        self.send_raw(call).await.map(|_| ())
    }

    async fn send_raw(&self, call: Call<'_>) -> Result<Vec<u8>, Error> {
        let span = tracing::debug_span!(
            "gridspawn.request",
            service = %call.service,
            route = call.route,
            status = tracing::field::Empty,
            request_id = tracing::field::Empty,
        );
        self.once(&call).instrument(span).await
    }

    async fn once(&self, call: &Call<'_>) -> Result<Vec<u8>, Error> {
        let mut request = self.client.request(call.method.clone(), call.url.clone());
        if let Some(token) = call.bearer {
            let header = Zeroizing::new(format!("Bearer {token}"));
            let mut value = HeaderValue::from_str(&header)
                .map_err(|_| Error::InvalidResponse("a token is not a valid header value"))?;
            value.set_sensitive(true);
            request = request.header(AUTHORIZATION, value);
        }
        match call.body {
            Body::None => {}
            Body::Json(body) => {
                let bytes = serde_json::to_vec(body).map_err(|_| Error::InvalidInput {
                    field: "body",
                    reason: "could not be encoded as JSON",
                })?;
                request = request.header(CONTENT_TYPE, "application/json").body(bytes);
            }
            Body::Form(pairs) => {
                let encoded = Zeroizing::new(
                    url::form_urlencoded::Serializer::new(String::new())
                        .extend_pairs(pairs.iter())
                        .finish(),
                );
                request = request
                    .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(encoded.as_bytes().to_vec());
            }
        }
        let response = request.send().await.map_err(transport)?;
        let status = response.status();
        let headers = response.headers().clone();
        let body = response.bytes().await.map_err(transport)?;
        let span = tracing::Span::current();
        span.record("status", status.as_u16());
        let header_request_id = headers
            .get("x-gridspawn-request-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        if let Some(id) = &header_request_id {
            span.record("request_id", id.as_str());
        }
        if status.is_success() {
            tracing::debug!("gridspawn request ok");
            return Ok(body.to_vec());
        }
        let error = api_error(call.service, status, &headers, &body, header_request_id);
        tracing::debug!(code = %error.wire_code, "gridspawn request refused");
        Err(Error::Api(Box::new(error)))
    }
}

fn transport(error: reqwest::Error) -> Error {
    if error.is_timeout() {
        Error::Timeout
    } else {
        Error::Network(Arc::new(error))
    }
}

/// Reads sdk-auth's `{"error": <oauth>, "error_description", "code": "auth/…", "requestId"}`,
/// the edge's `{"ok": false, "error": {"code", "message"}, "requestId"}`, a plain
/// OAuth `{"error": "…"}`, or anything else (proxy HTML) by status.
pub(crate) fn api_error(
    service: Service,
    status: StatusCode,
    headers: &HeaderMap,
    body: &[u8],
    header_request_id: Option<String>,
) -> ApiError {
    let text = |value: Option<&Value>| value.and_then(Value::as_str).map(str::to_owned);
    let (code, message, body_request_id) = match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(map)) => {
            let (code, message) = match (map.get("error"), text(map.get("code"))) {
                (Some(Value::Object(error)), _) => {
                    (text(error.get("code")), text(error.get("message")))
                }
                (_, Some(code)) => (Some(code), text(map.get("error_description"))),
                (Some(Value::String(error)), None) => {
                    (Some(error.clone()), text(map.get("error_description")))
                }
                _ => (None, None),
            };
            (code, message, text(map.get("requestId")))
        }
        _ => (None, None, None),
    };
    let wire_code = code.unwrap_or_else(|| {
        if status == StatusCode::TOO_MANY_REQUESTS {
            "rate_limited".to_owned()
        } else {
            format!("http_{}", status.as_u16())
        }
    });
    ApiError {
        service,
        status: status.as_u16(),
        code: ErrorCode::from_wire(&wire_code),
        wire_code,
        message,
        request_id: body_request_id.or(header_request_id),
        retry_after: headers
            .get(RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(Duration::from_secs),
    }
}

/// `base` + `path` (path already percent-encoded).
pub(crate) fn endpoint(base: &Url, path: &str) -> Url {
    Url::parse(&format!("{}{path}", base.as_str().trim_end_matches('/'))).expect("endpoint URL")
}

/// Percent-encodes one path segment.
pub(crate) fn segment(value: &str) -> String {
    value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(status: u16, body: &str) -> ApiError {
        api_error(
            Service::Auth,
            StatusCode::from_u16(status).unwrap(),
            &HeaderMap::new(),
            body.as_bytes(),
            Some("hdr".into()),
        )
    }

    #[test]
    fn envelopes() {
        let auth = parse(
            400,
            r#"{"error":"invalid_grant","error_description":"The authorization grant is invalid or expired.","code":"auth/invalid_grant","requestId":"r1"}"#,
        );
        assert_eq!(auth.code, ErrorCode::InvalidGrant);
        assert_eq!(auth.wire_code, "auth/invalid_grant");
        assert_eq!(auth.request_id.as_deref(), Some("r1"));
        assert!(auth.message.unwrap().contains("invalid or expired"));

        let edge = parse(
            401,
            r#"{"ok":false,"error":{"code":"session/ticket_invalid","message":"m"},"requestId":"r2"}"#,
        );
        assert_eq!(edge.code, ErrorCode::TicketInvalid);
        assert_eq!(edge.message.as_deref(), Some("m"));

        let oauth = parse(400, r#"{"error":"invalid_request"}"#);
        assert_eq!(oauth.wire_code, "invalid_request");

        let html = parse(502, "<html>bad gateway</html>");
        assert_eq!(html.wire_code, "http_502");
        assert_eq!(html.request_id.as_deref(), Some("hdr"));

        assert_eq!(parse(429, "").code, ErrorCode::RateLimited);
    }
}
