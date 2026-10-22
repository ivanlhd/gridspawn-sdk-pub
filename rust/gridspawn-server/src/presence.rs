//! Player presence set by the game server (protocol 03, the authoritative
//! server path): what a player is doing in your game, shown on their
//! Gridspawn profile, friends list and club member lists.

use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::{
    client::{segment, Client},
    error::Error,
    session::{Participant, ParticipantId, Session, SessionId},
};

/// What a player is doing (03 §Payload). Every field is optional: an empty
/// presence shows "Playing <your game>". The game's name and icon come from
/// its registration.
///
/// The service refuses text that contains email addresses, web addresses or
/// identifiers (`presence/text_not_allowed`); only lengths and characters are
/// checked here.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Presence {
    /// 1–128 characters after trimming, for example "Ranked match".
    pub details: Option<String>,
    /// 1–128 characters after trimming, for example "In lobby".
    pub state: Option<String>,
    /// 1–64 characters after trimming, for example "Neon District".
    pub location: Option<String>,
    /// Unix seconds, shown as time played: at most 5 minutes in the future and
    /// 30 days in the past.
    pub started_at: Option<i64>,
    /// Needs the player's `presence.party` consent.
    pub party: Option<Party>,
}

/// A party: its size is shown; its id is checked and then discarded.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Party {
    /// 1–64 of `[A-Za-z0-9._:-]`.
    pub id: String,
    pub size: u32,
    /// `1 ≤ size ≤ max ≤ 64`.
    pub max: u32,
}

impl Party {
    pub fn new(id: impl Into<String>, size: u32, max: u32) -> Self {
        Self {
            id: id.into(),
            size,
            max,
        }
    }
}

impl Presence {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn details(mut self, details: impl Into<String>) -> Self {
        self.details = Some(details.into());
        self
    }

    pub fn state(mut self, state: impl Into<String>) -> Self {
        self.state = Some(state.into());
        self
    }

    pub fn location(mut self, location: impl Into<String>) -> Self {
        self.location = Some(location.into());
        self
    }

    /// Unix seconds.
    pub fn started_at(mut self, unix_seconds: i64) -> Self {
        self.started_at = Some(unix_seconds);
        self
    }

    pub fn party(mut self, party: Party) -> Self {
        self.party = Some(party);
        self
    }

    /// The wire body, with text trimmed. Checks lengths, characters and the
    /// party bounds; the service checks the rest.
    pub(crate) fn to_body(&self) -> Result<Value, Error> {
        let mut body = Map::new();
        for (key, value, max, field) in [
            ("details", &self.details, 128, "presence details"),
            ("state", &self.state, 128, "presence state"),
            ("location", &self.location, 64, "presence location"),
        ] {
            if let Some(value) = value {
                body.insert(key.into(), json!(text(value, max, field)?));
            }
        }
        if let Some(started_at) = self.started_at {
            if started_at <= 0 {
                return Err(Error::InvalidInput {
                    field: "presence started at",
                    reason: "Unix seconds",
                });
            }
            body.insert("startedAt".into(), json!(started_at));
        }
        if let Some(party) = &self.party {
            let id_ok = (1..=64).contains(&party.id.len())
                && party
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'));
            if !id_ok {
                return Err(Error::InvalidInput {
                    field: "presence party id",
                    reason: "1-64 of A-Z a-z 0-9 . _ : -",
                });
            }
            if !(1 <= party.size && party.size <= party.max && party.max <= 64) {
                return Err(Error::InvalidInput {
                    field: "presence party",
                    reason: "1 <= size <= max <= 64",
                });
            }
            body.insert(
                "party".into(),
                json!({ "id": party.id, "size": party.size, "max": party.max }),
            );
        }
        Ok(Value::Object(body))
    }
}

fn text(value: &str, max: usize, field: &'static str) -> Result<String, Error> {
    let trimmed = value.trim();
    let length = trimmed.chars().count();
    if length == 0 || length > max {
        return Err(Error::InvalidInput {
            field,
            reason: if max == 64 {
                "1-64 characters after trimming"
            } else {
                "1-128 characters after trimming"
            },
        });
    }
    if trimmed.chars().any(char::is_control) {
        return Err(Error::InvalidInput {
            field,
            reason: "no control characters",
        });
    }
    Ok(trimmed.to_owned())
}

/// The service's answer to a presence update.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct PresenceAck {
    /// Unix seconds: the presence disappears then unless it is sent again
    /// (120 s after the update; re-send every 60 s while the player plays).
    pub expires_at: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetResponse {
    ok: bool,
    expires_at: i64,
}

#[derive(Deserialize)]
struct ClearResponse {
    ok: bool,
}

/// A participant of a session, by id: for presence updates after the
/// [`Participant`] value is gone (for example after a restart).
#[derive(Clone, Debug)]
pub struct ParticipantRef {
    session: Session,
    id: ParticipantId,
}

impl Session {
    /// A participant of this session by id.
    pub fn participant(&self, id: ParticipantId) -> ParticipantRef {
        ParticipantRef {
            session: self.clone(),
            id,
        }
    }
}

impl ParticipantRef {
    pub fn id(&self) -> &ParticipantId {
        &self.id
    }

    /// 03 server path: sets the player's presence in your game. It expires
    /// 120 s later: call again every 60 s while the player is in the session.
    /// Needs `session.manage`, a live lease, and the player's `presence.write`
    /// consent; guests are refused (`presence/guest_not_supported`).
    pub async fn set_presence(&self, presence: &Presence) -> Result<PresenceAck, Error> {
        set(self.session.client(), self.session.id(), &self.id, presence).await
    }

    /// 03 server path: removes the player's presence at once.
    pub async fn clear_presence(&self) -> Result<(), Error> {
        clear(self.session.client(), self.session.id(), &self.id).await
    }
}

impl Participant {
    /// As [`ParticipantRef::set_presence`].
    pub async fn set_presence(&self, presence: &Presence) -> Result<PresenceAck, Error> {
        set(self.client(), self.session_id(), &self.id, presence).await
    }

    /// As [`ParticipantRef::clear_presence`].
    pub async fn clear_presence(&self) -> Result<(), Error> {
        clear(self.client(), self.session_id(), &self.id).await
    }
}

fn path(session_id: &SessionId, participant: &ParticipantId) -> String {
    format!(
        "/v1/server/sessions/{}/participants/{}/presence",
        segment(session_id.as_str()),
        segment(participant.as_str())
    )
}

const ROUTE: &str = "PUT|DELETE /v1/server/sessions/{id}/participants/{participant}/presence";

async fn set(
    client: &Client,
    session_id: &SessionId,
    participant: &ParticipantId,
    presence: &Presence,
) -> Result<PresenceAck, Error> {
    let body = presence.to_body()?;
    // A PUT replaces the presence: retrying it is safe.
    let response: SetResponse = client
        .edge_send(
            reqwest::Method::PUT,
            ROUTE,
            &path(session_id, participant),
            Some(&body),
            None,
            true,
        )
        .await?;
    if !response.ok || response.expires_at <= 0 {
        return Err(Error::InvalidResponse("invalid presence answer"));
    }
    Ok(PresenceAck {
        expires_at: response.expires_at,
    })
}

async fn clear(
    client: &Client,
    session_id: &SessionId,
    participant: &ParticipantId,
) -> Result<(), Error> {
    let response: ClearResponse = client
        .edge_send(
            reqwest::Method::DELETE,
            ROUTE,
            &path(session_id, participant),
            None,
            None,
            true,
        )
        .await?;
    if !response.ok {
        return Err(Error::InvalidResponse("invalid presence answer"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_is_trimmed_and_only_has_set_fields() {
        let body = Presence::new()
            .details("  Ranked match ")
            .party(Party::new("p-1", 2, 4))
            .to_body()
            .unwrap();
        assert_eq!(
            body,
            json!({ "details": "Ranked match", "party": { "id": "p-1", "size": 2, "max": 4 } })
        );
        assert_eq!(Presence::new().to_body().unwrap(), json!({}));
    }

    #[test]
    fn bounds_are_checked_locally() {
        let long = "x".repeat(129);
        assert!(Presence::new().details(long).to_body().is_err());
        assert!(Presence::new().details("   ").to_body().is_err());
        assert!(Presence::new().location("y".repeat(65)).to_body().is_err());
        assert!(Presence::new().location("y".repeat(64)).to_body().is_ok());
        assert!(Presence::new().state("tab\there").to_body().is_err());
        assert!(Presence::new().started_at(0).to_body().is_err());
        assert!(Presence::new()
            .party(Party::new("bad id", 1, 2))
            .to_body()
            .is_err());
        assert!(Presence::new()
            .party(Party::new("ok", 3, 2))
            .to_body()
            .is_err());
        assert!(Presence::new()
            .party(Party::new("ok", 1, 65))
            .to_body()
            .is_err());
        // 128 characters, counted as characters, not bytes.
        assert!(Presence::new().details("é".repeat(128)).to_body().is_ok());
    }

    #[test]
    fn path_encodes_both_ids() {
        assert_eq!(
            path(
                &SessionId::new("gssn1.a/b"),
                &ParticipantId::new("gsp1.x y")
            ),
            "/v1/server/sessions/gssn1.a%2Fb/participants/gsp1.x%20y/presence"
        );
    }
}
