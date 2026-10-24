use std::{fmt, sync::Arc, time::Duration};

use reqwest::Method;
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::Value;
use tokio::sync::Mutex;
use url::Url;

use crate::{
    club::CachedGrant,
    credential::Credential,
    error::{Error, Service},
    game_data::GameShared,
    http::{Auth, Call, Http},
    types::{token_char, Environment, IdempotencyKey, Module, RetryPolicy},
};

const DEFAULT_SDK_BASE: &str = "https://sdk.gridspawn.net";
/// Not in bootstrap yet (protocol 05 §Setup).
const DEFAULT_CLUB_POSTS: &str =
    "https://presence-gateway.gridspawn.net/core/v1/game-servers/posts";
const PROTOCOL_VERSION: u32 = 1;

/// Configures a [`Client`]; [`ClientBuilder::connect`] checks bootstrap.
pub struct ClientBuilder {
    app_id: String,
    environment: Environment,
    credential: Credential,
    sdk_base: Url,
    club_posts: Url,
    timeout: Duration,
    retry: RetryPolicy,
}

impl ClientBuilder {
    /// Default `https://sdk.gridspawn.net`.
    pub fn sdk_base_url(mut self, url: Url) -> Self {
        self.sdk_base = url;
        self
    }

    /// Default `https://presence-gateway.gridspawn.net/core/v1/game-servers/posts`.
    pub fn club_posts_url(mut self, url: Url) -> Self {
        self.club_posts = url;
        self
    }

    /// Per-request timeout (default 10 s).
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn retry_policy(mut self, policy: RetryPolicy) -> Self {
        self.retry = policy;
        self
    }

    /// Calls bootstrap and checks the app, the environment and protocol 1.
    pub async fn connect(self) -> Result<Client, Error> {
        if !(3..=128).contains(&self.app_id.len()) || !self.app_id.bytes().all(token_char) {
            return Err(Error::InvalidInput {
                field: "app id",
                reason: "3-128 characters of A-Z a-z 0-9 _ -",
            });
        }
        let http = Http::new(self.timeout, self.retry)?;
        let mut url = endpoint(&self.sdk_base, "/v1/bootstrap");
        url.query_pairs_mut()
            .append_pair("appId", &self.app_id)
            .append_pair("environment", self.environment.as_str())
            .append_pair("protocolVersion", &PROTOCOL_VERSION.to_string())
            .append_pair("sdkVersion", env!("CARGO_PKG_VERSION"));
        let bootstrap: Bootstrap = http
            .send(Call {
                service: Service::Edge,
                method: Method::GET,
                url,
                route: "GET /v1/bootstrap",
                auth: Auth::None,
                body: None,
                idempotency_key: None,
                headers: &[],
                retry: true,
            })
            .await?;
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
        let modules = bootstrap
            .modules
            .iter()
            .filter_map(|module| Module::from_wire(module))
            .collect();
        Ok(Client {
            inner: Arc::new(Inner {
                app_id: self.app_id,
                environment: self.environment,
                credential: self.credential,
                sdk_base: self.sdk_base,
                club_posts: self.club_posts,
                http,
                modules,
                grant: Mutex::new(None),
                game: GameShared::default(),
            }),
        })
    }
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
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BootstrapApplication {
    app_id: String,
}

#[derive(Deserialize)]
struct BootstrapProtocol {
    selected: u32,
}

/// A connected server client. Cheap to clone; share one per process.
#[derive(Clone)]
pub struct Client {
    pub(crate) inner: Arc<Inner>,
}

pub(crate) struct Inner {
    app_id: String,
    environment: Environment,
    credential: Credential,
    sdk_base: Url,
    pub(crate) club_posts: Url,
    pub(crate) http: Http,
    modules: Vec<Module>,
    pub(crate) grant: Mutex<Option<CachedGrant>>,
    /// The game-data writer: one per client (see `Client::game`).
    pub(crate) game: GameShared,
}

impl Client {
    pub fn builder(
        app_id: impl Into<String>,
        environment: Environment,
        credential: Credential,
    ) -> ClientBuilder {
        ClientBuilder {
            app_id: app_id.into(),
            environment,
            credential,
            sdk_base: Url::parse(DEFAULT_SDK_BASE).expect("default SDK URL"),
            club_posts: Url::parse(DEFAULT_CLUB_POSTS).expect("default posting URL"),
            timeout: Duration::from_secs(10),
            retry: RetryPolicy::default(),
        }
    }

    pub fn app_id(&self) -> &str {
        &self.inner.app_id
    }

    pub fn environment(&self) -> Environment {
        self.inner.environment
    }

    /// Modules bootstrap reported for this application and environment.
    pub fn modules(&self) -> &[Module] {
        &self.inner.modules
    }

    /// POST to the SDK edge with the server credential.
    pub(crate) async fn edge_post<T: DeserializeOwned>(
        &self,
        route: &'static str,
        path: &str,
        body: &Value,
        idempotency_key: Option<&IdempotencyKey>,
        retry: bool,
    ) -> Result<T, Error> {
        self.edge_send(
            Method::POST,
            route,
            path,
            Some(body),
            idempotency_key,
            retry,
        )
        .await
    }

    /// Any method to the SDK edge with the server credential.
    pub(crate) async fn edge_send<T: DeserializeOwned>(
        &self,
        method: Method,
        route: &'static str,
        path: &str,
        body: Option<&Value>,
        idempotency_key: Option<&IdempotencyKey>,
        retry: bool,
    ) -> Result<T, Error> {
        self.edge_send_counted(method, route, path, body, idempotency_key, retry)
            .await
            .0
    }

    /// As `edge_send`, with the number of attempts made.
    pub(crate) async fn edge_send_counted<T: DeserializeOwned>(
        &self,
        method: Method,
        route: &'static str,
        path: &str,
        body: Option<&Value>,
        idempotency_key: Option<&IdempotencyKey>,
        retry: bool,
    ) -> (Result<T, Error>, u32) {
        self.edge_call(method, route, path, body, idempotency_key, &[], retry)
            .await
    }

    /// As `edge_send_counted`, with further request headers.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn edge_call<T: DeserializeOwned>(
        &self,
        method: Method,
        route: &'static str,
        path: &str,
        body: Option<&Value>,
        idempotency_key: Option<&IdempotencyKey>,
        headers: &[(&'static str, &'static str)],
        retry: bool,
    ) -> (Result<T, Error>, u32) {
        self.inner
            .http
            .send_counted(Call {
                service: Service::Edge,
                method,
                url: endpoint(&self.inner.sdk_base, path),
                route,
                auth: Auth::Server(&self.inner.credential),
                body,
                idempotency_key: idempotency_key.map(IdempotencyKey::as_str),
                headers,
                retry,
            })
            .await
    }
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("app_id", &self.inner.app_id)
            .field("environment", &self.inner.environment)
            .field("credential_id", &self.inner.credential.id())
            .finish()
    }
}

/// `base` + `path` (path already percent-encoded).
fn endpoint(base: &Url, path: &str) -> Url {
    Url::parse(&format!("{}{path}", base.as_str().trim_end_matches('/'))).expect("endpoint URL")
}

/// Percent-encodes one path segment.
pub(crate) fn segment(value: &str) -> String {
    value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}
