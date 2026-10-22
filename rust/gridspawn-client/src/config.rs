//! Configuration, bootstrap and the [`Client`] handle.

use std::{
    fmt,
    str::FromStr,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use reqwest::Method;
use serde::Deserialize;
use tokio::sync::{broadcast, watch};
use url::Url;

use crate::{
    auth::AuthEvent,
    error::{Error, Service},
    http::{endpoint, Body, Call, Http},
    storage::{MemoryTokenStore, StoreError, StoredSession, TokenStore},
};

const DEFAULT_SDK_BASE: &str = "https://sdk.gridspawn.net";
const PROTOCOL_VERSION: u32 = 1;

/// Which of the application's environments the game talks to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Environment {
    Development,
    Staging,
    Production,
}

impl Environment {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Development => "development",
            Self::Staging => "staging",
            Self::Production => "production",
        }
    }
}

impl FromStr for Environment {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self, Error> {
        match value {
            "development" => Ok(Self::Development),
            "staging" => Ok(Self::Staging),
            "production" => Ok(Self::Production),
            _ => Err(Error::InvalidInput {
                field: "environment",
                reason: "must be development, staging or production",
            }),
        }
    }
}

impl fmt::Display for Environment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A Gridspawn module a game session can grant (protocol 02 §5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum Module {
    Presence,
    Voice,
    Interakt,
}

impl Module {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Presence => "presence",
            Self::Voice => "voice",
            Self::Interakt => "interakt",
        }
    }

    pub(crate) fn from_wire(value: &str) -> Option<Self> {
        match value {
            "presence" => Some(Self::Presence),
            "voice" => Some(Self::Voice),
            "interakt" => Some(Self::Interakt),
            _ => None,
        }
    }
}

impl fmt::Display for Module {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Configures a [`Client`]; [`ClientBuilder::connect`] checks bootstrap.
pub struct ClientBuilder {
    app_id: String,
    environment: Environment,
    sdk_base: Url,
    store: Option<Arc<dyn TokenStore>>,
    timeout: Duration,
}

impl ClientBuilder {
    /// Default `https://sdk.gridspawn.net`.
    pub fn sdk_base_url(mut self, url: Url) -> Self {
        self.sdk_base = url;
        self
    }

    /// Where the session is kept between launches. Default: the OS credential
    /// store when it is available (feature `os-store`, on by default), else
    /// [`MemoryTokenStore`]. See [`Client::persists_sessions`].
    pub fn token_store(mut self, store: Arc<dyn TokenStore>) -> Self {
        self.store = Some(store);
        self
    }

    /// Per-request timeout (default 10 s).
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Calls bootstrap and checks the app, the environment and protocol 1.
    pub async fn connect(self) -> Result<Client, Error> {
        let app_id_ok = (3..=128).contains(&self.app_id.len())
            && self
                .app_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        if !app_id_ok {
            return Err(Error::InvalidInput {
                field: "app id",
                reason: "3-128 characters of A-Z a-z 0-9 _ -",
            });
        }
        if !secure(&self.sdk_base) {
            return Err(Error::InvalidInput {
                field: "SDK base URL",
                reason: "must be https (http only on a loopback address)",
            });
        }
        let http = Http::new(self.timeout)?;
        let bootstrap = bootstrap(&http, &self.sdk_base, &self.app_id, self.environment).await?;
        if !bootstrap.ok {
            return Err(Error::Bootstrap("ok is not true"));
        }
        if bootstrap.application.app_id != self.app_id {
            return Err(Error::Bootstrap("the application id differs"));
        }
        if bootstrap.environment != self.environment.as_str() {
            return Err(Error::Bootstrap("the environment differs"));
        }
        if bootstrap.protocol.selected != PROTOCOL_VERSION {
            return Err(Error::Bootstrap("protocol 1 was not selected"));
        }
        let parse = |value: &str, what: &'static str| {
            Url::parse(value)
                .ok()
                .filter(secure)
                .ok_or(Error::Bootstrap(what))
        };
        let oauth = parse(
            &bootstrap.endpoints.oauth,
            "endpoints.oauth is not a secure URL",
        )?;
        let api = parse(
            &bootstrap.endpoints.api,
            "endpoints.api is not a secure URL",
        )?;
        // 01 §5: assertions live at {base}/v1/auth/…; base is the OAuth endpoint without /oauth.
        let auth_base = oauth
            .as_str()
            .trim_end_matches('/')
            .strip_suffix("/oauth")
            .and_then(|base| Url::parse(base).ok())
            .unwrap_or_else(|| self.sdk_base.clone());
        let mut modules: Vec<Module> = bootstrap
            .modules
            .iter()
            .filter_map(|module| Module::from_wire(module))
            .collect();
        modules.sort();
        modules.dedup();
        let store = match self.store {
            Some(store) => store,
            None => default_store(&self.app_id, self.environment).await,
        };
        let (events, _) = broadcast::channel(16);
        Ok(Client {
            inner: Arc::new(Inner {
                app_id: self.app_id,
                environment: self.environment,
                application_name: bootstrap.application.display_name,
                modules,
                oauth,
                api,
                auth_base,
                http,
                store,
                session: Mutex::new(None),
                refresh: Mutex::new(None),
                events,
            }),
        })
    }
}

async fn bootstrap(
    http: &Http,
    base: &Url,
    app_id: &str,
    environment: Environment,
) -> Result<Bootstrap, Error> {
    let mut url = endpoint(base, "/v1/bootstrap");
    url.query_pairs_mut()
        .append_pair("appId", app_id)
        .append_pair("environment", environment.as_str())
        .append_pair("protocolVersion", &PROTOCOL_VERSION.to_string())
        .append_pair("sdkVersion", env!("CARGO_PKG_VERSION"));
    // Bootstrap is a plain GET: one retry after a network error or 5xx.
    let mut attempt = 1;
    loop {
        let result = http
            .send(Call {
                service: Service::Edge,
                method: Method::GET,
                url: url.clone(),
                route: "GET /v1/bootstrap",
                bearer: None,
                body: Body::None,
            })
            .await;
        match result {
            Err(error) if attempt == 1 && error.transport_retryable() => {
                tokio::time::sleep(Duration::from_millis(300)).await;
                attempt += 1;
            }
            other => return other,
        }
    }
}

/// https, or http on a loopback address (local tests).
fn secure(url: &Url) -> bool {
    match url.scheme() {
        "https" => url.host().is_some(),
        "http" => matches!(
            url.host_str(),
            Some("127.0.0.1") | Some("[::1]") | Some("localhost")
        ),
        _ => false,
    }
}

#[cfg(feature = "os-store")]
async fn default_store(app_id: &str, environment: Environment) -> Arc<dyn TokenStore> {
    let available = tokio::task::spawn_blocking(crate::storage::OsTokenStore::available)
        .await
        .unwrap_or(false);
    if available {
        Arc::new(crate::storage::OsTokenStore::new(app_id, environment))
    } else {
        tracing::info!(
            "no OS credential store is available; the session is kept in memory for this run"
        );
        Arc::new(MemoryTokenStore::new())
    }
}

#[cfg(not(feature = "os-store"))]
async fn default_store(_app_id: &str, _environment: Environment) -> Arc<dyn TokenStore> {
    Arc::new(MemoryTokenStore::new())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Bootstrap {
    ok: bool,
    application: BootstrapApplication,
    environment: String,
    protocol: BootstrapProtocol,
    #[serde(default)]
    modules: Vec<String>,
    endpoints: BootstrapEndpoints,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BootstrapApplication {
    app_id: String,
    #[serde(default)]
    display_name: String,
}

#[derive(Deserialize)]
struct BootstrapProtocol {
    selected: u32,
}

#[derive(Deserialize)]
struct BootstrapEndpoints {
    api: String,
    oauth: String,
}

/// The shared result of one in-flight refresh.
pub(crate) type RefreshResult = Option<Result<StoredSession, Error>>;

/// A connected game client. Cheap to clone; share one per process.
#[derive(Clone)]
pub struct Client {
    pub(crate) inner: Arc<Inner>,
}

pub(crate) struct Inner {
    pub app_id: String,
    pub environment: Environment,
    pub application_name: String,
    pub modules: Vec<Module>,
    pub oauth: Url,
    pub api: Url,
    pub auth_base: Url,
    pub http: Http,
    pub store: Arc<dyn TokenStore>,
    /// The held session (§6: "signed in" means this is `Some`).
    pub session: Mutex<Option<StoredSession>>,
    /// The refresh in flight, if any; concurrent callers wait for the same one.
    pub refresh: Mutex<Option<watch::Receiver<RefreshResult>>>,
    pub events: broadcast::Sender<AuthEvent>,
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Client {
    pub fn builder(app_id: impl Into<String>, environment: Environment) -> ClientBuilder {
        ClientBuilder {
            app_id: app_id.into(),
            environment,
            sdk_base: Url::parse(DEFAULT_SDK_BASE).expect("default SDK URL"),
            store: None,
            timeout: Duration::from_secs(10),
        }
    }

    pub fn app_id(&self) -> &str {
        &self.inner.app_id
    }

    pub fn environment(&self) -> Environment {
        self.inner.environment
    }

    /// The application's display name from bootstrap.
    pub fn application_name(&self) -> &str {
        &self.inner.application_name
    }

    /// Modules bootstrap reported for this application and environment.
    pub fn modules(&self) -> &[Module] {
        &self.inner.modules
    }

    /// Whether a signed-in session survives a restart: `false` with the
    /// in-memory fallback, where the player signs in on each launch.
    pub fn persists_sessions(&self) -> bool {
        self.inner.store.persistent()
    }

    // Extension point for protocol 03 (Presence): a `presence()` accessor will
    // be added here once its route exists. Not in phase 1.

    /// Runs a token store call on the blocking pool.
    pub(crate) async fn store_call<T, F>(&self, call: F) -> Result<T, Error>
    where
        T: Send + 'static,
        F: FnOnce(&dyn TokenStore) -> Result<T, StoreError> + Send + 'static,
    {
        let store = self.inner.store.clone();
        tokio::task::spawn_blocking(move || call(store.as_ref()))
            .await
            .map_err(|_| StoreError::new("the token store stopped unexpectedly"))?
            .map_err(Error::from)
    }
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("app_id", &self.inner.app_id)
            .field("environment", &self.inner.environment)
            .field("signed_in", &self.is_signed_in())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secure_urls() {
        for (url, ok) in [
            ("https://sdk.gridspawn.net", true),
            ("http://127.0.0.1:8080/v1", true),
            ("http://localhost:1", true),
            ("http://sdk.gridspawn.net", false),
            ("ftp://sdk.gridspawn.net", false),
        ] {
            assert_eq!(secure(&Url::parse(url).unwrap()), ok, "{url}");
        }
    }

    #[test]
    fn environments() {
        for environment in [
            Environment::Development,
            Environment::Staging,
            Environment::Production,
        ] {
            assert_eq!(
                environment.as_str().parse::<Environment>().unwrap(),
                environment
            );
        }
        assert!("prod".parse::<Environment>().is_err());
    }
}
