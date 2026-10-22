//! Sign-in, tokens, player assertions and sign-out (protocol 01).

use std::{
    collections::BTreeSet,
    fmt,
    future::Future,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::{sync::watch, time::Instant};
use url::Url;
use zeroize::Zeroizing;

use crate::{
    config::{lock, Client},
    error::{Error, ErrorCode, Service, SignInError},
    http::{endpoint, Body, Call},
    loopback::{Listener, Page},
    pkce::{normalize_scopes, random_base64url, valid_challenge, Pkce},
    storage::StoredSession,
};

/// Refresh when the access token has less than this left.
const REFRESH_MARGIN: Duration = Duration::from_secs(30);
/// The longest a restored access token is trusted without a refresh.
const MAX_RESTORED_FRESHNESS: i64 = 24 * 3600;
/// The server keeps an authorization request for 10 minutes.
const MAX_SIGN_IN_WAIT: Duration = Duration::from_secs(600);

/// The signed-in player.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    /// The only field to show in UI.
    pub display_name: String,
    /// Opaque and stable. Never show it in UI.
    pub subject: String,
    /// An object path (resolve it through bootstrap's media resolver), not a URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar_object_path: Option<String>,
}

/// An interactive sign-in request. Build with [`SignIn::new`] and the setters.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct SignIn {
    /// For example `["identity.basic"]`: 1–24 scopes, validated, deduplicated and sorted.
    pub scopes: Vec<String>,
    /// Path of the registered loopback redirect (default `/callback`). The host
    /// is always `127.0.0.1` and the port is chosen by the OS: a registered
    /// `http://127.0.0.1/<path>` matches any port (RFC 8252).
    pub redirect_path: String,
    /// How long to wait for the browser (default 5 min; at most 10, the
    /// server's limit).
    pub timeout: Duration,
    /// Open the system browser (default `true`). With `false`, show
    /// [`PendingSignIn::authorization_url`] to the player yourself.
    pub open_browser: bool,
}

impl SignIn {
    pub fn new(scopes: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            scopes: scopes.into_iter().map(Into::into).collect(),
            redirect_path: "/callback".into(),
            timeout: Duration::from_secs(300),
            open_browser: true,
        }
    }

    pub fn redirect_path(mut self, path: impl Into<String>) -> Self {
        self.redirect_path = path.into();
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn open_browser(mut self, open: bool) -> Self {
        self.open_browser = open;
        self
    }
}

/// What [`Client::restore`] found.
#[derive(Clone, Debug)]
pub enum Restore {
    SignedIn(User),
    SignedOut,
    /// The stored session is no longer valid (refresh refused, or the stored
    /// entry is unusable: `Other("stored_session_invalid")`). Sign in again.
    InteractionRequired {
        code: ErrorCode,
    },
}

/// Changes of the signed-in session, from [`Client::subscribe`].
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum AuthEvent {
    SignedIn(User),
    Refreshed(User),
    SignedOut,
    /// The refresh token was refused for good (`invalid_grant` /
    /// `invalid_client`): the stored session was cleared.
    SessionExpired {
        code: ErrorCode,
    },
}

/// A single-use player assertion (60 s) for the game server's admit call.
/// `Debug` prints `PlayerAssertion(****)`.
pub struct PlayerAssertion {
    value: Zeroizing<String>,
    expires_in: Duration,
}

impl PlayerAssertion {
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

impl fmt::Debug for PlayerAssertion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "PlayerAssertion(****, expires_in: {:?})",
            self.expires_in
        )
    }
}

/// A sign-in waiting for the browser. Dropping it (or [`PendingSignIn::cancel`])
/// closes the listener.
pub struct PendingSignIn {
    client: Client,
    listener: Listener,
    url: Url,
    redirect_uri: String,
    redirect_path: String,
    state: Zeroizing<String>,
    verifier: Zeroizing<String>,
    deadline: Instant,
}

impl PendingSignIn {
    /// The URL to open in the system browser. It carries the state and the
    /// PKCE challenge, never the verifier.
    pub fn authorization_url(&self) -> &Url {
        &self.url
    }

    /// Waits for the callback, exchanges the code, and stores the session.
    /// The browser tab shows the outcome once the exchange is done.
    pub async fn finish(self) -> Result<User, Error> {
        let Self {
            client,
            listener,
            redirect_uri,
            redirect_path,
            state,
            verifier,
            deadline,
            ..
        } = self;
        let callback = listener.wait(&redirect_path, &state, deadline).await?;
        drop(listener);
        let result = client
            .exchange(&callback.code, &redirect_uri, &verifier)
            .await;
        let page = if result.is_ok() {
            Page::SignedIn
        } else {
            Page::Failed
        };
        callback.responder.send(page).await;
        result
    }

    /// Closes the listener; the browser tab then shows a connection error.
    pub fn cancel(self) {}
}

impl fmt::Debug for PendingSignIn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PendingSignIn")
            .field("port", &self.listener.port())
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    token_type: String,
    expires_in: f64,
    refresh_token: String,
    #[serde(default)]
    scope: String,
    user: TokenUser,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TokenUser {
    subject: String,
    display_name: String,
    #[serde(default)]
    avatar_object_path: Option<String>,
}

impl TokenResponse {
    /// 01 §2: Bearer, both tokens non-empty, `expires_in > 0`, a subject and a
    /// display name. Also how long the access token lasts from now (the
    /// server's `expires_in`, for the monotonic freshness).
    fn into_session(self) -> Result<(StoredSession, Duration), Error> {
        let valid = self.token_type == "Bearer"
            && !self.access_token.is_empty()
            && !self.refresh_token.is_empty()
            && self.expires_in.is_finite()
            && self.expires_in > 0.0
            && !self.user.subject.is_empty();
        if !valid {
            return Err(Error::InvalidResponse("invalid token response"));
        }
        let scopes: BTreeSet<&str> = self.scope.split_whitespace().collect();
        let valid_for = Duration::from_secs_f64(self.expires_in.min(1.0e9));
        Ok((
            StoredSession {
                // Stored for the next launch only (see `Client::restore`).
                access_expires_at: now() + valid_for.as_secs() as i64,
                scopes: scopes.into_iter().map(str::to_owned).collect(),
                user: User {
                    display_name: self.user.display_name,
                    subject: self.user.subject,
                    avatar_object_path: self
                        .user
                        .avatar_object_path
                        .filter(|path| !path.is_empty()),
                },
                access_token: self.access_token,
                refresh_token: self.refresh_token,
            },
            valid_for,
        ))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AssertionResponse {
    player_assertion: String,
    expires_in: f64,
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

/// The service refused the player's access token: expired or revoked,
/// whatever this machine's clock said about it.
fn token_refused(error: &Error) -> bool {
    matches!(
        error.api_code(),
        Some(ErrorCode::LoginRequired | ErrorCode::PresenceTokenInvalid)
    )
}

/// A refusal that ends the session (§6).
fn ends_session(error: &Error) -> bool {
    matches!(
        error.api_code(),
        Some(ErrorCode::InvalidGrant | ErrorCode::InvalidClient)
    )
}

fn retry_pause() -> Duration {
    Duration::from_millis(250 + u64::from(jitter_byte()))
}

fn jitter_byte() -> u8 {
    let mut byte = [0u8; 1];
    let _ = getrandom::fill(&mut byte);
    byte[0]
}

#[cfg(feature = "browser")]
async fn open_browser(url: &Url) -> Result<(), SignInError> {
    let url = url.to_string();
    tokio::task::spawn_blocking(move || webbrowser::open(&url))
        .await
        .ok()
        .and_then(Result::ok)
        .ok_or(SignInError::BrowserUnavailable)
}

#[cfg(not(feature = "browser"))]
async fn open_browser(_url: &Url) -> Result<(), SignInError> {
    Err(SignInError::BrowserUnavailable)
}

impl Client {
    /// Loads the stored session. Refreshes it when the access token has less
    /// than 30 s left. Offline, a held session is kept: the error is returned
    /// and [`Client::is_signed_in`] stays true.
    pub async fn restore(&self) -> Result<Restore, Error> {
        let Some(stored) = self.store_call(|store| store.load()).await? else {
            *lock(&self.inner.session) = None;
            return Ok(Restore::SignedOut);
        };
        if !stored.is_valid() {
            *lock(&self.inner.session) = None;
            if let Err(error) = self.store_call(|store| store.clear()).await {
                tracing::warn!(%error, "the unusable stored session could not be cleared");
            }
            return Ok(Restore::InteractionRequired {
                code: ErrorCode::Other("stored_session_invalid".into()),
            });
        }
        *lock(&self.inner.session) = Some(stored.clone());
        // The stored expiry was written with this machine's clock, so its
        // difference from this clock is the remaining lifetime as far as this
        // machine can tell. It is read once, here; if the clock moved since,
        // a refused token is refreshed on first use (`with_access_token`).
        let remaining = (stored.access_expires_at - now()).clamp(0, MAX_RESTORED_FRESHNESS);
        self.mark_fresh(&stored, Duration::from_secs(remaining as u64));
        if self.is_fresh(&stored) {
            return Ok(Restore::SignedIn(stored.user.clone()));
        }
        match self.refreshed(&stored).await {
            Ok(session) => Ok(Restore::SignedIn(session.user.clone())),
            Err(error) if ends_session(&error) => Ok(Restore::InteractionRequired {
                code: error.api_code().cloned().expect("an API error"),
            }),
            Err(error) => Err(error),
        }
    }

    /// Full interactive sign-in: loopback listener, system browser, code exchange.
    pub async fn sign_in(&self, request: SignIn) -> Result<User, Error> {
        self.begin_sign_in(request).await?.finish().await
    }

    /// The same in two steps. Opens the browser when `request.open_browser` is
    /// true; otherwise show [`PendingSignIn::authorization_url`] yourself.
    pub async fn begin_sign_in(&self, request: SignIn) -> Result<PendingSignIn, Error> {
        let scopes = normalize_scopes(&request.scopes)?;
        let path = &request.redirect_path;
        let path_ok = path.starts_with('/')
            && path.len() <= 256
            && path.bytes().all(|b| {
                b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'~' | b'-')
            });
        if !path_ok {
            return Err(Error::InvalidInput {
                field: "redirect path",
                reason: "a path starting with / of A-Z a-z 0-9 / . _ ~ -, at most 256 characters",
            });
        }
        if request.timeout.is_zero() || request.timeout > MAX_SIGN_IN_WAIT {
            return Err(Error::InvalidInput {
                field: "sign-in timeout",
                reason: "more than 0 and at most 10 minutes",
            });
        }
        let pkce = Pkce::new()?;
        let state = random_base64url(32)?;
        let nonce = random_base64url(32)?;
        let listener = Listener::bind().await?;
        let redirect_uri = format!("http://127.0.0.1:{}{path}", listener.port());
        let mut url = endpoint(&self.inner.oauth, "/authorize");
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.inner.app_id)
            .append_pair("environment", self.inner.environment.as_str())
            .append_pair("redirect_uri", &redirect_uri)
            .append_pair("scope", &scopes.join(" "))
            .append_pair("state", &state)
            .append_pair("nonce", &nonce)
            .append_pair("code_challenge", &pkce.challenge)
            .append_pair("code_challenge_method", "S256");
        let pending = PendingSignIn {
            client: self.clone(),
            listener,
            url,
            redirect_uri,
            redirect_path: request.redirect_path.clone(),
            state,
            verifier: pkce.verifier,
            deadline: Instant::now() + request.timeout,
        };
        if request.open_browser {
            open_browser(pending.authorization_url()).await?;
        }
        Ok(pending)
    }

    /// The signed-in player, if a session is held.
    pub fn current_user(&self) -> Option<User> {
        lock(&self.inner.session)
            .as_ref()
            .map(|session| session.user.clone())
    }

    /// A session is held (no network needed; see §6 of the design note).
    pub fn is_signed_in(&self) -> bool {
        lock(&self.inner.session).is_some()
    }

    /// A single-use assertion for the game server's admit call (01 §5, 02 §3).
    /// Never retried.
    pub async fn player_assertion(&self, challenge: &str) -> Result<PlayerAssertion, Error> {
        if !valid_challenge(challenge) {
            return Err(Error::InvalidInput {
                field: "challenge",
                reason: "32-256 characters of A-Z a-z 0-9 . _ ~ -",
            });
        }
        let mut url = endpoint(&self.inner.auth_base, "/v1/auth/player-assertions");
        url.query_pairs_mut()
            .append_pair("client_id", &self.inner.app_id)
            .append_pair("environment", self.inner.environment.as_str());
        let body = json!({ "audience": "game_server", "challenge": challenge });
        let response: AssertionResponse = self
            .with_access_token(|token| {
                let url = url.clone();
                let body = &body;
                async move {
                    self.inner
                        .http
                        .send(Call {
                            service: Service::Auth,
                            method: Method::POST,
                            url,
                            route: "POST /v1/auth/player-assertions",
                            bearer: Some(&token),
                            body: Body::Json(body),
                        })
                        .await
                }
            })
            .await?;
        let value = Zeroizing::new(response.player_assertion);
        if value.is_empty() || !response.expires_in.is_finite() || response.expires_in <= 0.0 {
            return Err(Error::InvalidResponse("invalid player assertion"));
        }
        Ok(PlayerAssertion {
            value,
            expires_in: Duration::from_secs_f64(response.expires_in.min(1.0e9)),
        })
    }

    /// Clears the store first, then revokes the refresh token (01 §4). A failed
    /// revoke is returned, but the local session is gone either way.
    pub async fn sign_out(&self) -> Result<(), Error> {
        let held = lock(&self.inner.session).take();
        *lock(&self.inner.fresh_until) = None;
        let session = match held {
            Some(session) => Some(session),
            None => self.store_call(|store| store.load()).await.ok().flatten(),
        };
        let cleared = self.store_call(|store| store.clear()).await;
        let _ = self.inner.events.send(AuthEvent::SignedOut);
        let revoked = match &session {
            Some(session) => self.revoke(&session.refresh_token).await,
            None => Ok(()),
        };
        cleared?;
        revoked
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<AuthEvent> {
        self.inner.events.subscribe()
    }

    async fn exchange(
        &self,
        code: &str,
        redirect_uri: &str,
        verifier: &str,
    ) -> Result<User, Error> {
        let app_id = self.inner.app_id.as_str();
        let response = self
            .token_request(
                "POST /oauth/token (authorization_code)",
                &[
                    ("grant_type", "authorization_code"),
                    ("client_id", app_id),
                    ("code", code),
                    ("redirect_uri", redirect_uri),
                    ("code_verifier", verifier),
                ],
            )
            .await?;
        let (session, valid_for) = response.into_session()?;
        let user = session.user.clone();
        *lock(&self.inner.session) = Some(session.clone());
        self.mark_fresh(&session, valid_for);
        self.persist(&session).await;
        let _ = self.inner.events.send(AuthEvent::SignedIn(user.clone()));
        Ok(user)
    }

    async fn token_request(
        &self,
        route: &'static str,
        form: &[(&str, &str)],
    ) -> Result<TokenResponse, Error> {
        let mut url = endpoint(&self.inner.oauth, "/token");
        url.query_pairs_mut()
            .append_pair("client_id", &self.inner.app_id)
            .append_pair("environment", self.inner.environment.as_str());
        self.inner
            .http
            .send(Call {
                service: Service::Auth,
                method: Method::POST,
                url,
                route,
                bearer: None,
                body: Body::Form(form),
            })
            .await
    }

    async fn revoke(&self, refresh_token: &str) -> Result<(), Error> {
        let mut url = endpoint(&self.inner.oauth, "/revoke");
        url.query_pairs_mut()
            .append_pair("client_id", &self.inner.app_id)
            .append_pair("environment", self.inner.environment.as_str());
        let form = [
            ("token", refresh_token),
            ("client_id", self.inner.app_id.as_str()),
        ];
        let mut attempt = 1;
        loop {
            let result = self
                .inner
                .http
                .send_ignoring_body(Call {
                    service: Service::Auth,
                    method: Method::POST,
                    url: url.clone(),
                    route: "POST /oauth/revoke",
                    bearer: None,
                    body: Body::Form(&form),
                })
                .await;
            match result {
                Err(error) if attempt == 1 && error.transport_retryable() => {
                    tokio::time::sleep(retry_pause()).await;
                    attempt += 1;
                }
                other => return other,
            }
        }
    }

    /// Saves to the store; a failure keeps the session in memory for this run.
    async fn persist(&self, session: &StoredSession) {
        let session = session.clone();
        if let Err(error) = self.store_call(move |store| store.save(&session)).await {
            tracing::warn!(%error, "the session could not be saved; it is kept in memory for this run");
        }
    }

    /// A usable access token, refreshing first when needed.
    pub(crate) async fn valid_access_token(&self) -> Result<Zeroizing<String>, Error> {
        let held = lock(&self.inner.session)
            .clone()
            .ok_or(Error::SignInRequired)?;
        if self.is_fresh(&held) {
            return Ok(Zeroizing::new(held.access_token.clone()));
        }
        let session = self.refreshed(&held).await?;
        Ok(Zeroizing::new(session.access_token.clone()))
    }

    /// Runs `call` with the player's access token. When the service refuses
    /// the token as expired or invalid, refreshes once and runs it again: the
    /// local clock may have been wrong about the token's freshness.
    pub(crate) async fn with_access_token<T, F, Fut>(&self, call: F) -> Result<T, Error>
    where
        F: Fn(Zeroizing<String>) -> Fut,
        Fut: Future<Output = Result<T, Error>>,
    {
        let token = self.valid_access_token().await?;
        match call(token).await {
            Err(error) if token_refused(&error) => {
                let held = lock(&self.inner.session)
                    .clone()
                    .ok_or(Error::SignInRequired)?;
                tracing::debug!(%error, "the access token was refused; refreshing once");
                match self.refreshed(&held).await {
                    Ok(session) => call(Zeroizing::new(session.access_token.clone())).await,
                    // The session is over: say so.
                    Err(refresh) if ends_session(&refresh) => Err(refresh),
                    // The refresh could not run (offline, 5xx): the refusal stands.
                    Err(_) => Err(error),
                }
            }
            other => other,
        }
    }

    /// The held access token counts as fresh for `valid_for` from now
    /// (monotonic clock).
    fn mark_fresh(&self, session: &StoredSession, valid_for: Duration) {
        *lock(&self.inner.fresh_until) = Some((
            Zeroizing::new(session.access_token.clone()),
            Instant::now() + valid_for,
        ));
    }

    fn is_fresh(&self, session: &StoredSession) -> bool {
        !session.access_token.is_empty()
            && lock(&self.inner.fresh_until)
                .as_ref()
                .is_some_and(|(token, until)| {
                    token.as_str() == session.access_token
                        && *until > Instant::now() + REFRESH_MARGIN
                })
    }

    /// One refresh at a time: concurrent callers wait for the same one. It runs
    /// as its own task, so a rotated refresh token is saved even when every
    /// caller has gone away.
    async fn refreshed(&self, stale: &StoredSession) -> Result<StoredSession, Error> {
        let mut receiver = {
            let mut slot = lock(&self.inner.refresh);
            match slot.as_ref() {
                Some(receiver) => receiver.clone(),
                None => {
                    // A refresh may have finished since the caller looked.
                    match lock(&self.inner.session).as_ref() {
                        None => return Err(Error::SignInRequired),
                        Some(held) if held.refresh_token != stale.refresh_token => {
                            return Ok(held.clone())
                        }
                        Some(_) => {}
                    }
                    let (sender, receiver) = watch::channel(None);
                    *slot = Some(receiver.clone());
                    let client = self.clone();
                    let refresh_token = Zeroizing::new(stale.refresh_token.clone());
                    tokio::spawn(async move {
                        let result = client.refresh_now(&refresh_token).await;
                        *lock(&client.inner.refresh) = None;
                        let _ = sender.send(Some(result));
                    });
                    receiver
                }
            }
        };
        let outcome = receiver
            .wait_for(Option::is_some)
            .await
            .map_err(|_| Error::InvalidResponse("the token refresh stopped unexpectedly"))?
            .clone();
        outcome.unwrap_or(Err(Error::InvalidResponse(
            "the token refresh stopped unexpectedly",
        )))
    }

    /// The refresh itself: retried once after a network error or 5xx (the
    /// server's 5-minute rotation grace makes that safe).
    async fn refresh_now(&self, refresh_token: &str) -> Result<StoredSession, Error> {
        let app_id = self.inner.app_id.clone();
        let form = [
            ("grant_type", "refresh_token"),
            ("client_id", app_id.as_str()),
            ("refresh_token", refresh_token),
        ];
        let mut attempt = 1;
        let response = loop {
            match self
                .token_request("POST /oauth/token (refresh_token)", &form)
                .await
            {
                Err(error) if attempt == 1 && error.transport_retryable() => {
                    tracing::warn!(%error, "gridspawn token refresh failed; retrying once");
                    tokio::time::sleep(retry_pause()).await;
                    attempt += 1;
                }
                other => break other,
            }
        };
        let still_held = |held: &Option<StoredSession>| {
            held.as_ref()
                .is_some_and(|held| held.refresh_token == refresh_token)
        };
        match response.and_then(TokenResponse::into_session) {
            Ok((session, valid_for)) => {
                let replaced = {
                    let mut held = lock(&self.inner.session);
                    let replaced = still_held(&held);
                    if replaced {
                        *held = Some(session.clone());
                    }
                    replaced
                };
                if replaced {
                    self.mark_fresh(&session, valid_for);
                }
                if !replaced {
                    // Signed out (or in again) meanwhile: drop the new token.
                    let _ = self.revoke(&session.refresh_token).await;
                    return Err(Error::SignInRequired);
                }
                self.persist(&session).await;
                let _ = self
                    .inner
                    .events
                    .send(AuthEvent::Refreshed(session.user.clone()));
                Ok(session)
            }
            Err(error) => {
                if ends_session(&error) {
                    let ended = {
                        let mut held = lock(&self.inner.session);
                        let ended = still_held(&held);
                        if ended {
                            *held = None;
                        }
                        ended
                    };
                    if ended {
                        if let Err(store_error) = self.store_call(|store| store.clear()).await {
                            tracing::warn!(error = %store_error, "the expired session could not be cleared from the store");
                        }
                        let code = error.api_code().cloned().expect("an API error");
                        let _ = self.inner.events.send(AuthEvent::SessionExpired { code });
                    }
                }
                Err(error)
            }
        }
    }
}
