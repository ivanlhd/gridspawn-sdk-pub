use std::{fmt, sync::Arc, time::Duration};

/// Everything that can go wrong. `Api` carries the service's own answer.
#[derive(Clone, Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Bad local input (credential format, field bounds, key format). Nothing was sent.
    #[error("invalid {field}: {reason}")]
    InvalidInput {
        field: &'static str,
        reason: &'static str,
    },
    #[error("the credential file could not be read")]
    CredentialUnreadable(#[source] Arc<std::io::Error>),
    /// On Unix, `Credential::from_file` refuses a key other users can read.
    #[error(
        "the credential file {path} is readable by other users; run `chmod 600 {path}` \
         (or load it with Credential::from_file_unchecked)"
    )]
    CredentialFileTooOpen { path: String },
    /// Connect, TLS or read failure, after the retries of the `RetryPolicy`.
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
    /// The blocking facade could not start its runtime.
    #[error("could not start the async runtime")]
    Runtime(#[source] Arc<std::io::Error>),
    /// A documented error answer from the SDK edge or from Core.
    /// Boxed to keep `Result<_, Error>` small.
    #[error("{0}")]
    Api(Box<ApiError>),
}

impl Error {
    /// Worth retrying later with the same Idempotency-Key: network errors,
    /// timeouts, and answers with status 5xx or 429.
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

    /// What the crate retries by itself (429 is left to the caller).
    pub(crate) fn transport_retryable(&self) -> bool {
        match self {
            Self::Network(_) | Self::Timeout => true,
            Self::Api(error) => error.status >= 500,
            _ => false,
        }
    }
}

/// Which service answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Service {
    /// The SDK edge, `sdk.gridspawn.net` (sessions, grants, bootstrap).
    Edge,
    /// Core's posting route (club posts).
    Core,
}

impl fmt::Display for Service {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Edge => "edge",
            Self::Core => "core",
        })
    }
}

/// An error answer: the HTTP status and the documented code.
#[derive(Clone, Debug, thiserror::Error)]
#[error("{service} {status} {code}")]
#[non_exhaustive]
pub struct ApiError {
    pub service: Service,
    pub status: u16,
    pub code: ErrorCode,
    /// The code exactly as the service sent it (`http_<status>` when the body had none).
    pub wire_code: String,
    /// The edge's human-readable message; Core sends codes only.
    pub message: Option<String>,
    pub request_id: Option<String>,
    /// From `Retry-After` when present (presence rate limits send it; Core's
    /// posting limits are fixed one-minute windows without it).
    pub retry_after: Option<Duration>,
    /// The request field the error is about (`error.field`), when the service
    /// names one (presence and voice payload errors).
    pub field: Option<String>,
    /// Why that field was refused (`error.reason`, voice payload errors), for
    /// example `count` or `range`.
    pub reason: Option<String>,
}

/// The documented error codes of protocols 02 and 03 and the voice routes
/// (edge), and of 05 (Core).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorCode {
    /// `server/authentication_required`: wrong, revoked or expired key, or the
    /// application or environment is turned off.
    AuthenticationRequired,
    /// `server/scope_denied`: the key lacks the scope for this call.
    ScopeDenied,
    /// `server/browser_forbidden` or `game_server_browser_forbidden`.
    BrowserForbidden,
    /// `service/not_ready`.
    ServiceNotReady,
    /// `session/invalid_request`.
    SessionInvalidRequest,
    /// `session/not_found`.
    SessionNotFound,
    /// `session/capability_denied`: a module the app or the player may not use.
    CapabilityDenied,
    /// `session/assertion_invalid`.
    AssertionInvalid,
    /// `session/assertion_replayed`.
    AssertionReplayed,
    /// `request/idempotency_required`.
    IdempotencyRequired,
    /// `request/idempotency_conflict`: the same key with a different body.
    IdempotencyConflict,
    /// `request/idempotency_in_progress`.
    IdempotencyInProgress,
    /// `config/app_not_found`.
    AppNotFound,
    /// `config/protocol_incompatible`.
    ProtocolIncompatible,
    /// `grant_required`.
    GrantRequired,
    /// `grant_invalid` (after the one automatic grant refresh).
    GrantInvalid,
    /// `grant_keys_unavailable`.
    GrantKeysUnavailable,
    /// `feature_not_found`: no destination for the feature, posting off, or
    /// the environment not allowed to post.
    FeatureNotFound,
    /// `invalid_game_server_json`.
    InvalidBody,
    /// `invalid_idempotency_key`.
    InvalidIdempotencyKey,
    /// Content refused; the string is the rule (`missing_content`, `title_too_long`, …).
    ContentRejected(String),
    /// `announcement_board_full`.
    BoardFull,
    /// `announcement_board_busy`.
    BoardBusy,
    /// `club_chat_unavailable`, `club_webhook_chat_unavailable` or `chat_unavailable`.
    ChatUnavailable,
    /// `presence/invalid_request` ([`ApiError::field`] when known).
    PresenceInvalidRequest,
    /// `presence/text_not_allowed`: an email, web address or identifier in
    /// the text ([`ApiError::field`]).
    PresenceTextNotAllowed,
    /// `presence/scope_denied`: no `presence.write` consent, a party without
    /// `presence.party`, or the app's presence module is off.
    PresenceScopeDenied,
    /// `presence/guest_not_supported`.
    PresenceGuestNotSupported,
    /// `presence/account_unavailable`: the player's account is not active.
    PresenceAccountUnavailable,
    /// `voice/invalid_request`: the voice body broke a rule
    /// ([`ApiError::field`] and [`ApiError::reason`] when known).
    VoiceInvalidRequest,
    /// `voice/stale_sequence`: a newer update was already accepted.
    VoiceStaleSequence,
    /// `voice/participant_not_found`: not admitted to this session (or gone).
    VoiceParticipantNotFound,
    /// `voice/module_disabled`: voice is off for the application and environment.
    VoiceModuleDisabled,
    /// `voice/unavailable` (503): voice control is temporarily unreachable.
    VoiceUnavailable,
    /// `rate_limited`, `presence/rate_limited`, `voice/rate_limited`, or any
    /// 429 without a code (nginx).
    RateLimited,
    /// Any other code, kept verbatim.
    Other(String),
}

const CONTENT_CODES: [&str; 8] = [
    "missing_content",
    "missing_title",
    "title_too_long",
    "content_too_long",
    "summary_too_long",
    "body_too_long",
    "invalid_link_url",
    "link_label_too_long",
];

impl ErrorCode {
    pub(crate) fn from_wire(code: &str) -> Self {
        match code {
            "server/authentication_required" => Self::AuthenticationRequired,
            "server/scope_denied" => Self::ScopeDenied,
            "server/browser_forbidden"
            | "game_server_browser_forbidden"
            | "voice/browser_forbidden" => Self::BrowserForbidden,
            "service/not_ready" => Self::ServiceNotReady,
            "session/invalid_request" => Self::SessionInvalidRequest,
            "session/not_found" | "voice/session_not_found" => Self::SessionNotFound,
            "session/capability_denied" => Self::CapabilityDenied,
            "session/assertion_invalid" => Self::AssertionInvalid,
            "session/assertion_replayed" => Self::AssertionReplayed,
            "request/idempotency_required" => Self::IdempotencyRequired,
            "request/idempotency_conflict" => Self::IdempotencyConflict,
            "request/idempotency_in_progress" => Self::IdempotencyInProgress,
            "config/app_not_found" => Self::AppNotFound,
            "config/protocol_incompatible" => Self::ProtocolIncompatible,
            "grant_required" => Self::GrantRequired,
            "grant_invalid" => Self::GrantInvalid,
            "grant_keys_unavailable" => Self::GrantKeysUnavailable,
            "feature_not_found" => Self::FeatureNotFound,
            "invalid_game_server_json" => Self::InvalidBody,
            "invalid_idempotency_key" => Self::InvalidIdempotencyKey,
            "announcement_board_full" => Self::BoardFull,
            "announcement_board_busy" => Self::BoardBusy,
            "club_chat_unavailable" | "club_webhook_chat_unavailable" | "chat_unavailable" => {
                Self::ChatUnavailable
            }
            "presence/invalid_request" => Self::PresenceInvalidRequest,
            "presence/text_not_allowed" => Self::PresenceTextNotAllowed,
            "presence/scope_denied" => Self::PresenceScopeDenied,
            "presence/guest_not_supported" => Self::PresenceGuestNotSupported,
            "presence/account_unavailable" => Self::PresenceAccountUnavailable,
            "voice/invalid_request" => Self::VoiceInvalidRequest,
            "voice/stale_sequence" => Self::VoiceStaleSequence,
            "voice/participant_not_found" => Self::VoiceParticipantNotFound,
            "voice/module_disabled" => Self::VoiceModuleDisabled,
            "voice/unavailable" => Self::VoiceUnavailable,
            "rate_limited" | "presence/rate_limited" | "voice/rate_limited" => Self::RateLimited,
            other if CONTENT_CODES.contains(&other) => Self::ContentRejected(other.to_owned()),
            other => Self::Other(other.to_owned()),
        }
    }

    /// The wire code (the edge's spelling where two services share a variant).
    pub fn as_str(&self) -> &str {
        match self {
            Self::AuthenticationRequired => "server/authentication_required",
            Self::ScopeDenied => "server/scope_denied",
            Self::BrowserForbidden => "server/browser_forbidden",
            Self::ServiceNotReady => "service/not_ready",
            Self::SessionInvalidRequest => "session/invalid_request",
            Self::SessionNotFound => "session/not_found",
            Self::CapabilityDenied => "session/capability_denied",
            Self::AssertionInvalid => "session/assertion_invalid",
            Self::AssertionReplayed => "session/assertion_replayed",
            Self::IdempotencyRequired => "request/idempotency_required",
            Self::IdempotencyConflict => "request/idempotency_conflict",
            Self::IdempotencyInProgress => "request/idempotency_in_progress",
            Self::AppNotFound => "config/app_not_found",
            Self::ProtocolIncompatible => "config/protocol_incompatible",
            Self::GrantRequired => "grant_required",
            Self::GrantInvalid => "grant_invalid",
            Self::GrantKeysUnavailable => "grant_keys_unavailable",
            Self::FeatureNotFound => "feature_not_found",
            Self::InvalidBody => "invalid_game_server_json",
            Self::InvalidIdempotencyKey => "invalid_idempotency_key",
            Self::ContentRejected(code) | Self::Other(code) => code,
            Self::BoardFull => "announcement_board_full",
            Self::BoardBusy => "announcement_board_busy",
            Self::ChatUnavailable => "chat_unavailable",
            Self::PresenceInvalidRequest => "presence/invalid_request",
            Self::PresenceTextNotAllowed => "presence/text_not_allowed",
            Self::PresenceScopeDenied => "presence/scope_denied",
            Self::PresenceGuestNotSupported => "presence/guest_not_supported",
            Self::PresenceAccountUnavailable => "presence/account_unavailable",
            Self::VoiceInvalidRequest => "voice/invalid_request",
            Self::VoiceStaleSequence => "voice/stale_sequence",
            Self::VoiceParticipantNotFound => "voice/participant_not_found",
            Self::VoiceModuleDisabled => "voice/module_disabled",
            Self::VoiceUnavailable => "voice/unavailable",
            Self::RateLimited => "rate_limited",
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
            "server/authentication_required",
            "server/scope_denied",
            "server/browser_forbidden",
            "service/not_ready",
            "session/invalid_request",
            "session/not_found",
            "session/capability_denied",
            "session/assertion_invalid",
            "session/assertion_replayed",
            "request/idempotency_required",
            "request/idempotency_conflict",
            "request/idempotency_in_progress",
            "config/app_not_found",
            "config/protocol_incompatible",
            "grant_required",
            "grant_invalid",
            "grant_keys_unavailable",
            "feature_not_found",
            "invalid_game_server_json",
            "invalid_idempotency_key",
            "announcement_board_full",
            "announcement_board_busy",
            "chat_unavailable",
            "presence/invalid_request",
            "presence/text_not_allowed",
            "presence/scope_denied",
            "presence/guest_not_supported",
            "presence/account_unavailable",
            "voice/invalid_request",
            "voice/stale_sequence",
            "voice/participant_not_found",
            "voice/module_disabled",
            "voice/unavailable",
            "rate_limited",
        ] {
            let mapped = ErrorCode::from_wire(code);
            assert!(!matches!(mapped, ErrorCode::Other(_)), "{code}");
            assert_eq!(mapped.as_str(), code);
        }
        assert_eq!(
            ErrorCode::from_wire("game_server_browser_forbidden"),
            ErrorCode::BrowserForbidden
        );
        assert_eq!(
            ErrorCode::from_wire("club_webhook_chat_unavailable"),
            ErrorCode::ChatUnavailable
        );
        assert_eq!(
            ErrorCode::from_wire("presence/rate_limited"),
            ErrorCode::RateLimited
        );
        for (alias, code) in [
            ("voice/browser_forbidden", ErrorCode::BrowserForbidden),
            ("voice/session_not_found", ErrorCode::SessionNotFound),
            ("voice/rate_limited", ErrorCode::RateLimited),
        ] {
            assert_eq!(ErrorCode::from_wire(alias), code, "{alias}");
        }
        for code in CONTENT_CODES {
            assert_eq!(
                ErrorCode::from_wire(code),
                ErrorCode::ContentRejected(code.to_owned())
            );
        }
        assert_eq!(
            ErrorCode::from_wire("game_server_console_unavailable"),
            ErrorCode::Other("game_server_console_unavailable".into())
        );
    }
}
