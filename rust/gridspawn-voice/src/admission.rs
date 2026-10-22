//! Voice admission (protocol 04 §1): the session's voice grant buys an
//! endpoint and a short SFU token.

use std::sync::Arc;

use serde_json::{json, Value};
use url::Url;
use zeroize::Zeroizing;

use crate::error::VoiceError;

/// The only room the backend serves today.
pub(crate) const ROOM: &str = "match";

pub(crate) struct Admission {
    pub endpoint_id: String,
    pub room_id: String,
    pub node_id: String,
    pub sfu_url: Url,
    pub sfu_token: Zeroizing<String>,
    /// Unix seconds.
    pub sfu_expires_at: i64,
    pub audio_max_bps: Option<u32>,
}

impl std::fmt::Debug for Admission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Admission")
            .field("endpoint_id", &self.endpoint_id)
            .field("node_id", &self.node_id)
            .field("sfu_token", &"****")
            .field("sfu_expires_at", &self.sfu_expires_at)
            .finish()
    }
}

/// https, or http on a loopback host (local tests), as the reference client accepts.
fn secure(url: &Url) -> bool {
    match url.scheme() {
        "https" => url.host().is_some(),
        "http" => matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]")),
        _ => false,
    }
}

/// `POST {voice}/admissions`. With `endpoint`, renews that endpoint.
pub(crate) async fn admit(
    http: &reqwest::Client,
    voice: &Url,
    grant: &str,
    endpoint: Option<&str>,
) -> Result<Admission, VoiceError> {
    let mut body = json!({ "room": ROOM });
    if let Some(endpoint) = endpoint {
        body["endpointId"] = json!(endpoint);
    }
    let url = format!("{}/admissions", voice.as_str().trim_end_matches('/'));
    let response = http
        .post(url)
        .bearer_auth(grant)
        .json(&body)
        .send()
        .await
        .map_err(|error| VoiceError::Network(Arc::new(error)))?;
    let status = response.status();
    let body: Value = response
        .json()
        .await
        .map_err(|_| VoiceError::InvalidResponse("the admission answer is not JSON"))?;
    if !status.is_success() {
        let code = body
            .get("error")
            .and_then(|error| error.get("code").or(Some(error)))
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned();
        if code == "sdk_voice_reconnect_required" {
            return Err(VoiceError::ReconnectRequired);
        }
        return Err(VoiceError::Admission {
            status: status.as_u16(),
            code,
        });
    }
    parse(&body)
}

pub(crate) fn parse(body: &Value) -> Result<Admission, VoiceError> {
    let text = |value: &Value| value.as_str().map(str::to_owned);
    let invalid = VoiceError::InvalidResponse;
    let sfu = &body["sfu"];
    let endpoint_id = text(&body["endpointId"]).ok_or(invalid("endpointId"))?;
    let room_id = text(&body["room"]["roomId"]).ok_or(invalid("room.roomId"))?;
    let node_id = text(&sfu["nodeId"]).ok_or(invalid("sfu.nodeId"))?;
    let sfu_room = text(&sfu["roomId"]).ok_or(invalid("sfu.roomId"))?;
    let token = text(&sfu["token"]).ok_or(invalid("sfu.token"))?;
    let expires_at = sfu["expiresAt"].as_i64().ok_or(invalid("sfu.expiresAt"))?;
    let sfu_url = text(&sfu["url"])
        .and_then(|url| Url::parse(&url).ok())
        .filter(secure)
        .ok_or(invalid("sfu.url"))?;
    let node_ok = (3..=128).contains(&node_id.len())
        && node_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if sfu_room != room_id || !node_ok || endpoint_id.is_empty() || token.is_empty() {
        return Err(invalid("inconsistent admission"));
    }
    Ok(Admission {
        endpoint_id,
        room_id,
        node_id,
        sfu_url,
        sfu_token: Zeroizing::new(token),
        sfu_expires_at: expires_at,
        audio_max_bps: sfu["limits"]["audioMaxBps"]
            .as_f64()
            .filter(|bps| bps.is_finite() && *bps > 0.0)
            .map(|bps| bps.min(u32::MAX as f64) as u32),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Value {
        json!({
            "sessionId": "s",
            "participant": { "participantId": "gsp1.p", "identityKind": "server_verified_guest" },
            "endpointId": "gve1.0123456789abcdef0123456789abcdef",
            "room": { "name": "match", "roomId": "sdk:a:development:s:match" },
            "admissionExpiresAt": 2_000_000_000,
            "sfu": {
                "url": "https://sfu.example", "nodeId": "node-1",
                "roomId": "sdk:a:development:s:match", "token": "secret-token",
                "expiresAt": 2_000_000_060,
                "limits": { "audioMaxBps": 64000, "maxIncomingBps": 1000000 }
            }
        })
    }

    #[test]
    fn parses_and_validates() {
        let admission = parse(&sample()).unwrap();
        assert_eq!(admission.node_id, "node-1");
        assert_eq!(admission.audio_max_bps, Some(64000));
        assert!(!format!("{admission:?}").contains("secret-token"));

        let mut other_room = sample();
        other_room["sfu"]["roomId"] = json!("sdk:a:development:other:match");
        assert!(parse(&other_room).is_err());
        let mut plain_http = sample();
        plain_http["sfu"]["url"] = json!("http://sfu.example");
        assert!(parse(&plain_http).is_err());
        let mut bad_node = sample();
        bad_node["sfu"]["nodeId"] = json!("a b");
        assert!(parse(&bad_node).is_err());
    }
}
