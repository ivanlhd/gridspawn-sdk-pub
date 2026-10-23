//! Godot 4 GDExtension for Gridspawn, built on `gridspawn-client` and
//! `gridspawn-voice`.
//!
//! Two node classes: `GridspawnClient`, which the addon's `gridspawn.tscn`
//! puts in an autoload named `Gridspawn`, and `GridspawnVoice` (in-engine
//! voice for an attached game session). Every method returns at once; the
//! work runs on the node's own tokio runtime (background threads), and the
//! results come back as signals, emitted from `_process` on the main thread.
//! No Godot API is called off the main thread.

use std::{
    future::Future,
    sync::{mpsc, Arc},
};

use godot::{classes::INode, prelude::*};
use gridspawn_client::{
    AttachedSession, AuthEvent, Client, Environment, Error, GrantEvent, MemoryTokenStore, Module,
    Presence, PresenceEvent, PresenceUpdater, RenewerHandle, Restore, SignIn, SignInMethod,
};
use tokio::{
    runtime::{Handle, Runtime},
    sync::OnceCell,
    task::JoinHandle,
};

mod voice;

struct GridspawnExtension;

#[gdextension]
unsafe impl ExtensionLibrary for GridspawnExtension {}

/// The application environment the game talks to.
#[derive(GodotConvert, Var, Export, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[godot(via = i64)]
pub enum GridspawnEnvironment {
    Development,
    Staging,
    #[default]
    Production,
}

/// Where the signed-in session is kept between launches.
#[derive(GodotConvert, Var, Export, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[godot(via = i64)]
pub enum TokenStorage {
    /// The OS credential store (Windows Credential Manager, macOS Keychain,
    /// Linux Secret Service); memory when none is available.
    #[default]
    OsSecure,
    /// This run only: the player signs in on each launch.
    Memory,
}

/// A result from the background, delivered on the main thread.
enum Outcome {
    SignedIn(String),
    SignInFailed(String, String),
    SignedOut,
    SessionExpired(String),
    AssertionReady(String),
    AssertionFailed(String, String),
    Attached(Box<Attachment>),
    AttachFailed(String, String),
    GrantRenewed(String),
    GrantExpired(String, String),
    PresenceSet(i64),
    PresenceCleared,
    PresenceFailed(String, String),
    Warning(String),
}

/// The presence updater and where its token comes from.
struct PresenceState {
    updater: PresenceUpdater,
    /// With the attached session's grant (else the signed-in player's token).
    from_session: bool,
    events: JoinHandle<()>,
}

struct Attachment {
    session: AttachedSession,
    renewer: RenewerHandle,
    events: JoinHandle<()>,
}

/// What a background call needs from the node's properties.
struct Config {
    app_id: String,
    environment: Environment,
    memory_store: bool,
    sdk_base_url: Option<url::Url>,
}

fn failure(error: &Error) -> (String, String) {
    (error.code(), error.to_string())
}

/// Connects once (bootstrap); later calls reuse the client.
async fn connect(
    cell: &OnceCell<Client>,
    config: Config,
    outcomes: &mpsc::Sender<Outcome>,
) -> Result<Client, Error> {
    let outcomes = outcomes.clone();
    cell.get_or_try_init(|| async move {
        let mut builder = Client::builder(config.app_id, config.environment);
        if config.memory_store {
            builder = builder.token_store(Arc::new(MemoryTokenStore::new()));
        }
        if let Some(url) = config.sdk_base_url {
            builder = builder.sdk_base_url(url);
        }
        let client = builder.connect().await?;
        let mut events = client.subscribe();
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(AuthEvent::SessionExpired { code }) => {
                        let _ = outcomes.send(Outcome::SessionExpired(code.as_str().to_owned()));
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        Ok(client)
    })
    .await
    .cloned()
}

/// The Gridspawn client node. Add `res://addons/gridspawn/gridspawn.tscn` as
/// an autoload named `Gridspawn`.
#[derive(GodotClass)]
#[class(base = Node)]
pub struct GridspawnClient {
    /// The application id from the Dev Hub.
    #[export]
    app_id: GString,
    #[export]
    environment: GridspawnEnvironment,
    /// Scopes for sign-in, for example `identity.basic`.
    #[export]
    scopes: PackedStringArray,
    /// Path of the registered loopback redirect (`http://127.0.0.1/callback`).
    #[export]
    redirect_path: GString,
    /// Always sign in with the browser, even when the Gridspawn app is open.
    #[export]
    browser_only: bool,
    #[export]
    token_storage: TokenStorage,
    /// Advanced: leave empty for production (`https://sdk.gridspawn.net`).
    #[export]
    sdk_base_url: GString,

    runtime: Option<Runtime>,
    client: Arc<OnceCell<Client>>,
    outcomes_tx: mpsc::Sender<Outcome>,
    outcomes_rx: mpsc::Receiver<Outcome>,
    sign_in_task: Option<JoinHandle<()>>,
    attachment: Option<Attachment>,
    presence: Option<PresenceState>,
    base: Base<Node>,
}

#[godot_api]
impl INode for GridspawnClient {
    fn init(base: Base<Node>) -> Self {
        let (outcomes_tx, outcomes_rx) = mpsc::channel();
        let mut scopes = PackedStringArray::new();
        scopes.push("identity.basic");
        Self {
            app_id: GString::new(),
            environment: GridspawnEnvironment::Production,
            scopes,
            redirect_path: "/callback".into(),
            browser_only: false,
            token_storage: TokenStorage::OsSecure,
            sdk_base_url: GString::new(),
            runtime: None,
            client: Arc::new(OnceCell::new()),
            outcomes_tx,
            outcomes_rx,
            sign_in_task: None,
            attachment: None,
            presence: None,
            base,
        }
    }

    fn process(&mut self, _delta: f64) {
        let outcomes: Vec<Outcome> = self.outcomes_rx.try_iter().collect();
        for outcome in outcomes {
            self.deliver(outcome);
        }
    }
}

#[godot_api]
impl GridspawnClient {
    #[signal]
    fn signed_in(display_name: GString);
    #[signal]
    fn sign_in_failed(code: GString, message: GString);
    #[signal]
    fn signed_out();
    #[signal]
    fn session_expired(code: GString);
    #[signal]
    fn assertion_ready(assertion: GString);
    #[signal]
    fn assertion_failed(code: GString, message: GString);
    #[signal]
    fn attached(session_id: GString, display_name: GString, capabilities: PackedStringArray);
    #[signal]
    fn attach_failed(code: GString, message: GString);
    #[signal]
    fn grant_renewed(module: GString);
    #[signal]
    fn grant_expired(module: GString, code: GString);
    #[signal]
    fn presence_set(expires_at: i64);
    #[signal]
    fn presence_cleared();
    #[signal]
    fn presence_failed(code: GString, message: GString);

    /// Loads the stored session → `signed_in` | `signed_out` | `sign_in_failed`.
    /// Offline with a stored session, `sign_in_failed("network", …)` comes
    /// while `is_signed_in()` stays true: the game may continue offline.
    #[func]
    fn restore(&mut self) {
        self.run(|config, cell, outcomes| async move {
            let outcome = match connect(&cell, config, &outcomes).await {
                Err(error) => sign_in_failure(&error),
                Ok(client) => match client.restore().await {
                    Ok(Restore::SignedIn(user)) => Outcome::SignedIn(user.display_name),
                    Ok(_) => Outcome::SignedOut,
                    Err(error) => sign_in_failure(&error),
                },
            };
            let _ = outcomes.send(outcome);
        });
    }

    /// Asks the Gridspawn app when it is open (the player confirms there),
    /// otherwise opens the system browser → `signed_in` | `sign_in_failed`.
    /// The player has 2 minutes to answer in the app. Declining there is
    /// `sign_in_failed("auth/access_denied", …)`, as in the browser.
    #[func]
    fn sign_in(&mut self) {
        if let Some(task) = self.sign_in_task.take() {
            task.abort();
        }
        let scopes: Vec<String> = self
            .scopes
            .as_slice()
            .iter()
            .map(|s| s.to_string())
            .collect();
        let request = SignIn::new(scopes)
            .redirect_path(self.redirect_path.to_string())
            .browser_only(self.browser_only);
        self.sign_in_task = self.run(|config, cell, outcomes| async move {
            let outcome = match connect(&cell, config, &outcomes).await {
                Err(error) => sign_in_failure(&error),
                Ok(client) => match client.sign_in(request).await {
                    Ok(user) => Outcome::SignedIn(user.display_name),
                    Err(error) => sign_in_failure(&error),
                },
            };
            let _ = outcomes.send(outcome);
        });
    }

    /// How the current sign-in happened: `"app"` (confirmed in the Gridspawn
    /// app), `"browser"`, or `""` when unknown (after `restore()`) or signed out.
    #[func]
    fn sign_in_method(&self) -> GString {
        match self.client.get().and_then(Client::sign_in_method) {
            Some(SignInMethod::App) => "app".into(),
            Some(SignInMethod::Browser) => "browser".into(),
            _ => GString::new(),
        }
    }

    /// Stops a sign-in in progress (closes the listener, or the connection to
    /// the Gridspawn app, whose confirmation then closes too) →
    /// `sign_in_failed("sign_in/cancelled", …)`.
    #[func]
    fn cancel_sign_in(&mut self) {
        if let Some(task) = self.sign_in_task.take() {
            if !task.is_finished() {
                task.abort();
                let _ = self.outcomes_tx.send(Outcome::SignInFailed(
                    "sign_in/cancelled".into(),
                    "Sign-in was cancelled.".into(),
                ));
            }
        }
    }

    /// Clears the stored session, then revokes it → `signed_out`. Stops
    /// re-sending a presence set with the player's token.
    #[func]
    fn sign_out(&mut self) {
        if self.presence.as_ref().is_some_and(|p| !p.from_session) {
            self.stop_presence();
        }
        self.run(|config, cell, outcomes| async move {
            match connect(&cell, config, &outcomes).await {
                Err(error) => {
                    let _ = outcomes.send(Outcome::Warning(format!("sign-out: {error}")));
                }
                Ok(client) => {
                    if let Err(error) = client.sign_out().await {
                        let _ = outcomes.send(Outcome::Warning(format!(
                            "signed out locally; revoking failed: {error}"
                        )));
                    }
                }
            }
            let _ = outcomes.send(Outcome::SignedOut);
        });
    }

    /// Whether a session is held (no network needed).
    #[func]
    fn is_signed_in(&self) -> bool {
        self.client.get().is_some_and(Client::is_signed_in)
    }

    /// The player's display name, or "" when signed out. The only identity
    /// to show in UI.
    #[func]
    fn player_display_name(&self) -> GString {
        self.client
            .get()
            .and_then(Client::current_user)
            .map(|user| GString::from(user.display_name.as_str()))
            .unwrap_or_default()
    }

    /// Whether a sign-in is remembered across launches (false with the
    /// memory store, or before the first call).
    #[func]
    fn sessions_persist(&self) -> bool {
        self.client.get().is_some_and(Client::persists_sessions)
    }

    /// A single-use assertion for your game server → `assertion_ready` | `assertion_failed`.
    #[func]
    fn player_assertion(&mut self, challenge: GString) {
        let challenge = challenge.to_string();
        self.run(|config, cell, outcomes| async move {
            let result = match connect(&cell, config, &outcomes).await {
                Err(error) => Err(error),
                Ok(client) => client.player_assertion(&challenge).await,
            };
            let _ = outcomes.send(match result {
                Ok(assertion) => Outcome::AssertionReady(assertion.into_string()),
                Err(error) => {
                    let (code, message) = failure(&error);
                    Outcome::AssertionFailed(code, message)
                }
            });
        });
    }

    /// Redeems a ticket from your game server → `attached` | `attach_failed`.
    /// Grants are renewed in the background until `detach()`.
    #[func]
    fn attach(&mut self, ticket: GString) {
        let ticket = ticket.to_string();
        self.run(|config, cell, outcomes| async move {
            let result = match connect(&cell, config, &outcomes).await {
                Err(error) => Err(error),
                Ok(client) => client.attach(&ticket).await,
            };
            let outcome = match result {
                Ok(session) => {
                    let mut grant_events = session.subscribe();
                    let forward = outcomes.clone();
                    let events = tokio::spawn(async move {
                        loop {
                            let event = match grant_events.recv().await {
                                Ok(event) => event,
                                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                    continue
                                }
                                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                            };
                            let outcome = match event {
                                GrantEvent::Renewed(module) => {
                                    Outcome::GrantRenewed(module.as_str().to_owned())
                                }
                                GrantEvent::Expired { module, code } => Outcome::GrantExpired(
                                    module.as_str().to_owned(),
                                    code.as_str().to_owned(),
                                ),
                                _ => continue,
                            };
                            let _ = forward.send(outcome);
                        }
                    });
                    let renewer = session.spawn_renewer();
                    Outcome::Attached(Box::new(Attachment {
                        session,
                        renewer,
                        events,
                    }))
                }
                Err(error) => {
                    let (code, message) = failure(&error);
                    Outcome::AttachFailed(code, message)
                }
            };
            let _ = outcomes.send(outcome);
        });
    }

    /// Renews every grant of the attached session now → `grant_renewed` | `grant_expired`.
    #[func]
    fn renew(&mut self) {
        let Some(session) = self.attachment.as_ref().map(|a| a.session.clone()) else {
            godot_warn!("Gridspawn: renew() without an attached session");
            return;
        };
        self.run(|_config, _cell, outcomes| async move {
            for module in session.capabilities().to_vec() {
                // Success and session end arrive as grant events; anything
                // else (network, 5xx) is only a warning: the renewer retries.
                if let Err(error) = session.renew(module).await {
                    let ended = matches!(
                        error.api().map(|api| &api.code),
                        Some(gridspawn_client::ErrorCode::GrantInvalid)
                            | Some(gridspawn_client::ErrorCode::SessionNotFound)
                    );
                    if !ended {
                        let _ =
                            outcomes.send(Outcome::Warning(format!("renewing {module}: {error}")));
                    }
                }
            }
        });
    }

    /// Leaves the attached session: stops renewing its grants (and re-sending
    /// a presence that used its grant).
    #[func]
    fn detach(&mut self) {
        if let Some(attachment) = self.attachment.take() {
            attachment.events.abort();
            attachment.renewer.stop();
        }
        if self.presence.as_ref().is_some_and(|p| p.from_session) {
            self.stop_presence();
        }
    }

    /// Shows what the player is doing in your game on their Gridspawn profile
    /// → `presence_set(expires_at)` | `presence_failed(code, message)`.
    /// Empty strings are left out (all empty shows "Playing <your game>").
    /// Re-sent every 60 s until `clear_presence()`; a later failure of the
    /// re-send also arrives as `presence_failed`. Uses the attached session's
    /// presence grant when it has one, otherwise the signed-in player (who
    /// must have signed in with the `presence.write` scope).
    #[func]
    fn set_presence(&mut self, details: GString, state: GString, location: GString) {
        let mut presence = Presence::new();
        for (value, apply) in [
            (
                details,
                Presence::details as fn(Presence, String) -> Presence,
            ),
            (state, Presence::state),
            (location, Presence::location),
        ] {
            let value = value.to_string();
            if !value.trim().is_empty() {
                presence = apply(presence, value);
            }
        }
        let Some(updater) = self.presence_updater() else {
            let _ = self.outcomes_tx.send(Outcome::PresenceFailed(
                "sign_in/required".into(),
                "Sign in (with presence.write) or attach a session with presence first.".into(),
            ));
            return;
        };
        self.run(|_config, _cell, outcomes| async move {
            let _ = outcomes.send(match updater.set(presence).await {
                Ok(ack) => Outcome::PresenceSet(ack.expires_at),
                Err(error) => {
                    let (code, message) = failure(&error);
                    Outcome::PresenceFailed(code, message)
                }
            });
        });
    }

    /// Removes the player's presence now and stops re-sending it
    /// → `presence_cleared` | `presence_failed(code, message)`.
    #[func]
    fn clear_presence(&mut self) {
        let Some(state) = self.presence.take() else {
            let _ = self.outcomes_tx.send(Outcome::PresenceCleared);
            return;
        };
        state.events.abort();
        let updater = state.updater;
        self.run(|_config, _cell, outcomes| async move {
            let _ = outcomes.send(match updater.clear().await {
                Ok(()) => Outcome::PresenceCleared,
                Err(error) => {
                    let (code, message) = failure(&error);
                    Outcome::PresenceFailed(code, message)
                }
            });
        });
    }
}

fn sign_in_failure(error: &Error) -> Outcome {
    let (code, message) = failure(error);
    Outcome::SignInFailed(code, message)
}

impl GridspawnClient {
    fn config(&self) -> Result<Config, String> {
        let base = self.sdk_base_url.to_string();
        let sdk_base_url = if base.trim().is_empty() {
            None
        } else {
            Some(url::Url::parse(base.trim()).map_err(|_| "sdk_base_url is not a URL".to_owned())?)
        };
        Ok(Config {
            app_id: self.app_id.to_string(),
            environment: match self.environment {
                GridspawnEnvironment::Development => Environment::Development,
                GridspawnEnvironment::Staging => Environment::Staging,
                GridspawnEnvironment::Production => Environment::Production,
            },
            memory_store: self.token_storage == TokenStorage::Memory,
            sdk_base_url,
        })
    }

    /// Runs a background task on the node's runtime.
    fn run<F, Fut>(&mut self, task: F) -> Option<JoinHandle<()>>
    where
        F: FnOnce(Config, Arc<OnceCell<Client>>, mpsc::Sender<Outcome>) -> Fut,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let config = match self.config() {
            Ok(config) => config,
            Err(message) => {
                godot_error!("Gridspawn: {message}");
                return None;
            }
        };
        let future = task(config, self.client.clone(), self.outcomes_tx.clone());
        Some(self.runtime().spawn(future))
    }

    /// The node's runtime, started on first use. `GridspawnVoice` shares it.
    fn runtime(&mut self) -> &Runtime {
        self.runtime.get_or_insert_with(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("gridspawn")
                .enable_all()
                .build()
                .expect("the Gridspawn runtime could not start")
        })
    }

    /// The presence updater for the current token source: the attached
    /// session's grant when it has presence, else the signed-in player. A new
    /// source replaces the updater (the old one stops re-sending). `None`
    /// before the client is connected.
    fn presence_updater(&mut self) -> Option<PresenceUpdater> {
        let session = self
            .attachment
            .as_ref()
            .filter(|a| a.session.capabilities().contains(&Module::Presence))
            .map(|a| a.session.clone());
        let from_session = session.is_some();
        if let Some(state) = &self.presence {
            if state.from_session == from_session {
                return Some(state.updater.clone());
            }
        }
        let updater = match session {
            Some(session) => session.presence(),
            None => self.client.get()?.presence(),
        };
        self.stop_presence();
        let mut events = updater.subscribe();
        let outcomes = self.outcomes_tx.clone();
        let task = self.runtime().spawn(async move {
            loop {
                match events.recv().await {
                    Ok(PresenceEvent::Stopped { code, message }) => {
                        let _ = outcomes.send(Outcome::PresenceFailed(code, message));
                    }
                    Ok(PresenceEvent::RetryFailed { code, message, .. }) => {
                        let _ = outcomes.send(Outcome::Warning(format!(
                            "presence re-send failed ({code}): {message}; retrying"
                        )));
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        self.presence = Some(PresenceState {
            updater: updater.clone(),
            from_session,
            events: task,
        });
        Some(updater)
    }

    /// Drops the updater: re-sending stops and the presence expires (120 s).
    fn stop_presence(&mut self) {
        if let Some(state) = self.presence.take() {
            state.events.abort();
        }
    }

    /// For `GridspawnVoice`: the client, the attached session and the runtime.
    pub(crate) fn voice_context(&mut self) -> Option<(Client, AttachedSession, Handle)> {
        let client = self.client.get()?.clone();
        let session = self.attachment.as_ref()?.session.clone();
        Some((client, session, self.runtime().handle().clone()))
    }

    /// Main thread: applies an outcome and emits its signal. Signals go
    /// through `base_mut()`, so handlers may call back into this node.
    fn deliver(&mut self, outcome: Outcome) {
        let s = |value: &str| GString::from(value).to_variant();
        match outcome {
            Outcome::SignedIn(name) => {
                self.base_mut().emit_signal("signed_in", &[s(&name)]);
            }
            Outcome::SignInFailed(code, message) => {
                self.base_mut()
                    .emit_signal("sign_in_failed", &[s(&code), s(&message)]);
            }
            Outcome::SignedOut => {
                self.base_mut().emit_signal("signed_out", &[]);
            }
            Outcome::SessionExpired(code) => {
                self.base_mut().emit_signal("session_expired", &[s(&code)]);
            }
            Outcome::AssertionReady(assertion) => {
                self.base_mut()
                    .emit_signal("assertion_ready", &[s(&assertion)]);
            }
            Outcome::AssertionFailed(code, message) => {
                self.base_mut()
                    .emit_signal("assertion_failed", &[s(&code), s(&message)]);
            }
            Outcome::Attached(attachment) => {
                self.detach();
                let session_id = attachment.session.id().to_owned();
                let display_name = attachment
                    .session
                    .participant()
                    .display_name
                    .clone()
                    .unwrap_or_default();
                let mut capabilities = PackedStringArray::new();
                for module in attachment.session.capabilities() {
                    capabilities.push(module.as_str());
                }
                self.attachment = Some(*attachment);
                self.base_mut().emit_signal(
                    "attached",
                    &[s(&session_id), s(&display_name), capabilities.to_variant()],
                );
            }
            Outcome::AttachFailed(code, message) => {
                self.base_mut()
                    .emit_signal("attach_failed", &[s(&code), s(&message)]);
            }
            Outcome::GrantRenewed(module) => {
                self.base_mut().emit_signal("grant_renewed", &[s(&module)]);
            }
            Outcome::GrantExpired(module, code) => {
                self.base_mut()
                    .emit_signal("grant_expired", &[s(&module), s(&code)]);
            }
            Outcome::PresenceSet(expires_at) => {
                self.base_mut()
                    .emit_signal("presence_set", &[expires_at.to_variant()]);
            }
            Outcome::PresenceCleared => {
                self.base_mut().emit_signal("presence_cleared", &[]);
            }
            Outcome::PresenceFailed(code, message) => {
                self.base_mut()
                    .emit_signal("presence_failed", &[s(&code), s(&message)]);
            }
            Outcome::Warning(message) => godot_warn!("Gridspawn: {message}"),
        }
    }
}

impl Drop for GridspawnClient {
    fn drop(&mut self) {
        self.detach();
        if let Some(runtime) = self.runtime.take() {
            // Never block the main thread on background work.
            runtime.shutdown_background();
        }
    }
}
