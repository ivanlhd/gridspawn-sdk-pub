//! Voice admission (protocol 04 §1): the session's voice grant buys an
//! endpoint and a short SFU token.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use serde_json::{json, Value};
use url::Url;
use zeroize::Zeroizing;

use crate::error::VoiceError;

/// Every game session has this room; others exist when the game server
/// configured them (and the voice service has named rooms on).
pub(crate) const DEFAULT_ROOM: &str = "match";

/// The SFU token's lifetime when the answer has no usable `Date` header:
/// shorter than the service's tokens, so renewal comes early, never late.
const FALLBACK_LIFETIME: Duration = Duration::from_secs(30);
const MAX_LIFETIME: Duration = Duration::from_secs(3600);

pub(crate) struct Admission {
    pub endpoint_id: String,
    /// The game session's participant id (`gsp1.…`), when the answer has it.
    pub participant_id: Option<String>,
    pub room_id: String,
    pub node_id: String,
    pub sfu_url: Url,
    pub sfu_token: Zeroizing<String>,
    /// When the SFU token stops working, on this machine's monotonic clock:
    /// `sfu.expiresAt` minus the server's own time (its `Date` header), from
    /// when the answer arrived. The local wall clock is never used.
    pub sfu_deadline: Instant,
    pub audio_max_bps: Option<u32>,
}

impl std::fmt::Debug for Admission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Admission")
            .field("endpoint_id", &self.endpoint_id)
            .field("node_id", &self.node_id)
            .field("sfu_token", &"****")
            .field("sfu_deadline", &self.sfu_deadline)
            .finish()
    }
}

/// How long the token lasts, measured on the server's clock only: its
/// `expiresAt` against the `Date` of the same answer (Unix seconds).
pub(crate) fn token_lifetime(expires_at: i64, server_now: Option<i64>) -> Duration {
    match server_now {
        Some(now) => {
            Duration::from_secs(expires_at.saturating_sub(now).max(0) as u64).min(MAX_LIFETIME)
        }
        None => FALLBACK_LIFETIME,
    }
}

/// An HTTP `Date` in IMF-fixdate form (`Sun, 06 Nov 1994 08:49:37 GMT`, the
/// form servers send) as Unix seconds.
pub(crate) fn parse_http_date(value: &str) -> Option<i64> {
    let rest = value.trim().split_once(", ")?.1;
    let mut parts = rest.split(' ');
    let (day, month, year, time, zone) = (
        parts.next()?,
        parts.next()?,
        parts.next()?,
        parts.next()?,
        parts.next()?,
    );
    if zone != "GMT" || parts.next().is_some() || day.len() != 2 || year.len() != 4 {
        return None;
    }
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|name| *name == month)? as i64
        + 1;
    let (day, year): (i64, i64) = (day.parse().ok()?, year.parse().ok()?);
    let mut clock = time.split(':').map(|part| {
        (part.len() == 2)
            .then(|| part.parse::<i64>().ok())
            .flatten()
    });
    let (hour, minute, second) = (clock.next()??, clock.next()??, clock.next()??);
    if clock.next().is_some() || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60
    {
        return None;
    }
    // Days from 1970-01-01 (Howard Hinnant's days_from_civil).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// https, or http on a loopback host (local tests), as the reference client accepts.
fn secure(url: &Url) -> bool {
    match url.scheme() {
        "https" => url.host().is_some(),
        "http" => matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]")),
        _ => false,
    }
}

/// `POST {voice}/admissions` for `room`. With `endpoint`, renews that endpoint.
pub(crate) async fn admit(
    http: &reqwest::Client,
    voice: &Url,
    grant: &str,
    room: &str,
    endpoint: Option<&str>,
) -> Result<Admission, VoiceError> {
    let mut body = json!({ "room": room });
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
    let received = Instant::now();
    let status = response.status();
    let server_now = response
        .headers()
        .get(reqwest::header::DATE)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_http_date);
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
    parse(&body, received, server_now)
}

/// `received`: when the answer arrived (monotonic); `server_now`: its `Date`.
pub(crate) fn parse(
    body: &Value,
    received: Instant,
    server_now: Option<i64>,
) -> Result<Admission, VoiceError> {
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
        participant_id: text(&body["participant"]["participantId"]).filter(|id| !id.is_empty()),
        room_id,
        node_id,
        sfu_url,
        sfu_token: Zeroizing::new(token),
        sfu_deadline: received + token_lifetime(expires_at, server_now),
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
        let now = Instant::now();
        let admission = parse(&sample(), now, Some(2_000_000_000)).unwrap();
        assert_eq!(admission.node_id, "node-1");
        assert_eq!(admission.participant_id.as_deref(), Some("gsp1.p"));
        assert_eq!(admission.audio_max_bps, Some(64000));
        assert!(!format!("{admission:?}").contains("secret-token"));

        let mut other_room = sample();
        other_room["sfu"]["roomId"] = json!("sdk:a:development:other:match");
        assert!(parse(&other_room, now, None).is_err());
        let mut plain_http = sample();
        plain_http["sfu"]["url"] = json!("http://sfu.example");
        assert!(parse(&plain_http, now, None).is_err());
        let mut bad_node = sample();
        bad_node["sfu"]["nodeId"] = json!("a b");
        assert!(parse(&bad_node, now, None).is_err());
    }

    /// The deadline comes from the server's two times only. The sample's
    /// times are in 2033: this machine's clock (2026, or anything else) plays
    /// no part, so a client clock that is minutes or years off renews on time.
    #[test]
    fn the_token_deadline_ignores_the_local_clock() {
        let received = Instant::now();
        let admission = parse(&sample(), received, Some(2_000_000_000)).unwrap();
        assert_eq!(admission.sfu_deadline - received, Duration::from_secs(60));
        // Without a Date header: a short fallback, never the local clock.
        let admission = parse(&sample(), received, None).unwrap();
        assert_eq!(admission.sfu_deadline - received, FALLBACK_LIFETIME);
        // An answer that is already expired by its own clock: renew at once.
        assert_eq!(token_lifetime(100, Some(200)), Duration::ZERO);
        assert_eq!(token_lifetime(i64::MAX, Some(0)), MAX_LIFETIME);
    }

    #[test]
    fn http_dates() {
        assert_eq!(
            parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(784_111_777)
        );
        assert_eq!(parse_http_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(
            parse_http_date("Fri, 23 Oct 2026 00:00:15 GMT"),
            Some(1_792_713_615)
        );
        assert_eq!(
            parse_http_date("Tue, 29 Feb 2028 23:59:59 GMT"),
            Some(1_835_481_599)
        );
        for bad in [
            "",
            "Sunday, 06-Nov-94 08:49:37 GMT",
            "Sun Nov  6 08:49:37 1994",
            "Sun, 06 Nov 1994 08:49:37 UTC",
            "Sun, 06 Foo 1994 08:49:37 GMT",
            "Sun, 6 Nov 1994 08:49:37 GMT",
            "Sun, 06 Nov 1994 8:49:37 GMT",
            "Sun, 06 Nov 1994 25:49:37 GMT",
        ] {
            assert_eq!(parse_http_date(bad), None, "{bad:?}");
        }
    }
}
