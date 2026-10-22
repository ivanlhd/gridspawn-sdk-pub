use std::{fmt, sync::Arc, time::Duration};

use crate::storage::StoreError;

/// Everything that can go wrong. `Api` carries the service's own answer.
#[derive(Clone, Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Bad local input (app id, scopes, challenge, ticket shape). Nothing was sent.
    #[error("invalid {field}: {reason}")]
    InvalidInput {
        field: &'static str,
        reason: &'static str,
    },
    /// No session is held: call `restore` or `sign_in` first.
    #[error("no player is signed in")]
    SignInRequired,
    /// The local part of the browser sign-in failed.
    #[error("sign-in failed: {0}")]
    SignIn(SignInError),
    /// The token store failed.
    #[error("token store: {0}")]
    Store(#[source] Arc<StoreError>),
    /// Connect, TLS or read failure.
    #[error("network error")]
    Network(#[source] Arc<reqwest::Error>),
    #[error("request timed out")]
    Timeout,
    /// The service answered something the protocol does not allow.
    #[error("invalid response: {0}")]
    InvalidResponse(&'static str),
    /// Bootstrap did not match the app, the environment or protocol 1.
    #[error("bootstrap mismatch: {0}")]
    Bootstrap(&'static str),
    /// A documented error answer from sdk-auth or the SDK edge.
    /// Boxed to keep `Result<_, Error>` small.
    #[error("{0}")]
    Api(Box<ApiError>),
}

impl Error {
    /// Temporary: network errors, timeouts, and answers with status 5xx or 429.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Network(_) | Self::Timeout => true,
            Self::Api(error) => error.status == 429 || error.status >= 500,
            _ => false,
        }
    }

    pub fn api(&self) -> Option<&ApiError> {
        match self {
            Self::Api(error) => Some(error.as_ref()),
            _ => None,
        }
    }

    /// A stable code for game UI and logs: the wire code for service answers
    /// (`auth/invalid_grant`, `session/ticket_invalid`, …), otherwise a local
    /// one (`sign_in/cancelled`, `network`, `timeout`, …). Never contains secrets.
    pub fn code(&self) -> String {
        match self {
            Self::InvalidInput { .. } => "invalid_input".into(),
            Self::SignInRequired => "sign_in/required".into(),
            Self::SignIn(error) => error.code(),
            Self::Store(_) => "store".into(),
            Self::Network(_) => "network".into(),
            Self::Timeout => "timeout".into(),
            Self::InvalidResponse(_) => "invalid_response".into(),
            Self::Bootstrap(_) => "bootstrap".into(),
            Self::Api(error) => error.wire_code.clone(),
        }
    }

    /// What the crate retries by itself where it retries at all (429 is left to the caller).
    pub(crate) fn transport_retryable(&self) -> bool {
        match self {
            Self::Network(_) | Self::Timeout => true,
            Self::Api(error) => error.status >= 500,
            _ => false,
        }
    }

    pub(crate) fn api_code(&self) -> Option<&ErrorCode> {
        self.api().map(|error| &error.code)
    }
}

impl From<StoreError> for Error {
    fn from(error: StoreError) -> Self {
        Self::Store(Arc::new(error))
    }
}

impl From<SignInError> for Error {
    fn from(error: SignInError) -> Self {
        Self::SignIn(error)
    }
}

/// The local part of the loopback sign-in (protocol 01 §1).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SignInError {
    /// `PendingSignIn::cancel`, or the flow was dropped.
    #[error("cancelled")]
    Cancelled,
    /// No callback within `SignIn::timeout`.
    #[error("timed out waiting for the browser")]
    TimedOut,
    /// No system browser could be opened; use `begin_sign_in` and show the URL.
    #[error("no browser could be opened")]
    BrowserUnavailable,
    /// Could not listen on 127.0.0.1.
    #[error("could not listen on 127.0.0.1")]
    ListenerFailed,
    /// The callback's `state` is not the one sent.
    #[error("the callback state does not match")]
    StateMismatch,
    /// The callback did not come to the redirect URI that was sent.
    #[error("the callback does not match the redirect URI")]
    RedirectMismatch,
    /// The callback has neither a code nor an error.
    #[error("the callback has no code")]
    MissingCode,
    /// `?error=<oauth error>` on the callback, e.g. `access_denied`.
    #[error("authorization was not approved ({0})")]
    Denied(String),
}

impl SignInError {
    /// `sign_in/cancelled`, …; `Denied` gives the OAuth error as `auth/<error>`.
    pub fn code(&self) -> String {
        match self {
            Self::Cancelled => "sign_in/cancelled".into(),
            Self::TimedOut => "sign_in/timed_out".into(),
            Self::BrowserUnavailable => "sign_in/browser_unavailable".into(),
            Self::ListenerFailed => "sign_in/listener_failed".into(),
            Self::StateMismatch => "sign_in/state_mismatch".into(),
            Self::RedirectMismatch => "sign_in/redirect_mismatch".into(),
            Self::MissingCode => "sign_in/missing_code".into(),
            Self::Denied(error) => format!("auth/{error}"),
        }
    }
}

/// Which service answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Service {
    /// sdk-auth: `/oauth/*` and `/v1/auth/player-assertions` (protocol 01).
    Auth,
    /// The SDK edge: bootstrap, attach and grants (protocol 02).
    Edge,
}

impl fmt::Display for Service {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Auth => "auth",
            Self::Edge => "edge",
        })
    }
}

/// An error answer: the HTTP status and the documented code.
#[derive(Clone, Debug, thiserror::Error)]
#[error("{service} {status} {wire_code}")]
#[non_exhaustive]
pub struct ApiError {
    pub service: Service,
    pub status: u16,
    pub code: ErrorCode,
    /// The code exactly as the service sent it (`http_<status>` when the body had none).
    pub wire_code: String,
    /// The service's human-readable message (`error_description` on sdk-auth).
    pub message: Option<String>,
    pub request_id: Option<String>,
    /// From `Retry-After` when present.
    pub retry_after: Option<Duration>,
}

/// The documented error codes of protocol 01 (sdk-auth) and 02 (edge).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorCode {
    /// `auth/invalid_request`.
    InvalidRequest,
    /// `auth/invalid_client`: the app or environment is unknown, off, or has no
    /// client settings (redirect URIs, scopes) for sign-in.
    InvalidClient,
    /// `auth/invalid_grant`: code expired or reused; refresh token revoked or expired.
    InvalidGrant,
    /// `auth/invalid_scope`.
    InvalidScope,
    /// `auth/access_denied`.
    AccessDenied,
    /// `auth/login_required`.
    LoginRequired,
    /// `auth/account_setup_required`.
    AccountSetupRequired,
    /// `auth/rate_limited`, or any 429 without a code.
    RateLimited,
    /// `service/not_ready`.
    ServiceNotReady,
    /// `session/ticket_invalid`: expired (120 s), reused or forged.
    TicketInvalid,
    /// `session/grant_invalid`.
    GrantInvalid,
    /// `session/not_found`.
    SessionNotFound,
    /// `session/capability_denied`.
    CapabilityDenied,
    /// `session/invalid_request`.
    SessionInvalidRequest,
    /// `config/app_not_found`.
    AppNotFound,
    /// `config/protocol_incompatible`.
    ProtocolIncompatible,
    /// Any other code, kept verbatim.
    Other(String),
}

impl ErrorCode {
    pub(crate) fn from_wire(code: &str) -> Self {
        match code {
            "auth/invalid_request" => Self::InvalidRequest,
            "auth/invalid_client" => Self::InvalidClient,
            "auth/invalid_grant" => Self::InvalidGrant,
            "auth/invalid_scope" => Self::InvalidScope,
            "auth/access_denied" => Self::AccessDenied,
            "auth/login_required" => Self::LoginRequired,
            "auth/account_setup_required" => Self::AccountSetupRequired,
            "auth/rate_limited" | "rate_limited" => Self::RateLimited,
            "service/not_ready" => Self::ServiceNotReady,
            "session/ticket_invalid" => Self::TicketInvalid,
            "session/grant_invalid" => Self::GrantInvalid,
            "session/not_found" => Self::SessionNotFound,
            "session/capability_denied" => Self::CapabilityDenied,
            "session/invalid_request" => Self::SessionInvalidRequest,
            "config/app_not_found" => Self::AppNotFound,
            "config/protocol_incompatible" => Self::ProtocolIncompatible,
            other => Self::Other(other.to_owned()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::InvalidRequest => "auth/invalid_request",
            Self::InvalidClient => "auth/invalid_client",
            Self::InvalidGrant => "auth/invalid_grant",
            Self::InvalidScope => "auth/invalid_scope",
            Self::AccessDenied => "auth/access_denied",
            Self::LoginRequired => "auth/login_required",
            Self::AccountSetupRequired => "auth/account_setup_required",
            Self::RateLimited => "auth/rate_limited",
            Self::ServiceNotReady => "service/not_ready",
            Self::TicketInvalid => "session/ticket_invalid",
            Self::GrantInvalid => "session/grant_invalid",
            Self::SessionNotFound => "session/not_found",
            Self::CapabilityDenied => "session/capability_denied",
            Self::SessionInvalidRequest => "session/invalid_request",
            Self::AppNotFound => "config/app_not_found",
            Self::ProtocolIncompatible => "config/protocol_incompatible",
            Self::Other(code) => code,
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_codes_map_both_ways() {
        for code in [
            "auth/invalid_request",
            "auth/invalid_client",
            "auth/invalid_grant",
            "auth/invalid_scope",
            "auth/access_denied",
            "auth/login_required",
            "auth/account_setup_required",
            "auth/rate_limited",
            "service/not_ready",
            "session/ticket_invalid",
            "session/grant_invalid",
            "session/not_found",
            "session/capability_denied",
            "session/invalid_request",
            "config/app_not_found",
            "config/protocol_incompatible",
        ] {
            let mapped = ErrorCode::from_wire(code);
            assert!(!matches!(mapped, ErrorCode::Other(_)), "{code}");
            assert_eq!(mapped.as_str(), code);
        }
        assert_eq!(ErrorCode::from_wire("rate_limited"), ErrorCode::RateLimited);
        assert_eq!(
            ErrorCode::from_wire("config/origin_not_allowed"),
            ErrorCode::Other("config/origin_not_allowed".into())
        );
    }

    #[test]
    fn local_codes() {
        assert_eq!(
            Error::SignIn(SignInError::Cancelled).code(),
            "sign_in/cancelled"
        );
        assert_eq!(
            Error::SignIn(SignInError::Denied("access_denied".into())).code(),
            "auth/access_denied"
        );
        assert_eq!(Error::Timeout.code(), "timeout");
        assert_eq!(Error::SignInRequired.code(), "sign_in/required");
    }
}
