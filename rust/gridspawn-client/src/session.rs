//! Attaching to a game session with a participant ticket, and module grants
//! (protocol 02 §6–7).

use std::{
    collections::{BTreeSet, HashMap},
    fmt,
    sync::{Arc, Mutex},
    time::Duration,
};

use reqwest::Method;
use serde::Deserialize;
use serde_json::json;
use tokio::{sync::broadcast, time::Instant};
use zeroize::Zeroizing;

use crate::{
    config::{lock, Client, Module},
    error::{Error, ErrorCode, Service},
    http::{endpoint, segment, Body, Call},
};

/// Renew a grant when it has less than this left (TypeScript client: 60 s).
const RENEW_MARGIN: Duration = Duration::from_secs(60);
const FIRST_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(10);

/// How the participant was admitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentityKind {
    GridspawnUser,
    ServerVerifiedGuest,
}

/// The attached participant.
#[derive(Clone, Debug)]
pub struct Participant {
    pub id: String,
    pub identity_kind: IdentityKind,
    /// Opaque. Never show it in UI.
    pub subject: String,
    /// Always present for guests; optional for Gridspawn accounts.
    pub display_name: Option<String>,
}

/// A module grant: a short-lived bearer token for one Gridspawn module.
/// `Debug` redacts the token.
#[derive(Clone)]
pub struct ModuleGrant {
    token: Arc<Zeroizing<String>>,
    expires_at: Instant,
    scope: String,
}

impl ModuleGrant {
    pub fn as_str(&self) -> &str {
        &self.token
    }

    pub fn expires_at(&self) -> std::time::Instant {
        self.expires_at.into_std()
    }

    /// Time left before it expires (zero once expired).
    pub fn remaining(&self) -> Duration {
        self.expires_at.saturating_duration_since(Instant::now())
    }

    /// Space-separated scopes, e.g. `voice.join voice.position`.
    pub fn scope(&self) -> &str {
        &self.scope
    }
}

impl fmt::Debug for ModuleGrant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModuleGrant")
            .field("token", &"****")
            .field("scope", &self.scope)
            .field("remaining", &self.remaining())
            .finish()
    }
}

/// Grant changes, from [`AttachedSession::subscribe`].
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum GrantEvent {
    Renewed(Module),
    /// The grant can no longer be renewed: the session ended
    /// (`session/grant_invalid`, `session/not_found`), another refusal, or it
    /// expired while renewals failed (`Other("network")`, `Other("timeout")`, …).
    Expired {
        module: Module,
        code: ErrorCode,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AttachResponse {
    session_id: String,
    participant: ParticipantResponse,
    modules: Vec<String>,
    #[serde(default)]
    grants: HashMap<String, GrantResponse>,
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

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GrantResponse {
    access_token: String,
    token_type: String,
    expires_in: f64,
    scope: String,
}

impl GrantResponse {
    fn into_grant(self) -> Result<ModuleGrant, Error> {
        if self.token_type != "Bearer"
            || self.access_token.is_empty()
            || !self.expires_in.is_finite()
            || self.expires_in <= 0.0
        {
            return Err(Error::InvalidResponse("invalid module grant"));
        }
        Ok(ModuleGrant {
            token: Arc::new(Zeroizing::new(self.access_token)),
            expires_at: Instant::now() + Duration::from_secs_f64(self.expires_in.min(1.0e9)),
            scope: self.scope,
        })
    }
}

impl Client {
    /// Redeems a participant ticket from the game server (02 §6). Sends no
    /// Authorization header; never retried (the ticket is single-use).
    pub async fn attach(&self, ticket: &str) -> Result<AttachedSession, Error> {
        let shape_ok = ticket.strip_prefix("gpt1.").is_some_and(|rest| {
            rest.len() == 43
                && rest
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        });
        if !shape_ok {
            return Err(Error::InvalidInput {
                field: "participant ticket",
                reason: "gpt1. followed by 43 characters of A-Z a-z 0-9 _ -",
            });
        }
        let mut url = endpoint(&self.inner.api, "/sessions/attach");
        url.query_pairs_mut()
            .append_pair("appId", &self.inner.app_id)
            .append_pair("environment", self.inner.environment.as_str());
        let response: AttachResponse = self
            .inner
            .http
            .send(Call {
                service: Service::Edge,
                method: Method::POST,
                url,
                route: "POST /v1/sessions/attach",
                bearer: None,
                body: Body::Json(&json!({ "participantTicket": ticket })),
            })
            .await?;
        self.attached(response)
    }

    fn attached(&self, response: AttachResponse) -> Result<AttachedSession, Error> {
        let participant = response.participant;
        let identity_kind = match participant.identity_kind.as_str() {
            "gridspawn_user" => IdentityKind::GridspawnUser,
            "server_verified_guest" => IdentityKind::ServerVerifiedGuest,
            _ => return Err(Error::InvalidResponse("unknown identity kind")),
        };
        let guest_without_name = identity_kind == IdentityKind::ServerVerifiedGuest
            && participant
                .display_name
                .as_deref()
                .is_none_or(str::is_empty);
        if response.session_id.is_empty()
            || participant.participant_id.is_empty()
            || participant.subject.is_empty()
            || guest_without_name
        {
            return Err(Error::InvalidResponse("invalid participant"));
        }
        let mut grants_in = response.grants;
        let mut grants = HashMap::new();
        for name in &response.modules {
            let module = Module::from_wire(name).ok_or(Error::InvalidResponse("unknown module"))?;
            let grant = grants_in
                .remove(name)
                .ok_or(Error::InvalidResponse("a module has no grant"))?
                .into_grant()?;
            grants.insert(module, grant);
        }
        if grants.is_empty() {
            return Err(Error::InvalidResponse("no session capabilities"));
        }
        let mut capabilities: Vec<Module> = grants.keys().copied().collect();
        capabilities.sort();
        let renewals = capabilities
            .iter()
            .map(|module| (*module, tokio::sync::Mutex::new(())))
            .collect();
        let (events, _) = broadcast::channel(16);
        Ok(AttachedSession {
            inner: Arc::new(AttachedInner {
                client: self.clone(),
                id: response.session_id,
                participant: Participant {
                    id: participant.participant_id,
                    identity_kind,
                    subject: participant.subject,
                    display_name: participant.display_name,
                },
                capabilities,
                grants: Mutex::new(grants),
                renewals,
                events,
            }),
        })
    }
}

/// A game session this client is attached to. Cheap to clone.
#[derive(Clone)]
pub struct AttachedSession {
    inner: Arc<AttachedInner>,
}

struct AttachedInner {
    client: Client,
    id: String,
    participant: Participant,
    capabilities: Vec<Module>,
    grants: Mutex<HashMap<Module, ModuleGrant>>,
    /// One renewal per module at a time.
    renewals: HashMap<Module, tokio::sync::Mutex<()>>,
    events: broadcast::Sender<GrantEvent>,
}

impl AttachedSession {
    pub fn id(&self) -> &str {
        &self.inner.id
    }

    pub(crate) fn client(&self) -> &Client {
        &self.inner.client
    }

    pub fn participant(&self) -> &Participant {
        &self.inner.participant
    }

    /// The modules this session granted, sorted.
    pub fn capabilities(&self) -> &[Module] {
        &self.inner.capabilities
    }

    /// The current grant for a module, renewed first when it has less than
    /// 60 s left.
    pub async fn grant(&self, module: Module) -> Result<ModuleGrant, Error> {
        let current = self.current(module).ok_or(no_grant())?;
        if current.remaining() > RENEW_MARGIN {
            return Ok(current);
        }
        self.renew_from(module, Some(&current)).await
    }

    /// Renews a module grant now (02 §7). Not retried; the renewer retries.
    pub async fn renew(&self, module: Module) -> Result<ModuleGrant, Error> {
        let seen = self.current(module).ok_or(no_grant())?;
        self.renew_from(module, Some(&seen)).await
    }

    pub fn subscribe(&self) -> broadcast::Receiver<GrantEvent> {
        self.inner.events.subscribe()
    }

    /// Renews every grant at 60 s before it expires, on the current tokio
    /// runtime. Network errors, timeouts, 429 and 5xx are retried with backoff
    /// (0.5 s doubling to 10 s) until the grant expires; any other refusal, or
    /// expiry, emits [`GrantEvent::Expired`] and stops renewing that module.
    /// The task stops when the handle is dropped, when every module has
    /// stopped, or when every clone of this session is gone.
    pub fn spawn_renewer(&self) -> RenewerHandle {
        let weak = Arc::downgrade(&self.inner);
        let mut active: BTreeSet<Module> = self.inner.capabilities.iter().copied().collect();
        let last_error = Arc::new(Mutex::new(None));
        let errors = last_error.clone();
        let task = tokio::spawn(async move {
            let mut failures: HashMap<Module, u32> = HashMap::new();
            let mut retry_at: HashMap<Module, Instant> = HashMap::new();
            loop {
                let next = {
                    let Some(inner) = weak.upgrade() else { break };
                    let grants = lock(&inner.grants);
                    active
                        .iter()
                        .filter_map(|module| {
                            let grant = grants.get(module)?;
                            let due = retry_at.get(module).copied().unwrap_or_else(|| {
                                grant
                                    .expires_at
                                    .checked_sub(RENEW_MARGIN)
                                    .unwrap_or_else(Instant::now)
                            });
                            Some((due, *module))
                        })
                        .min()
                };
                let Some((due, module)) = next else { break };
                tokio::time::sleep_until(due).await;
                let Some(inner) = weak.upgrade() else { break };
                let session = AttachedSession { inner };
                let seen = session.current(module);
                match session.renew_from(module, seen.as_ref()).await {
                    Ok(_) => {
                        failures.remove(&module);
                        retry_at.remove(&module);
                        *lock(&errors) = None;
                    }
                    Err(error) => {
                        let code = error
                            .api_code()
                            .cloned()
                            .unwrap_or_else(|| ErrorCode::Other(error.code()));
                        let remaining = session
                            .current(module)
                            .map_or(Duration::ZERO, |grant| grant.remaining());
                        if !error.is_retryable() || remaining.is_zero() {
                            // renew_from has already reported the session's end.
                            if !ends_session(&code) {
                                let _ = session.inner.events.send(GrantEvent::Expired {
                                    module,
                                    code: code.clone(),
                                });
                            }
                            active.remove(&module);
                            retry_at.remove(&module);
                        } else {
                            let count = failures.entry(module).or_insert(0);
                            *count += 1;
                            let backoff = FIRST_BACKOFF
                                .saturating_mul(1 << (*count - 1).min(8))
                                .min(MAX_BACKOFF);
                            tracing::warn!(module = module.as_str(), %error, retry_in = ?backoff.min(remaining), "gridspawn grant renewal failed");
                            retry_at.insert(module, Instant::now() + backoff.min(remaining));
                        }
                        *lock(&errors) = Some(error);
                    }
                }
            }
        });
        RenewerHandle { task, last_error }
    }

    fn current(&self, module: Module) -> Option<ModuleGrant> {
        lock(&self.inner.grants).get(&module).cloned()
    }

    /// Renews unless another call already replaced `seen` while this one waited.
    async fn renew_from(
        &self,
        module: Module,
        seen: Option<&ModuleGrant>,
    ) -> Result<ModuleGrant, Error> {
        let gate = self.inner.renewals.get(&module).ok_or(no_grant())?;
        let _turn = gate.lock().await;
        let current = self.current(module).ok_or(no_grant())?;
        if seen.is_some_and(|seen| !Arc::ptr_eq(&seen.token, &current.token)) {
            return Ok(current);
        }
        let client = &self.inner.client.inner;
        let mut url = endpoint(
            &client.api,
            &format!(
                "/sessions/{}/grants/{}/renew",
                segment(&self.inner.id),
                module.as_str()
            ),
        );
        url.query_pairs_mut()
            .append_pair("appId", &client.app_id)
            .append_pair("environment", client.environment.as_str());
        let result = client
            .http
            .send::<GrantResponse>(Call {
                service: Service::Edge,
                method: Method::POST,
                url,
                route: "POST /v1/sessions/{id}/grants/{module}/renew",
                bearer: Some(current.as_str()),
                body: Body::None,
            })
            .await
            .and_then(GrantResponse::into_grant);
        match result {
            Ok(grant) => {
                lock(&self.inner.grants).insert(module, grant.clone());
                let _ = self.inner.events.send(GrantEvent::Renewed(module));
                Ok(grant)
            }
            Err(error) => {
                if let Some(code) = error.api_code().filter(|code| ends_session(code)) {
                    let _ = self.inner.events.send(GrantEvent::Expired {
                        module,
                        code: code.clone(),
                    });
                }
                Err(error)
            }
        }
    }
}

fn ends_session(code: &ErrorCode) -> bool {
    matches!(code, ErrorCode::GrantInvalid | ErrorCode::SessionNotFound)
}

fn no_grant() -> Error {
    Error::InvalidInput {
        field: "module",
        reason: "this session has no grant for it",
    }
}

impl fmt::Debug for AttachedSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AttachedSession")
            .field("id", &self.inner.id)
            .field("participant", &self.inner.participant)
            .field("capabilities", &self.inner.capabilities)
            .finish()
    }
}

/// Stops the grant renewer when stopped or dropped.
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

    /// Whether the renewer has stopped by itself (every module expired or stopped).
    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }
}

impl Drop for RenewerHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}
