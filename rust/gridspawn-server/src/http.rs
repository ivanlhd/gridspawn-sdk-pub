//! The one request path: headers, envelopes, retries and tracing.

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

use crate::{
    credential::Credential,
    error::{ApiError, Error, ErrorCode, Service},
    types::RetryPolicy,
};

const USER_AGENT: &str = concat!("gridspawn-server-rust/", env!("CARGO_PKG_VERSION"));

pub(crate) enum Auth<'a> {
    None,
    Server(&'a Credential),
    Bearer(&'a str),
}

pub(crate) struct Call<'a> {
    pub service: Service,
    pub method: Method,
    pub url: Url,
    /// A route template for logs (never ids or bodies).
    pub route: &'static str,
    pub auth: Auth<'a>,
    pub body: Option<&'a Value>,
    pub idempotency_key: Option<&'a str>,
    /// Retry network errors, timeouts and 5xx per the policy.
    pub retry: bool,
}

#[derive(Clone)]
pub(crate) struct Http {
    // The crate's own client: no default headers, so never an Origin header.
    client: reqwest::Client,
    retry: RetryPolicy,
}

impl Http {
    pub fn new(timeout: Duration, retry: RetryPolicy) -> Result<Self, Error> {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| Error::Network(Arc::new(error)))?;
        Ok(Self { client, retry })
    }

    pub async fn send<T: DeserializeOwned>(&self, call: Call<'_>) -> Result<T, Error> {
        let attempts = if call.retry {
            self.retry.max_attempts.max(1)
        } else {
            1
        };
        let mut attempt = 1;
        loop {
            let span = tracing::debug_span!(
                "gridspawn.request",
                service = %call.service,
                route = call.route,
                attempt,
                status = tracing::field::Empty,
                request_id = tracing::field::Empty,
            );
            match self.once::<T>(&call).instrument(span.clone()).await {
                Ok(value) => return Ok(value),
                Err(error) if attempt < attempts && error.transport_retryable() => {
                    let wait = self.retry.backoff(attempt);
                    tracing::warn!(parent: &span, %error, ?wait, "gridspawn request failed; retrying");
                    tokio::time::sleep(wait).await;
                    attempt += 1;
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn once<T: DeserializeOwned>(&self, call: &Call<'_>) -> Result<T, Error> {
        let mut request = self.client.request(call.method.clone(), call.url.clone());
        match call.auth {
            Auth::None => {}
            Auth::Server(credential) => {
                request = request.header(AUTHORIZATION, credential.authorization())
            }
            Auth::Bearer(token) => {
                let header = Zeroizing::new(format!("Bearer {token}"));
                let mut value = HeaderValue::from_str(&header)
                    .map_err(|_| Error::InvalidResponse("the grant is not a valid header value"))?;
                value.set_sensitive(true);
                request = request.header(AUTHORIZATION, value);
            }
        }
        if let Some(key) = call.idempotency_key {
            request = request.header("Idempotency-Key", key);
        }
        if let Some(body) = call.body {
            let bytes = serde_json::to_vec(body).map_err(|_| Error::InvalidInput {
                field: "body",
                reason: "could not be encoded as JSON",
            })?;
            request = request.header(CONTENT_TYPE, "application/json").body(bytes);
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
            return serde_json::from_slice(&body)
                .map_err(|_| Error::InvalidResponse("the response body is not the expected JSON"));
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

/// Reads the edge envelope `{"ok":false,"error":{"code","message"},"requestId"}`,
/// Core's `{"error":"<code>"}`, or anything else (nginx HTML) by status.
fn api_error(
    service: Service,
    status: StatusCode,
    headers: &HeaderMap,
    body: &[u8],
    header_request_id: Option<String>,
) -> ApiError {
    let text = |value: Option<&Value>| value.and_then(Value::as_str).map(str::to_owned);
    let (code, message, body_request_id) = match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(map)) => {
            let (code, message) = match map.get("error") {
                Some(Value::Object(error)) => (text(error.get("code")), text(error.get("message"))),
                Some(Value::String(code)) => (Some(code.clone()), None),
                _ => (text(map.get("code")), None),
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
