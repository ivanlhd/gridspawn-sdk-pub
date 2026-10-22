//! Player presence from the game client (protocol 03, client path): what the
//! player is doing in your game, shown on their Gridspawn profile, friends
//! list and club member lists.
//!
//! [`Client::presence`] uses the signed-in player's token (scope
//! `presence.write`); [`AttachedSession::presence`] uses the session's
//! presence grant. Either gives a [`PresenceUpdater`] that sends the presence
//! now and again every 60 s (it expires 120 s after the last update).

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use reqwest::Method;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use tokio::{sync::broadcast, task::JoinHandle};
use url::Url;
use zeroize::Zeroizing;

use crate::{
    config::{lock, Client, Module},
    error::{Error, Service},
    http::{Body, Call},
    session::AttachedSession,
};

/// Re-send interval (03: presence expires 120 s after the last update).
const RESEND: Duration = Duration::from_secs(60);
/// After a network error, timeout or 5xx without `Retry-After`.
const RETRY: Duration = Duration::from_secs(10);

/// What the player is doing (03 §Payload). Every field is optional: an empty
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
    /// Unix seconds: the presence disappears then unless it is sent again.
    pub expires_at: i64,
}

/// Background re-send results, from [`PresenceUpdater::subscribe`].
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum PresenceEvent {
    /// A re-send failed; it is tried again (`retry_in`).
    RetryFailed {
        code: String,
        message: String,
        retry_in: Duration,
    },
    /// A re-send was refused (for example `presence/token_invalid`,
    /// `presence/scope_denied`, `session/not_found`): re-sending stopped and
    /// the presence will expire. A later [`PresenceUpdater::set`] starts again.
    Stopped { code: String, message: String },
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

/// Which token authorizes the updates.
#[derive(Clone)]
enum Source {
    /// The signed-in player's OAuth access token (`presence.write`).
    Player(Client),
    /// The attached session's presence grant.
    Session(AttachedSession),
}

impl Source {
    fn client(&self) -> &Client {
        match self {
            Self::Player(client) => client,
            Self::Session(session) => session.client(),
        }
    }

    /// Runs `call` with the token. The player's token is refreshed once and
    /// the call repeated when the service refuses it (`presence/token_invalid`).
    async fn authorized<T, F, Fut>(&self, call: F) -> Result<T, Error>
    where
        F: Fn(Zeroizing<String>) -> Fut,
        Fut: std::future::Future<Output = Result<T, Error>>,
    {
        match self {
            Self::Player(client) => client.with_access_token(call).await,
            Self::Session(session) => {
                let grant = session.grant(Module::Presence).await?;
                call(Zeroizing::new(grant.as_str().to_owned())).await
            }
        }
    }

    fn url(&self) -> Result<Url, Error> {
        let client = self.client();
        let mut url = client.presence_url().cloned().ok_or(Error::InvalidInput {
            field: "presence",
            reason: "bootstrap advertises no presence endpoint for this app",
        })?;
        url.query_pairs_mut()
            .append_pair("appId", client.app_id())
            .append_pair("environment", client.environment().as_str());
        Ok(url)
    }

    async fn put(&self, body: &Value) -> Result<PresenceAck, Error> {
        let url = self.url()?;
        let response: SetResponse = self
            .authorized(|token| {
                let url = url.clone();
                async move {
                    self.client()
                        .inner
                        .http
                        .send(Call {
                            service: Service::Edge,
                            method: Method::PUT,
                            url,
                            route: "PUT {presence}",
                            bearer: Some(&token),
                            body: Body::Json(body),
                        })
                        .await
                }
            })
            .await?;
        if !response.ok || response.expires_at <= 0 {
            return Err(Error::InvalidResponse("invalid presence answer"));
        }
        Ok(PresenceAck {
            expires_at: response.expires_at,
        })
    }

    async fn delete(&self) -> Result<(), Error> {
        let url = self.url()?;
        let response: ClearResponse = self
            .authorized(|token| {
                let url = url.clone();
                async move {
                    self.client()
                        .inner
                        .http
                        .send(Call {
                            service: Service::Edge,
                            method: Method::DELETE,
                            url,
                            route: "DELETE {presence}",
                            bearer: Some(&token),
                            body: Body::None,
                        })
                        .await
                }
            })
            .await?;
        if !response.ok {
            return Err(Error::InvalidResponse("invalid presence answer"));
        }
        Ok(())
    }
}

impl Client {
    /// Presence updates with the signed-in player's token. The player must
    /// have signed in with the `presence.write` scope (and `presence.party`
    /// for a party).
    pub fn presence(&self) -> PresenceUpdater {
        PresenceUpdater::new(Source::Player(self.clone()))
    }
}

impl AttachedSession {
    /// Presence updates with this session's presence grant (the ticket must
    /// include the `presence` module). Guests are refused
    /// (`presence/guest_not_supported`).
    pub fn presence(&self) -> PresenceUpdater {
        PresenceUpdater::new(Source::Session(self.clone()))
    }
}

/// Sets and clears the player's presence, and re-sends the last one every
/// 60 s on the current tokio runtime. Dropping it stops the re-sends (the
/// presence then expires within 120 s). Cheap to clone; clones share the
/// re-send task.
///
/// Limits: 5 writes per 20 s per player and game (`RateLimited` with
/// `retry_after`), so set it when the activity changes, not every frame.
#[derive(Clone)]
pub struct PresenceUpdater {
    shared: Arc<Shared>,
}

struct Shared {
    source: Source,
    /// The re-send task for the current presence, if any.
    task: Mutex<Option<JoinHandle<()>>>,
    events: broadcast::Sender<PresenceEvent>,
    /// 60 s; shorter only in tests.
    every: Mutex<Duration>,
}

impl Drop for Shared {
    fn drop(&mut self) {
        if let Some(task) = lock(&self.task).take() {
            task.abort();
        }
    }
}

impl PresenceUpdater {
    fn new(source: Source) -> Self {
        let (events, _) = broadcast::channel(16);
        Self {
            shared: Arc::new(Shared {
                source,
                task: Mutex::new(None),
                events,
                every: Mutex::new(RESEND),
            }),
        }
    }

    /// Test hook: the re-send interval (60 s by default). The service expires
    /// presence 120 s after the last update, so keep it well under that.
    #[doc(hidden)]
    pub fn set_resend_interval(&self, every: Duration) {
        *lock(&self.shared.every) = every;
    }

    /// Sends `presence` now, then every 60 s until [`Self::clear`], another
    /// `set`, or the updater is dropped. Not retried here: a failed first send
    /// is returned (and re-sending is not started).
    pub async fn set(&self, presence: Presence) -> Result<PresenceAck, Error> {
        let body = presence.to_body()?;
        self.stop();
        let ack = self.shared.source.put(&body).await?;
        let weak = Arc::downgrade(&self.shared);
        let task = tokio::spawn(async move { resend(weak, body).await });
        if let Some(previous) = lock(&self.shared.task).replace(task) {
            previous.abort();
        }
        Ok(ack)
    }

    /// Stops re-sending and removes the presence now.
    pub async fn clear(&self) -> Result<(), Error> {
        self.stop();
        self.shared.source.delete().await
    }

    /// Background re-send failures.
    pub fn subscribe(&self) -> broadcast::Receiver<PresenceEvent> {
        self.shared.events.subscribe()
    }

    /// Whether a presence is being re-sent.
    pub fn is_active(&self) -> bool {
        lock(&self.shared.task)
            .as_ref()
            .is_some_and(|task| !task.is_finished())
    }

    fn stop(&self) {
        if let Some(task) = lock(&self.shared.task).take() {
            task.abort();
        }
    }
}

/// Re-sends `body` every 60 s. Retries network errors, timeouts, 429 and 5xx
/// (after `Retry-After`, or 10 s); stops on any other refusal.
async fn resend(shared: std::sync::Weak<Shared>, body: Value) {
    let every = match shared.upgrade() {
        Some(shared) => *lock(&shared.every),
        None => return,
    };
    let mut wait = every;
    loop {
        tokio::time::sleep(wait).await;
        let Some(shared) = shared.upgrade() else {
            return;
        };
        match shared.source.put(&body).await {
            Ok(_) => wait = every,
            Err(error) if error.is_retryable() => {
                wait = error
                    .api()
                    .and_then(|api| api.retry_after)
                    .unwrap_or(RETRY)
                    .min(every);
                tracing::warn!(%error, retry_in = ?wait, "gridspawn presence re-send failed");
                let _ = shared.events.send(PresenceEvent::RetryFailed {
                    code: error.code(),
                    message: error.to_string(),
                    retry_in: wait,
                });
            }
            Err(error) => {
                tracing::warn!(%error, "gridspawn presence re-send refused; stopping");
                let _ = shared.events.send(PresenceEvent::Stopped {
                    code: error.code(),
                    message: error.to_string(),
                });
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_is_trimmed_and_only_has_set_fields() {
        let body = Presence::new()
            .details("  Ranked match ")
            .location("Neon District")
            .started_at(1_792_310_400)
            .to_body()
            .unwrap();
        assert_eq!(
            body,
            json!({ "details": "Ranked match", "location": "Neon District", "startedAt": 1_792_310_400 })
        );
        assert_eq!(Presence::new().to_body().unwrap(), json!({}));
    }

    #[test]
    fn bounds_are_checked_locally() {
        assert!(Presence::new().details("x".repeat(129)).to_body().is_err());
        assert!(Presence::new().state(" ").to_body().is_err());
        assert!(Presence::new().location("y".repeat(65)).to_body().is_err());
        assert!(Presence::new().details("new\nline").to_body().is_err());
        assert!(Presence::new()
            .party(Party::new("ok", 0, 4))
            .to_body()
            .is_err());
        assert!(Presence::new()
            .party(Party::new("ok:1", 4, 4))
            .to_body()
            .is_ok());
    }
}
