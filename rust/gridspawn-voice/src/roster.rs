//! The room as this client sees it, from the SFU's events: who is in it (the
//! roster, with display names), what the game server decided for each speaker
//! (audibility, gain, pan), and the room's rules. Pure parsing and math; the
//! session keeps the state.

use serde_json::Value;

/// The room's rules as the voice service tells clients (`voiceRoom`). Proximity
/// distances are not sent: the game server owns them, clients get gain and pan.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct RoomConfig {
    /// `open`, `channels`, `proximity` or `hybrid`.
    pub mode: String,
    /// `server` (the service sends gain and pan) or `client` (the game places
    /// voices itself).
    pub spatial: String,
    /// False for client-spatialized rooms: a modified client hears everyone
    /// its channels allow. Label such rooms in the game's UI.
    pub secrecy_safe: bool,
    pub max_participants: u32,
}

impl RoomConfig {
    pub(crate) fn parse(value: &Value) -> Option<Self> {
        let text = |key: &str| value[key].as_str().map(str::to_owned);
        Some(Self {
            mode: text("mode")?,
            spatial: text("spatial")?,
            secrecy_safe: value["secrecySafe"].as_bool()?,
            max_participants: value["maxParticipants"].as_u64()?.min(u64::from(u32::MAX)) as u32,
        })
    }
}

/// What the server decided for one speaker, for this listener.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Audibility {
    pub audible: bool,
    /// 0–1, multiplied with the local volume before mixing.
    pub gain: f32,
    /// −1 (left) … 1 (right); `None` without a server pan.
    pub pan: Option<f32>,
}

impl Default for Audibility {
    /// Until the service says otherwise: heard at full volume, centred.
    fn default() -> Self {
        Self {
            audible: true,
            gain: 1.0,
            pan: None,
        }
    }
}

impl Audibility {
    /// `audibility {producerId, audible, gain?, pan?}` → (producer, decision).
    pub(crate) fn parse(value: &Value) -> Option<(String, Self)> {
        let producer = value["producerId"].as_str()?.to_owned();
        let audible = value["audible"].as_bool()?;
        let finite = |key: &str| value[key].as_f64().filter(|v| v.is_finite());
        let gain = if audible {
            finite("gain").unwrap_or(1.0).clamp(0.0, 1.0) as f32
        } else {
            0.0
        };
        let pan = if audible {
            finite("pan").map(|pan| pan.clamp(-1.0, 1.0) as f32)
        } else {
            None
        };
        Some((producer, Self { audible, gain, pan }))
    }
}

/// A producer the SFU announced (`joinRoom`'s `producers`, `newProducer`):
/// `{producerId, kind, peerId?, ownerPeerId?, type?, serverMuted?}`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Announced {
    pub producer_id: String,
    /// The SFU endpoint that produces it.
    pub owner: String,
    /// Its player is muted by the game server now (game-session rooms only;
    /// later changes come as `producerPaused` / `producerResumed`).
    pub server_muted: bool,
}

impl Announced {
    /// Someone's voice (audio, `type` absent or `voice`, with an owner), or `None`.
    pub(crate) fn parse(value: &Value) -> Option<Self> {
        let is_voice =
            value["kind"] == "audio" && value["type"].as_str().is_none_or(|t| t == "voice");
        let producer_id = value["producerId"].as_str().filter(|id| !id.is_empty())?;
        let owner = value["peerId"]
            .as_str()
            .or(value["ownerPeerId"].as_str())
            .filter(|owner| !owner.is_empty())?;
        is_voice.then(|| Self {
            producer_id: producer_id.to_owned(),
            owner: owner.to_owned(),
            server_muted: value["serverMuted"].as_bool() == Some(true),
        })
    }
}

/// One roster entry: an SFU endpoint and the participant behind it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Peer {
    pub endpoint: String,
    pub participant: String,
    pub display_name: Option<String>,
}

impl Peer {
    /// `{endpointId, participantId, displayName?, identityKind?}`
    pub(crate) fn parse(value: &Value) -> Option<Self> {
        let text = |key: &str| {
            value[key]
                .as_str()
                .filter(|text| !text.is_empty())
                .map(str::to_owned)
        };
        Some(Self {
            endpoint: text("endpointId")?,
            participant: text("participantId")?,
            display_name: text("displayName"),
        })
    }
}

/// Left and right gains for a pan in [−1, 1]: equal-power, scaled so that the
/// centre keeps both channels at 1 and neither side ever goes above 1. Without
/// a pan both stay at 1 (as the mono pulls).
pub(crate) fn pan_gains(pan: Option<f32>) -> (f32, f32) {
    let Some(pan) = pan else {
        return (1.0, 1.0);
    };
    let angle = (pan.clamp(-1.0, 1.0) + 1.0) * std::f32::consts::FRAC_PI_4;
    let scale = std::f32::consts::SQRT_2;
    (
        (angle.cos() * scale).clamp(0.0, 1.0),
        (angle.sin() * scale).clamp(0.0, 1.0),
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn room_config_from_voice_room() {
        let config = RoomConfig::parse(&json!({
            "mode": "proximity", "spatial": "server", "secrecySafe": true, "maxParticipants": 64
        }))
        .unwrap();
        assert_eq!(config.mode, "proximity");
        assert!(config.secrecy_safe);
        assert_eq!(config.max_participants, 64);
        assert!(
            RoomConfig::parse(&json!(null)).is_none(),
            "unmanaged rooms send null"
        );
        assert!(RoomConfig::parse(&json!({"mode": "open"})).is_none());
    }

    #[test]
    fn audibility_events() {
        let (producer, heard) = Audibility::parse(&json!({
            "producerId": "p1", "audible": true, "gain": 0.5625, "pan": 1.0
        }))
        .unwrap();
        assert_eq!(producer, "p1");
        assert_eq!(
            heard,
            Audibility {
                audible: true,
                gain: 0.5625,
                pan: Some(1.0)
            }
        );
        // Unmanaged rooms: gain 1, pan null.
        let (_, open) = Audibility::parse(
            &json!({"producerId": "p1", "audible": true, "gain": 1, "pan": null}),
        )
        .unwrap();
        assert_eq!(open, Audibility::default());
        let (_, silent) =
            Audibility::parse(&json!({"producerId": "p1", "audible": false})).unwrap();
        assert!(!silent.audible && silent.gain == 0.0 && silent.pan.is_none());
        // Out-of-range values are clamped; a missing field is refused.
        let (_, loud) =
            Audibility::parse(&json!({"producerId": "p", "audible": true, "gain": 3, "pan": -7}))
                .unwrap();
        assert_eq!((loud.gain, loud.pan), (1.0, Some(-1.0)));
        assert!(Audibility::parse(&json!({"producerId": "p"})).is_none());
    }

    #[test]
    fn announced_producers() {
        let muted = Announced::parse(&json!({
            "producerId": "p1", "peerId": "gve1.a", "ownerPeerId": "gve1.a",
            "kind": "audio", "type": "voice", "routeClass": "room", "serverMuted": true
        }))
        .unwrap();
        assert_eq!(
            muted,
            Announced {
                producer_id: "p1".into(),
                owner: "gve1.a".into(),
                server_muted: true
            }
        );
        // Outside game-session rooms the field is absent: not muted.
        let plain = Announced::parse(
            &json!({"producerId": "p2", "ownerPeerId": "gve1.b", "kind": "audio"}),
        )
        .unwrap();
        assert_eq!(
            (plain.owner.as_str(), plain.server_muted),
            ("gve1.b", false)
        );
        assert!(
            !Announced::parse(
                &json!({"producerId": "p3", "peerId": "gve1.c", "kind": "audio", "serverMuted": false})
            )
            .unwrap()
            .server_muted
        );
        // Not someone's voice.
        for value in [
            json!({"producerId": "v", "peerId": "gve1.a", "kind": "video", "serverMuted": true}),
            json!({"producerId": "s", "peerId": "gve1.a", "kind": "audio", "type": "screen"}),
            json!({"producerId": "o", "kind": "audio"}),
            json!({"peerId": "gve1.a", "kind": "audio"}),
        ] {
            assert!(Announced::parse(&value).is_none(), "{value}");
        }
    }

    #[test]
    fn roster_peers() {
        let peer = Peer::parse(&json!({
            "endpointId": "gve1.a", "participantId": "gsp1.a", "displayName": "Wanderer",
            "identityKind": "server_verified_guest"
        }))
        .unwrap();
        assert_eq!(peer.participant, "gsp1.a");
        assert_eq!(peer.display_name.as_deref(), Some("Wanderer"));
        let nameless = Peer::parse(
            &json!({"endpointId": "gve1.b", "participantId": "gsp1.b", "displayName": null}),
        )
        .unwrap();
        assert_eq!(nameless.display_name, None);
        assert!(Peer::parse(&json!({"endpointId": "gve1.c"})).is_none());
    }

    #[test]
    fn pan_law() {
        assert_eq!(pan_gains(None), (1.0, 1.0));
        let (l, r) = pan_gains(Some(0.0));
        assert!(
            (l - 1.0).abs() < 1e-6 && (r - 1.0).abs() < 1e-6,
            "centre {l} {r}"
        );
        let (l, r) = pan_gains(Some(-1.0));
        assert!((l - 1.0).abs() < 1e-6 && r.abs() < 1e-6, "left {l} {r}");
        let (l, r) = pan_gains(Some(1.0));
        assert!(l.abs() < 1e-6 && (r - 1.0).abs() < 1e-6, "right {l} {r}");
        for step in -16..=16 {
            let (l, r) = pan_gains(Some(step as f32 / 16.0));
            assert!(l <= 1.0 && r <= 1.0 && l >= 0.0 && r >= 0.0);
        }
        let (l, r) = pan_gains(Some(0.5));
        assert!(
            l < r && r == 1.0,
            "right of centre: left quieter, right full ({l} {r})"
        );
    }
}
