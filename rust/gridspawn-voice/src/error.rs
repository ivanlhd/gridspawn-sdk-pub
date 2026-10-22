use std::sync::Arc;

/// Voice failures. [`VoiceError::code`] gives the stable `voice/…` code
/// (`docs/protocol/04-voice.md`, the voice module's error table).
#[derive(Clone, Debug, thiserror::Error)]
#[non_exhaustive]
pub enum VoiceError {
    /// The attached session has no voice grant, or bootstrap has no voice endpoint.
    #[error("voice is not available for this session")]
    ModuleUnavailable,
    /// The voice gateway refused the admission (`code` is its error code).
    #[error("voice admission refused: {status} {code}")]
    Admission { status: u16, code: String },
    /// The endpoint moved to another SFU node; leave and join again.
    #[error("voice must reconnect")]
    ReconnectRequired,
    /// Socket.IO could not connect to the SFU.
    #[error("voice signaling failed")]
    SignalingFailed,
    #[error("voice signaling timed out")]
    SignalingTimeout,
    #[error("voice signaling disconnected")]
    SignalingDisconnected,
    /// The SFU refused a request: `operation` and its reason.
    #[error("the voice SFU refused {operation}: {reason}")]
    Sfu { operation: String, reason: String },
    /// A WebRTC transport failed (ICE or DTLS).
    #[error("voice transport failed: {0}")]
    TransportFailed(String),
    /// Opus or the audio processing module failed.
    #[error("voice audio failed: {0}")]
    Audio(String),
    /// Renewing the grant, the admission or the SFU token failed.
    #[error("voice authorization renewal failed")]
    AuthRefreshFailed,
    /// A service answered something the protocol does not allow.
    #[error("invalid voice response: {0}")]
    InvalidResponse(&'static str),
    #[error("voice is not joined")]
    NotReady,
    #[error("network error")]
    Network(#[source] Arc<reqwest::Error>),
    #[error("{0}")]
    Client(#[from] gridspawn_client::Error),
}

impl VoiceError {
    pub fn code(&self) -> String {
        match self {
            Self::ModuleUnavailable => "voice/module_unavailable".into(),
            Self::Admission { code, .. } => match code.as_str() {
                "sdk_voice_room_not_supported" => "voice/room_not_supported".into(),
                "sdk_voice_room_not_configured" => "voice/room_not_configured".into(),
                "sdk_voice_room_full" => "voice/room_full".into(),
                other => format!("voice/admission_{}", normalize(other)),
            },
            Self::ReconnectRequired => "voice/reconnect_required".into(),
            Self::SignalingFailed => "voice/signaling_failed".into(),
            Self::SignalingTimeout => "voice/signaling_timeout".into(),
            Self::SignalingDisconnected => "voice/signaling_disconnected".into(),
            Self::Sfu { reason, .. } => match reason.as_str() {
                "room_full" => "voice/room_full".into(),
                "server_muted" => "voice/server_muted".into(),
                other => format!("voice/sfu_{}", normalize(other)),
            },
            Self::TransportFailed(_) => "voice/transport_failed".into(),
            Self::Audio(_) => "voice/audio_failed".into(),
            Self::AuthRefreshFailed => "voice/auth_refresh_failed".into(),
            Self::InvalidResponse(_) => "voice/invalid_response".into(),
            Self::NotReady => "voice/not_ready".into(),
            Self::Network(_) => "network".into(),
            Self::Client(error) => error.code(),
        }
    }
}

/// Lower-case `[a-z0-9_]` for codes built from service text.
fn normalize(value: &str) -> String {
    let mut out: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    out.truncate(64);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes() {
        assert_eq!(
            VoiceError::ReconnectRequired.code(),
            "voice/reconnect_required"
        );
        assert_eq!(
            VoiceError::Sfu {
                operation: "consume".into(),
                reason: "not-authorized-for-producer".into()
            }
            .code(),
            "voice/sfu_not_authorized_for_producer"
        );
        for (code, expected) in [
            ("sdk_voice_room_not_supported", "voice/room_not_supported"),
            ("sdk_voice_room_not_configured", "voice/room_not_configured"),
            ("sdk_voice_room_full", "voice/room_full"),
            (
                "sdk_voice_rate_limited",
                "voice/admission_sdk_voice_rate_limited",
            ),
        ] {
            let error = VoiceError::Admission {
                status: 400,
                code: code.into(),
            };
            assert_eq!(error.code(), expected);
        }
        for (reason, expected) in [
            ("room_full", "voice/room_full"),
            ("server_muted", "voice/server_muted"),
            ("sdk_server_authority", "voice/sfu_sdk_server_authority"),
        ] {
            let error = VoiceError::Sfu {
                operation: "joinRoom".into(),
                reason: reason.into(),
            };
            assert_eq!(error.code(), expected);
        }
    }
}
