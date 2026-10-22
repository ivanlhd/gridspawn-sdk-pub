//! Game sessions, admission and client tickets (protocol 02).

use std::{
    collections::BTreeMap,
    fmt,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use serde::Deserialize;
use serde_json::{json, Map, Value};
use zeroize::Zeroizing;

use crate::{
    client::{segment, Client},
    error::Error,
    types::{bounded, token_char, IdempotencyKey, Module},
};

/// 02 §1. Create with [`CreateSession::new`] and the builder methods.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct CreateSession {
    /// 1–256 characters: your own id for the match or room.
    pub external_session_id: String,
    /// 1–64 characters.
    pub region: Option<String>,
    /// 1–10000.
    pub max_participants: Option<u32>,
    /// At most 16 entries; keys 1–64 characters, values 0–256.
    pub metadata: BTreeMap<String, String>,
    pub idempotency_key: IdempotencyKey,
}

impl CreateSession {
    pub fn new(external_session_id: impl Into<String>, idempotency_key: IdempotencyKey) -> Self {
        Self {
            external_session_id: external_session_id.into(),
            region: None,
            max_participants: None,
            metadata: BTreeMap::new(),
            idempotency_key,
        }
    }

    pub fn region(mut self, region: impl Into<String>) -> Self {
        self.region = Some(region.into());
        self
    }

    pub fn max_participants(mut self, max: u32) -> Self {
        self.max_participants = Some(max);
        self
    }

    pub fn metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }

    fn validate(&self) -> Result<(), Error> {
        bounded(&self.external_session_id, 1, 256, "external session id")?;
        if let Some(region) = &self.region {
            bounded(region, 1, 64, "region")?;
        }
        if let Some(max) = self.max_participants {
            if !(1..=10_000).contains(&max) {
                return Err(Error::InvalidInput {
                    field: "max participants",
                    reason: "must be 1-10000",
                });
            }
        }
        if self.metadata.len() > 16 {
            return Err(Error::InvalidInput {
                field: "metadata",
                reason: "at most 16 entries",
            });
        }
        for (key, value) in &self.metadata {
            bounded(key, 1, 64, "metadata key")?;
            bounded(value, 0, 256, "metadata value")?;
        }
        Ok(())
    }
}

/// A session id from the edge (`gssn1.…`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionId(String);

impl SessionId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A participant id from the edge (`gsp1.…`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ParticipantId(String);

impl ParticipantId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ParticipantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The session's lease: it lasts 45 s and should be renewed every `renew_after`.
#[derive(Clone, Debug)]
pub struct Lease {
    pub session_id: SessionId,
    /// RFC 3339, as the edge sent it.
    pub expires_at: String,
    pub renew_after: Duration,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LeaseResponse {
    session_id: String,
    lease_expires_at: String,
    renew_after_seconds: f64,
}

fn parse_lease(response: LeaseResponse) -> Result<Lease, Error> {
    if response.session_id.is_empty()
        || response.lease_expires_at.is_empty()
        || !response.renew_after_seconds.is_finite()
        || response.renew_after_seconds <= 0.0
    {
        return Err(Error::InvalidResponse("invalid session lease"));
    }
    Ok(Lease {
        session_id: SessionId(response.session_id),
        expires_at: response.lease_expires_at,
        renew_after: Duration::from_secs_f64(response.renew_after_seconds),
    })
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Client {
    /// 02 §1: create a session (idempotent by its key).
    pub async fn create_session(&self, request: CreateSession) -> Result<Session, Error> {
        request.validate()?;
        let mut body = Map::new();
        body.insert(
            "externalSessionId".into(),
            json!(request.external_session_id),
        );
        if let Some(region) = &request.region {
            body.insert("region".into(), json!(region));
        }
        if let Some(max) = request.max_participants {
            body.insert("maxParticipants".into(), json!(max));
        }
        body.insert("metadata".into(), json!(request.metadata));
        let response: LeaseResponse = self
            .edge_post(
                "POST /v1/server/sessions",
                "/v1/server/sessions",
                &Value::Object(body),
                Some(&request.idempotency_key),
                true,
            )
            .await?;
        let lease = parse_lease(response)?;
        Ok(Session {
            client: self.clone(),
            id: lease.session_id.clone(),
            lease: Arc::new(Mutex::new(Some(lease))),
        })
    }

    /// A handle for a session created earlier (for example after a restart).
    pub fn session(&self, id: SessionId) -> Session {
        Session {
            client: self.clone(),
            id,
            lease: Arc::new(Mutex::new(None)),
        }
    }
}

async fn renew(client: &Client, id: &SessionId) -> Result<Lease, Error> {
    let response: LeaseResponse = client
        .edge_post(
            "POST /v1/server/sessions/{id}/renew",
            &format!("/v1/server/sessions/{}/renew", segment(id.as_str())),
            &json!({}),
            None,
            true,
        )
        .await?;
    parse_lease(response)
}

/// A game session on the SDK edge.
#[derive(Clone)]
pub struct Session {
    client: Client,
    id: SessionId,
    lease: Arc<Mutex<Option<Lease>>>,
}

impl Session {
    pub fn id(&self) -> &SessionId {
        &self.id
    }

    /// The last lease seen; `None` for a [`Client::session`] handle until it renews.
    pub fn lease(&self) -> Option<Lease> {
        lock(&self.lease).clone()
    }

    /// 02 §2: extend the lease.
    pub async fn renew(&self) -> Result<Lease, Error> {
        let lease = renew(&self.client, &self.id).await?;
        *lock(&self.lease) = Some(lease.clone());
        Ok(lease)
    }

    /// Renews every `renew_after` on the current tokio runtime. After a failure
    /// it tries again in min(5 s, max(1 s, renew_after / 3)). The task stops when
    /// the handle is dropped, or when every clone of this session or of the
    /// client is gone.
    pub fn spawn_renewer(&self) -> RenewerHandle {
        let client = Arc::downgrade(&self.client.inner);
        let cell = Arc::downgrade(&self.lease);
        let id = self.id.clone();
        let last_error = Arc::new(Mutex::new(None));
        let errors = last_error.clone();
        let mut delay = self
            .lease()
            .map_or(Duration::ZERO, |lease| lease.renew_after);
        let task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(delay).await;
                let (Some(inner), Some(cell)) = (client.upgrade(), cell.upgrade()) else {
                    break;
                };
                let client = Client { inner };
                match renew(&client, &id).await {
                    Ok(lease) => {
                        delay = lease.renew_after;
                        *lock(&cell) = Some(lease);
                        *lock(&errors) = None;
                    }
                    Err(error) => {
                        let base = lock(&cell)
                            .as_ref()
                            .map_or(Duration::from_secs(3), |lease| lease.renew_after);
                        delay = (base / 3).clamp(Duration::from_secs(1), Duration::from_secs(5));
                        tracing::warn!(session_id = %id, %error, retry_in = ?delay, "gridspawn session lease renewal failed");
                        *lock(&errors) = Some(error);
                    }
                }
            }
        });
        RenewerHandle { task, last_error }
    }

    /// 02 §3: admit a signed-in player with the assertion from their client.
    pub async fn admit_participant(&self, request: AdmitParticipant) -> Result<Participant, Error> {
        request.validate()?;
        let mut body = Map::new();
        body.insert("playerAssertion".into(), json!(request.player_assertion));
        body.insert("challenge".into(), json!(request.challenge));
        body.insert("externalPlayerId".into(), json!(request.external_player_id));
        if let Some(role) = &request.role {
            body.insert("role".into(), json!(role));
        }
        if let Some(team) = &request.team {
            body.insert("team".into(), json!(team));
        }
        let response: ParticipantResponse = self
            .client
            .edge_post(
                "POST /v1/server/sessions/{id}/participants",
                &format!(
                    "/v1/server/sessions/{}/participants",
                    segment(self.id.as_str())
                ),
                &Value::Object(body),
                Some(&request.idempotency_key),
                true,
            )
            .await?;
        self.participant(response)
    }

    /// 02 §4: admit a guest (when the app allows guests).
    pub async fn admit_guest(&self, request: AdmitGuest) -> Result<Participant, Error> {
        request.validate()?;
        let mut body = Map::new();
        body.insert("externalPlayerId".into(), json!(request.external_player_id));
        body.insert("displayName".into(), json!(request.display_name.trim()));
        if let Some(role) = &request.role {
            body.insert("role".into(), json!(role));
        }
        if let Some(team) = &request.team {
            body.insert("team".into(), json!(team));
        }
        let response: ParticipantResponse = self
            .client
            .edge_post(
                "POST /v1/server/sessions/{id}/guests",
                &format!("/v1/server/sessions/{}/guests", segment(self.id.as_str())),
                &Value::Object(body),
                Some(&request.idempotency_key),
                true,
            )
            .await?;
        self.participant(response)
    }

    fn participant(&self, response: ParticipantResponse) -> Result<Participant, Error> {
        let identity_kind = match response.identity_kind.as_str() {
            "gridspawn_user" => IdentityKind::GridspawnUser,
            "server_verified_guest" => IdentityKind::ServerVerifiedGuest,
            _ => return Err(Error::InvalidResponse("unknown identity kind")),
        };
        let guest_without_name = identity_kind == IdentityKind::ServerVerifiedGuest
            && response.display_name.as_deref().is_none_or(str::is_empty);
        if response.participant_id.is_empty() || response.subject.is_empty() || guest_without_name {
            return Err(Error::InvalidResponse("invalid participant"));
        }
        Ok(Participant {
            id: ParticipantId(response.participant_id),
            identity_kind,
            subject: response.subject,
            display_name: response.display_name,
            client: self.client.clone(),
            session_id: self.id.clone(),
        })
    }
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("id", &self.id)
            .field("lease", &self.lease())
            .finish()
    }
}

/// Stops the renewer task when stopped or dropped.
#[derive(Debug)]
pub struct RenewerHandle {
    task: tokio::task::JoinHandle<()>,
    last_error: Arc<Mutex<Option<Error>>>,
}

impl RenewerHandle {
    pub fn stop(self) {}

    /// The failure since the last successful renewal, if any.
    pub fn last_error(&self) -> Option<Error> {
        lock(&self.last_error).clone()
    }
}

impl Drop for RenewerHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// 02 §3.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct AdmitParticipant {
    /// From the client (protocol 01 §5); 1–8192 characters.
    pub player_assertion: String,
    /// The challenge you gave the client: 32–256 of `[A-Za-z0-9._~-]`.
    pub challenge: String,
    /// 1–256 characters.
    pub external_player_id: String,
    pub role: Option<String>,
    pub team: Option<String>,
    pub idempotency_key: IdempotencyKey,
}

impl AdmitParticipant {
    pub fn new(
        player_assertion: impl Into<String>,
        challenge: impl Into<String>,
        external_player_id: impl Into<String>,
        idempotency_key: IdempotencyKey,
    ) -> Self {
        Self {
            player_assertion: player_assertion.into(),
            challenge: challenge.into(),
            external_player_id: external_player_id.into(),
            role: None,
            team: None,
            idempotency_key,
        }
    }

    pub fn role(mut self, role: impl Into<String>) -> Self {
        self.role = Some(role.into());
        self
    }

    pub fn team(mut self, team: impl Into<String>) -> Self {
        self.team = Some(team.into());
        self
    }

    fn validate(&self) -> Result<(), Error> {
        if self.player_assertion.is_empty() || self.player_assertion.len() > 8192 {
            return Err(Error::InvalidInput {
                field: "player assertion",
                reason: "1-8192 characters",
            });
        }
        let challenge_ok = (32..=256).contains(&self.challenge.len())
            && self
                .challenge
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'~' | b'-'));
        if !challenge_ok {
            return Err(Error::InvalidInput {
                field: "challenge",
                reason: "32-256 characters of A-Z a-z 0-9 . _ ~ -",
            });
        }
        bounded(&self.external_player_id, 1, 256, "external player id")?;
        optional_label(&self.role, "role")?;
        optional_label(&self.team, "team")
    }
}

/// 02 §4.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct AdmitGuest {
    /// 1–256 characters, opaque, not an email.
    pub external_player_id: String,
    /// 1–64 characters after trimming, not an email, not equal to the external id.
    pub display_name: String,
    pub role: Option<String>,
    pub team: Option<String>,
    pub idempotency_key: IdempotencyKey,
}

impl AdmitGuest {
    pub fn new(
        external_player_id: impl Into<String>,
        display_name: impl Into<String>,
        idempotency_key: IdempotencyKey,
    ) -> Self {
        Self {
            external_player_id: external_player_id.into(),
            display_name: display_name.into(),
            role: None,
            team: None,
            idempotency_key,
        }
    }

    pub fn role(mut self, role: impl Into<String>) -> Self {
        self.role = Some(role.into());
        self
    }

    pub fn team(mut self, team: impl Into<String>) -> Self {
        self.team = Some(team.into());
        self
    }

    fn validate(&self) -> Result<(), Error> {
        let name = self.display_name.trim();
        bounded(&self.external_player_id, 1, 256, "external player id")?;
        bounded(name, 1, 64, "display name")?;
        if looks_like_email(name)
            || looks_like_email(&self.external_player_id)
            || self.external_player_id == name
        {
            return Err(Error::InvalidInput {
                field: "guest",
                reason: "use an opaque external id and a display name that is not an email",
            });
        }
        optional_label(&self.role, "role")?;
        optional_label(&self.team, "team")
    }
}

fn optional_label(value: &Option<String>, field: &'static str) -> Result<(), Error> {
    match value {
        Some(value) => bounded(value, 1, 64, field),
        None => Ok(()),
    }
}

fn looks_like_email(value: &str) -> bool {
    let Some(at) = value.find('@') else {
        return false;
    };
    if at == 0 || value.rfind('@') != Some(at) {
        return false;
    }
    let domain = &value[at + 1..];
    match domain.rfind('.') {
        Some(dot) => dot > 0 && domain.len() - dot > 2,
        None => false,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentityKind {
    GridspawnUser,
    ServerVerifiedGuest,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ParticipantResponse {
    participant_id: String,
    identity_kind: String,
    subject: String,
    #[serde(default)]
    display_name: Option<String>,
}

/// An admitted player or guest.
#[derive(Clone)]
pub struct Participant {
    pub id: ParticipantId,
    pub identity_kind: IdentityKind,
    pub subject: String,
    /// Always present for guests.
    pub display_name: Option<String>,
    client: Client,
    session_id: SessionId,
}

impl Participant {
    /// 02 §5: a single-use ticket for 1–3 modules. Never retried: each call
    /// issues a new ticket.
    pub async fn issue_ticket(&self, modules: &[Module]) -> Result<ClientTicket, Error> {
        let mut names: Vec<&str> = modules.iter().map(|module| module.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        if names.is_empty() || names.len() > 3 {
            return Err(Error::InvalidInput {
                field: "modules",
                reason: "1-3 different modules",
            });
        }
        let response: TicketResponse = self
            .client
            .edge_post(
                "POST /v1/server/sessions/{id}/participants/{participant}/tickets",
                &format!(
                    "/v1/server/sessions/{}/participants/{}/tickets",
                    segment(self.session_id.as_str()),
                    segment(self.id.as_str())
                ),
                &json!({ "modules": names }),
                None,
                false,
            )
            .await?;
        let ticket = Zeroizing::new(response.participant_ticket);
        let shape_ok = ticket
            .strip_prefix("gpt1.")
            .is_some_and(|rest| rest.len() == 43 && rest.bytes().all(token_char));
        if !shape_ok
            || response.token_type != "urn:gridspawn:participant-ticket"
            || !response.expires_in.is_finite()
            || response.expires_in <= 0.0
        {
            return Err(Error::InvalidResponse("invalid participant ticket"));
        }
        Ok(ClientTicket {
            value: ticket,
            expires_in: Duration::from_secs_f64(response.expires_in),
        })
    }
}

impl fmt::Debug for Participant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Participant")
            .field("id", &self.id)
            .field("identity_kind", &self.identity_kind)
            .field("subject", &self.subject)
            .field("display_name", &self.display_name)
            .finish()
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TicketResponse {
    participant_ticket: String,
    token_type: String,
    expires_in: f64,
}

/// A single-use client ticket (valid 120 s). Send it to that player's client
/// over the game's own connection. `Debug` prints `gpt1.****`.
pub struct ClientTicket {
    value: Zeroizing<String>,
    expires_in: Duration,
}

impl ClientTicket {
    pub fn as_str(&self) -> &str {
        &self.value
    }

    pub fn into_string(mut self) -> String {
        std::mem::take(&mut *self.value)
    }

    pub fn expires_in(&self) -> Duration {
        self.expires_in
    }
}

impl fmt::Debug for ClientTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "ClientTicket(gpt1.****, expires_in: {:?})",
            self.expires_in
        )
    }
}
