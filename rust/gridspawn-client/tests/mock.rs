//! Mock tests: wiremock plays sdk-auth and the SDK edge, and a scripted
//! "browser" completes the loopback sign-in. No network beyond 127.0.0.1.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use gridspawn_client::{
    AuthEvent, Client, Environment, Error, ErrorCode, GrantEvent, IdentityKind, MemoryTokenStore,
    Module, PendingSignIn, Restore, Service, SignIn, SignInError, StoreError, StoredSession,
    TokenStore, User,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wiremock::{
    matchers::{body_json, header, method, path, query_param},
    Mock, MockServer, Request, Respond, ResponseTemplate,
};

const APP: &str = "game-test";
const TICKET: &str = "gpt1.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const CHALLENGE: &str = "challenge-0123456789-0123456789-abcdef";

// ---- helpers ----------------------------------------------------------------

fn bootstrap_body(uri: &str, oauth: &str) -> Value {
    json!({
        "ok": true,
        "requestId": "req-boot",
        "application": { "appId": APP, "displayName": "Test Game" },
        "environment": "development",
        "protocol": { "selected": 1, "minimum": 1, "maximum": 1 },
        "modules": ["voice", "presence", "maps"],
        "endpoints": {
            "api": format!("{uri}/v1"),
            "oauth": oauth,
            "voice": format!("{uri}/v1/sdk/voice"),
            "mediaResolver": format!("{uri}/v1/media/resolve")
        }
    })
}

async fn server() -> MockServer {
    let server = MockServer::start().await;
    let oauth = format!("{}/oauth", server.uri());
    Mock::given(method("GET"))
        .and(path("/v1/bootstrap"))
        .and(query_param("appId", APP))
        .and(query_param("environment", "development"))
        .and(query_param("protocolVersion", "1"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(bootstrap_body(&server.uri(), &oauth)),
        )
        .mount(&server)
        .await;
    server
}

async fn client(server: &MockServer, store: Arc<dyn TokenStore>) -> Client {
    Client::builder(APP, Environment::Development)
        .sdk_base_url(server.uri().parse().unwrap())
        .token_store(store)
        .timeout(Duration::from_secs(3))
        .connect()
        .await
        .expect("connect")
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn user() -> User {
    User {
        display_name: "Player One".into(),
        subject: "sub-1".into(),
        avatar_object_path: None,
    }
}

fn stored(access_left: i64, refresh: &str) -> StoredSession {
    StoredSession {
        access_token: "access-0".into(),
        refresh_token: refresh.into(),
        access_expires_at: now() + access_left,
        scopes: vec!["identity.basic".into()],
        user: user(),
    }
}

fn store_with(session: StoredSession) -> Arc<MemoryTokenStore> {
    let store = Arc::new(MemoryTokenStore::new());
    store.save(&session).unwrap();
    store
}

fn token_body(access: &str, refresh: &str, expires_in: u64) -> Value {
    json!({
        "access_token": access,
        "token_type": "Bearer",
        "expires_in": expires_in,
        "refresh_token": refresh,
        "scope": "voice.join identity.basic",
        "user": { "subject": "sub-1", "displayName": "Player One", "avatarObjectPath": "avatars/p1.png" }
    })
}

fn auth_error(status: u16, oauth: &str, code: &str) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(json!({
        "error": oauth,
        "error_description": "described",
        "code": code,
        "requestId": "req-auth"
    }))
}

fn edge_error(status: u16, code: &str) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(json!({
        "ok": false,
        "error": { "code": code, "message": "described" },
        "requestId": "req-edge"
    }))
}

async fn requests(server: &MockServer, route: &str) -> Vec<Request> {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path() == route)
        .collect()
}

fn form(request: &Request) -> HashMap<String, String> {
    url::form_urlencoded::parse(&request.body)
        .into_owned()
        .collect()
}

async fn assert_no_origin(server: &MockServer) {
    for request in server.received_requests().await.unwrap() {
        assert!(
            request.headers.get("origin").is_none(),
            "Origin sent to {}",
            request.url.path()
        );
    }
}

fn api(error: Error) -> gridspawn_client::ApiError {
    match error {
        Error::Api(api) => *api,
        other => panic!("expected an API error, got {other:?}"),
    }
}

async fn next_event<T: Clone>(events: &mut tokio::sync::broadcast::Receiver<T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("an event within 5 s")
        .expect("event")
}

/// What the authorization URL carried.
struct Authorize {
    params: HashMap<String, String>,
    redirect_uri: String,
    state: String,
}

fn authorize(pending: &PendingSignIn) -> Authorize {
    let url = pending.authorization_url();
    assert_eq!(url.path(), "/oauth/authorize");
    let params: HashMap<String, String> = url.query_pairs().into_owned().collect();
    Authorize {
        redirect_uri: params["redirect_uri"].clone(),
        state: params["state"].clone(),
        params,
    }
}

fn browser() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

/// The scripted browser: one GET, returns status and page.
async fn visit(url: String) -> (u16, String) {
    let response = browser().get(url).send().await.expect("browser request");
    let status = response.status().as_u16();
    (status, response.text().await.unwrap())
}

/// Begins a sign-in and lets the browser come back with `query`.
async fn sign_in_with(
    client: &Client,
    query: impl FnOnce(&Authorize) -> String,
) -> (Result<User, Error>, u16, String) {
    let pending = client
        .begin_sign_in(SignIn::new(["identity.basic"]).open_browser(false))
        .await
        .unwrap();
    let authorize = authorize(&pending);
    let url = format!("{}?{}", authorize.redirect_uri, query(&authorize));
    let visit = tokio::spawn(visit(url));
    let result = pending.finish().await;
    let (status, page) = visit.await.unwrap();
    (result, status, page)
}

fn good_callback(authorize: &Authorize) -> String {
    format!("code=code-1&state={}", authorize.state)
}

// ---- sign-in ----------------------------------------------------------------

#[tokio::test]
async fn sign_in_completes_with_a_scripted_browser() {
    let server = server().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(query_param("client_id", APP))
        .and(query_param("environment", "development"))
        .and(header("content-type", "application/x-www-form-urlencoded"))
        .respond_with(ResponseTemplate::new(200).set_body_json(token_body(
            "access-1",
            "refresh-1",
            600,
        )))
        .expect(1)
        .mount(&server)
        .await;
    let store = Arc::new(MemoryTokenStore::new());
    let client = client(&server, store.clone()).await;
    assert_eq!(client.application_name(), "Test Game");
    assert_eq!(client.modules(), [Module::Presence, Module::Voice]);
    assert_eq!(
        client.voice_url().map(|url| url.path()),
        Some("/v1/sdk/voice")
    );
    assert!(!client.persists_sessions());
    let mut events = client.subscribe();

    let pending = client
        .begin_sign_in(
            SignIn::new(["voice.join", " identity.basic", "voice.join"]).open_browser(false),
        )
        .await
        .unwrap();
    let auth = authorize(&pending);
    let p = &auth.params;
    assert_eq!(p["response_type"], "code");
    assert_eq!(p["client_id"], APP);
    assert_eq!(p["environment"], "development");
    assert_eq!(p["scope"], "identity.basic voice.join");
    assert_eq!(p["code_challenge_method"], "S256");
    assert_eq!(p["code_challenge"].len(), 43);
    assert_eq!(p["state"].len(), 43);
    assert_eq!(p["nonce"].len(), 43);
    assert!(!p.contains_key("code_verifier"));
    assert!(auth.redirect_uri.starts_with("http://127.0.0.1:"));
    assert!(auth.redirect_uri.ends_with("/callback"));
    let debug = format!("{pending:?}");
    assert!(
        !debug.contains(&auth.state) && !debug.contains(&p["code_challenge"]),
        "{debug}"
    );

    let callback = auth.redirect_uri.clone();
    let origin = callback.trim_end_matches("/callback").to_owned();
    let state = auth.state.clone();
    let browser = tokio::spawn(async move {
        // Noise first: the listener answers 404 and keeps waiting.
        assert_eq!(visit(format!("{origin}/favicon.ico")).await.0, 404);
        assert_eq!(
            visit(format!("{origin}/other?code=x&state={state}"))
                .await
                .0,
            404
        );
        visit(format!("{callback}?code=code-1&state={state}")).await
    });
    let user = pending.finish().await.expect("sign-in");
    let (status, page) = browser.await.unwrap();
    assert_eq!(status, 200);
    assert!(page.contains("You're signed in"), "{page}");
    assert!(
        !page.contains("<script") && !page.contains("http"),
        "{page}"
    );

    assert_eq!(user.display_name, "Player One");
    assert_eq!(user.subject, "sub-1");
    assert_eq!(user.avatar_object_path.as_deref(), Some("avatars/p1.png"));
    assert!(client.is_signed_in());
    assert_eq!(client.current_user(), Some(user.clone()));

    let token = &requests(&server, "/oauth/token").await[0];
    let sent = form(token);
    assert_eq!(sent["grant_type"], "authorization_code");
    assert_eq!(sent["client_id"], APP);
    assert_eq!(sent["code"], "code-1");
    assert_eq!(sent["redirect_uri"], auth.redirect_uri);
    let verifier = &sent["code_verifier"];
    assert_eq!(verifier.len(), 86);
    assert_eq!(
        URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
        p["code_challenge"]
    );

    let saved = store.load().unwrap().expect("saved");
    assert_eq!(saved.refresh_token, "refresh-1");
    assert_eq!(saved.scopes, ["identity.basic", "voice.join"]);
    assert!(saved.access_expires_at >= now() + 590);
    assert!(matches!(next_event(&mut events).await, AuthEvent::SignedIn(u) if u == user));
    assert_no_origin(&server).await;
}

#[tokio::test]
async fn callback_failures_end_the_sign_in_without_an_exchange() {
    let server = server().await;
    let client = client(&server, Arc::new(MemoryTokenStore::new())).await;

    let (result, status, page) =
        sign_in_with(&client, |a| format!("code=c&state={}x", a.state)).await;
    assert!(matches!(
        result,
        Err(Error::SignIn(SignInError::StateMismatch))
    ));
    assert_eq!(status, 400);
    assert!(page.contains("didn't finish"));

    let (result, status, page) = sign_in_with(&client, |a| {
        format!("error=access_denied&state={}", a.state)
    })
    .await;
    match result {
        Err(Error::SignIn(SignInError::Denied(code))) => assert_eq!(code, "access_denied"),
        other => panic!("{other:?}"),
    }
    assert_eq!(status, 200);
    assert!(page.contains("not approved"));
    assert_eq!(
        Error::SignIn(SignInError::Denied("access_denied".into())).code(),
        "auth/access_denied"
    );

    let (result, _, _) = sign_in_with(&client, |a| format!("state={}", a.state)).await;
    assert!(matches!(
        result,
        Err(Error::SignIn(SignInError::MissingCode))
    ));

    let (result, _, _) = sign_in_with(&client, |_| "code=c".into()).await;
    assert!(matches!(
        result,
        Err(Error::SignIn(SignInError::StateMismatch))
    ));

    // Another host name resolving to 127.0.0.1 (DNS rebinding) is refused.
    let pending = client
        .begin_sign_in(SignIn::new(["identity.basic"]).open_browser(false))
        .await
        .unwrap();
    let auth = authorize(&pending);
    let address = auth
        .redirect_uri
        .trim_start_matches("http://")
        .trim_end_matches("/callback")
        .to_owned();
    let state = auth.state.clone();
    let raw = tokio::spawn(async move {
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let request =
            format!("GET /callback?code=c&state={state} HTTP/1.1\r\nHost: evil.example\r\n\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut answer = String::new();
        stream.read_to_string(&mut answer).await.unwrap();
        answer
    });
    assert!(matches!(
        pending.finish().await,
        Err(Error::SignIn(SignInError::RedirectMismatch))
    ));
    assert!(raw.await.unwrap().starts_with("HTTP/1.1 400"));

    assert!(requests(&server, "/oauth/token").await.is_empty());
    assert!(!client.is_signed_in());
}

#[tokio::test]
async fn timeout_cancel_and_drop_close_the_listener() {
    let server = server().await;
    let client = client(&server, Arc::new(MemoryTokenStore::new())).await;
    let address = |pending: &PendingSignIn| {
        authorize(pending)
            .redirect_uri
            .trim_start_matches("http://")
            .trim_end_matches("/callback")
            .to_owned()
    };

    let pending = client
        .begin_sign_in(
            SignIn::new(["identity.basic"])
                .open_browser(false)
                .timeout(Duration::from_millis(300)),
        )
        .await
        .unwrap();
    let port = address(&pending);
    assert!(matches!(
        pending.finish().await,
        Err(Error::SignIn(SignInError::TimedOut))
    ));
    assert!(tokio::net::TcpStream::connect(&port).await.is_err());

    let pending = client
        .begin_sign_in(SignIn::new(["identity.basic"]).open_browser(false))
        .await
        .unwrap();
    let port = address(&pending);
    assert!(tokio::net::TcpStream::connect(&port).await.is_ok());
    pending.cancel();
    assert!(tokio::net::TcpStream::connect(&port).await.is_err());

    let pending = client
        .begin_sign_in(SignIn::new(["identity.basic"]).open_browser(false))
        .await
        .unwrap();
    let port = address(&pending);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), pending.finish())
            .await
            .is_err()
    );
    assert!(tokio::net::TcpStream::connect(&port).await.is_err());
}

#[tokio::test]
async fn local_input_is_checked_before_anything_is_sent() {
    let server = server().await;
    let client = client(&server, Arc::new(MemoryTokenStore::new())).await;
    let invalid = |result: Result<PendingSignIn, Error>, expected: &str| match result {
        Err(Error::InvalidInput { field, .. }) => assert_eq!(field, expected),
        other => panic!("{other:?}"),
    };
    invalid(
        client
            .begin_sign_in(SignIn::new(Vec::<String>::new()))
            .await,
        "scopes",
    );
    invalid(
        client.begin_sign_in(SignIn::new(["bad scope"])).await,
        "scopes",
    );
    invalid(
        client
            .begin_sign_in(SignIn::new(["identity.basic"]).redirect_path("callback"))
            .await,
        "redirect path",
    );
    invalid(
        client
            .begin_sign_in(SignIn::new(["identity.basic"]).redirect_path("/call back"))
            .await,
        "redirect path",
    );
    invalid(
        client
            .begin_sign_in(SignIn::new(["identity.basic"]).timeout(Duration::ZERO))
            .await,
        "sign-in timeout",
    );
    invalid(
        client
            .begin_sign_in(SignIn::new(["identity.basic"]).timeout(Duration::from_secs(601)))
            .await,
        "sign-in timeout",
    );
    assert!(matches!(
        client.player_assertion("too-short").await,
        Err(Error::InvalidInput {
            field: "challenge",
            ..
        })
    ));
    assert!(matches!(
        client.player_assertion(&format!("{CHALLENGE}+")).await,
        Err(Error::InvalidInput {
            field: "challenge",
            ..
        })
    ));
    for ticket in [
        "gpt2.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "gpt1.short",
        &format!("{TICKET}A"),
        "gpt1.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA+",
    ] {
        assert!(
            matches!(
                client.attach(ticket).await,
                Err(Error::InvalidInput {
                    field: "participant ticket",
                    ..
                })
            ),
            "{ticket}"
        );
    }
    let requests = server.received_requests().await.unwrap();
    assert!(requests
        .iter()
        .all(|request| request.url.path() == "/v1/bootstrap"));
}

#[tokio::test]
async fn token_responses_are_validated() {
    let server = server().await;
    for body in [
        json!({ "access_token": "a", "token_type": "mac", "expires_in": 600, "refresh_token": "r", "scope": "", "user": { "subject": "s", "displayName": "P" } }),
        json!({ "access_token": "a", "token_type": "Bearer", "expires_in": 600, "refresh_token": "", "scope": "", "user": { "subject": "s", "displayName": "P" } }),
        json!({ "access_token": "a", "token_type": "Bearer", "expires_in": 0, "refresh_token": "r", "scope": "", "user": { "subject": "s", "displayName": "P" } }),
        json!({ "access_token": "a", "token_type": "Bearer", "expires_in": 600, "refresh_token": "r", "scope": "" }),
    ] {
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .up_to_n_times(1)
            .mount(&server)
            .await;
    }
    let store = Arc::new(MemoryTokenStore::new());
    let client = client(&server, store.clone()).await;
    for _ in 0..4 {
        let (result, status, _) = sign_in_with(&client, good_callback).await;
        assert!(
            matches!(result, Err(Error::InvalidResponse(_))),
            "{result:?}"
        );
        assert_eq!(status, 400);
    }
    assert!(store.load().unwrap().is_none());
    assert!(!client.is_signed_in());
}

#[tokio::test]
async fn exchange_errors_carry_the_service_answer() {
    let server = server().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(auth_error(400, "invalid_grant", "auth/invalid_grant"))
        .expect(1)
        .mount(&server)
        .await;
    let client = client(&server, Arc::new(MemoryTokenStore::new())).await;
    let (result, status, _) = sign_in_with(&client, good_callback).await;
    let error = result.unwrap_err();
    assert_eq!(error.code(), "auth/invalid_grant");
    let error = api(error);
    assert_eq!(error.service, Service::Auth);
    assert_eq!(error.status, 400);
    assert_eq!(error.code, ErrorCode::InvalidGrant);
    assert_eq!(error.request_id.as_deref(), Some("req-auth"));
    assert_eq!(error.message.as_deref(), Some("described"));
    assert_eq!(status, 400);
}

// ---- tokens -----------------------------------------------------------------

#[tokio::test]
async fn restore_loads_refreshes_and_rejects() {
    let server = server().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(token_body(
            "access-1",
            "refresh-1",
            600,
        )))
        .mount(&server)
        .await;

    let empty = client(&server, Arc::new(MemoryTokenStore::new())).await;
    assert!(matches!(empty.restore().await.unwrap(), Restore::SignedOut));
    assert!(!empty.is_signed_in());

    let fresh = client(&server, store_with(stored(600, "refresh-0"))).await;
    assert!(matches!(fresh.restore().await.unwrap(), Restore::SignedIn(u) if u == user()));
    assert!(requests(&server, "/oauth/token").await.is_empty());

    let store = store_with(stored(10, "refresh-0"));
    let stale = client(&server, store.clone()).await;
    let mut events = stale.subscribe();
    assert!(matches!(
        stale.restore().await.unwrap(),
        Restore::SignedIn(_)
    ));
    let sent = form(&requests(&server, "/oauth/token").await[0]);
    assert_eq!(sent["grant_type"], "refresh_token");
    assert_eq!(sent["refresh_token"], "refresh-0");
    assert_eq!(sent["client_id"], APP);
    assert_eq!(store.load().unwrap().unwrap().refresh_token, "refresh-1");
    assert!(matches!(
        next_event(&mut events).await,
        AuthEvent::Refreshed(_)
    ));

    let mut broken = stored(600, "");
    broken.access_token = "a".into();
    let store = store_with(broken);
    let unusable = client(&server, store.clone()).await;
    match unusable.restore().await.unwrap() {
        Restore::InteractionRequired { code } => {
            assert_eq!(code, ErrorCode::Other("stored_session_invalid".into()))
        }
        other => panic!("{other:?}"),
    }
    assert!(store.load().unwrap().is_none());
}

/// Token and assertion endpoints that always succeed.
async fn assertion_server(token_delay: Duration) -> MockServer {
    let server = server().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(token_body("access-1", "refresh-1", 600))
                .set_delay(token_delay),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/auth/player-assertions"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "playerAssertion": "assertion-1", "tokenType": "urn:gridspawn:player-assertion", "expiresIn": 60
        })))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn refresh_happens_only_under_30_seconds() {
    // 40 s left: the stored access token is used as is.
    let server = assertion_server(Duration::ZERO).await;
    let client = client(&server, store_with(stored(40, "refresh-0"))).await;
    client.restore().await.unwrap();
    client.player_assertion(CHALLENGE).await.unwrap();
    assert!(requests(&server, "/oauth/token").await.is_empty());
    let sent = requests(&server, "/v1/auth/player-assertions").await;
    assert_eq!(sent[0].headers["authorization"], "Bearer access-0");

    // 20 s left: restore refreshes, and the assertion uses the new token.
    let server = assertion_server(Duration::ZERO).await;
    let client = self::client(&server, store_with(stored(20, "refresh-0"))).await;
    client.restore().await.unwrap();
    assert_eq!(requests(&server, "/oauth/token").await.len(), 1);
    client.player_assertion(CHALLENGE).await.unwrap();
    assert_eq!(requests(&server, "/oauth/token").await.len(), 1);
    let sent = requests(&server, "/v1/auth/player-assertions").await;
    assert_eq!(sent[0].headers["authorization"], "Bearer access-1");
}

#[tokio::test]
async fn refresh_is_retried_once_after_5xx_and_keeps_the_session() {
    let server = server().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(auth_error(
            503,
            "temporarily_unavailable",
            "service/not_ready",
        ))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(token_body(
            "access-1",
            "refresh-1",
            600,
        )))
        .mount(&server)
        .await;
    let recovered = client(&server, store_with(stored(5, "refresh-0"))).await;
    assert!(matches!(
        recovered.restore().await.unwrap(),
        Restore::SignedIn(_)
    ));
    let sent = requests(&server, "/oauth/token").await;
    assert_eq!(sent.len(), 2);
    assert_eq!(
        form(&sent[1])["refresh_token"],
        "refresh-0",
        "the retry reuses the token"
    );

    let down = MockServer::start().await;
    let oauth = format!("{}/oauth", down.uri());
    Mock::given(method("GET"))
        .and(path("/v1/bootstrap"))
        .respond_with(ResponseTemplate::new(200).set_body_json(bootstrap_body(&down.uri(), &oauth)))
        .mount(&down)
        .await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(auth_error(
            503,
            "temporarily_unavailable",
            "service/not_ready",
        ))
        .mount(&down)
        .await;
    let store = store_with(stored(5, "refresh-0"));
    let kept = client(&down, store.clone()).await;
    let error = api(kept.restore().await.unwrap_err());
    assert_eq!(error.code, ErrorCode::ServiceNotReady);
    assert_eq!(requests(&down, "/oauth/token").await.len(), 2);
    assert!(kept.is_signed_in());
    assert_eq!(kept.current_user(), Some(user()));
    assert!(store.load().unwrap().is_some());
}

#[tokio::test]
async fn refused_refresh_ends_the_session_and_rate_limits_do_not() {
    for (status, oauth, code, expected) in [
        (
            400,
            "invalid_grant",
            "auth/invalid_grant",
            ErrorCode::InvalidGrant,
        ),
        (
            400,
            "invalid_client",
            "auth/invalid_client",
            ErrorCode::InvalidClient,
        ),
    ] {
        let server = server().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(auth_error(status, oauth, code))
            .expect(1)
            .mount(&server)
            .await;
        let store = store_with(stored(5, "refresh-0"));
        let client = client(&server, store.clone()).await;
        let mut events = client.subscribe();
        match client.restore().await.unwrap() {
            Restore::InteractionRequired { code } => assert_eq!(code, expected),
            other => panic!("{other:?}"),
        }
        assert!(store.load().unwrap().is_none());
        assert!(!client.is_signed_in());
        assert!(client.current_user().is_none());
        assert!(matches!(
            next_event(&mut events).await,
            AuthEvent::SessionExpired { code } if code == expected
        ));
        assert!(matches!(
            client.player_assertion(CHALLENGE).await,
            Err(Error::SignInRequired)
        ));
    }

    let server = server().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(auth_error(429, "slow_down", "auth/rate_limited"))
        .mount(&server)
        .await;
    let store = store_with(stored(5, "refresh-0"));
    let client = client(&server, store.clone()).await;
    let error = client.restore().await.unwrap_err();
    assert!(error.is_retryable());
    assert_eq!(api(error).code, ErrorCode::RateLimited);
    assert_eq!(
        requests(&server, "/oauth/token").await.len(),
        1,
        "429 is not retried"
    );
    assert!(client.is_signed_in());
    assert!(store.load().unwrap().is_some());
}

#[tokio::test]
async fn offline_restore_keeps_the_session() {
    let server = MockServer::start().await;
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let oauth = format!("http://{}/oauth", closed.local_addr().unwrap());
    drop(closed);
    Mock::given(method("GET"))
        .and(path("/v1/bootstrap"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(bootstrap_body(&server.uri(), &oauth)),
        )
        .mount(&server)
        .await;

    let valid = client(&server, store_with(stored(600, "refresh-0"))).await;
    assert!(matches!(
        valid.restore().await.unwrap(),
        Restore::SignedIn(_)
    ));

    let store = store_with(stored(-5, "refresh-0"));
    let expired = client(&server, store.clone()).await;
    let error = expired.restore().await.unwrap_err();
    assert!(matches!(error, Error::Network(_)), "{error:?}");
    assert_eq!(error.code(), "network");
    assert!(expired.is_signed_in());
    assert_eq!(expired.current_user().unwrap().display_name, "Player One");
    assert!(matches!(
        expired.player_assertion(CHALLENGE).await,
        Err(Error::Network(_))
    ));
    assert!(expired.is_signed_in());
    assert!(store.load().unwrap().is_some());
}

#[tokio::test]
async fn concurrent_callers_share_one_refresh() {
    let server = assertion_server(Duration::from_millis(300)).await;
    // Restore at 32 s left (no refresh), then let the token age past the margin.
    let client = client(&server, store_with(stored(32, "refresh-0"))).await;
    client.restore().await.unwrap();
    assert!(requests(&server, "/oauth/token").await.is_empty());
    tokio::time::sleep(Duration::from_millis(2100)).await;
    let calls: Vec<_> = (0..5)
        .map(|_| {
            let client = client.clone();
            tokio::spawn(async move { client.player_assertion(CHALLENGE).await })
        })
        .collect();
    for call in calls {
        call.await.unwrap().expect("assertion");
    }
    assert_eq!(requests(&server, "/oauth/token").await.len(), 1);
    let sent = requests(&server, "/v1/auth/player-assertions").await;
    assert_eq!(sent.len(), 5);
    assert!(sent
        .iter()
        .all(|request| request.headers["authorization"] == "Bearer access-1"));
}

/// A store that records whether it was cleared.
struct RecordingStore {
    inner: MemoryTokenStore,
    cleared: Arc<AtomicBool>,
}

impl TokenStore for RecordingStore {
    fn load(&self) -> Result<Option<StoredSession>, StoreError> {
        self.inner.load()
    }
    fn save(&self, session: &StoredSession) -> Result<(), StoreError> {
        self.inner.save(session)
    }
    fn clear(&self) -> Result<(), StoreError> {
        self.cleared.store(true, Ordering::SeqCst);
        self.inner.clear()
    }
}

/// Answers the revoke with 200 only if the store was already cleared.
struct RevokeAfterClear(Arc<AtomicBool>);

impl Respond for RevokeAfterClear {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        if self.0.load(Ordering::SeqCst) {
            ResponseTemplate::new(200).set_body_json(json!({ "ok": true }))
        } else {
            ResponseTemplate::new(418)
        }
    }
}

#[tokio::test]
async fn sign_out_clears_the_store_before_revoking() {
    let server = server().await;
    let cleared = Arc::new(AtomicBool::new(false));
    Mock::given(method("POST"))
        .and(path("/oauth/revoke"))
        .and(query_param("client_id", APP))
        .and(query_param("environment", "development"))
        .respond_with(RevokeAfterClear(cleared.clone()))
        .expect(1)
        .mount(&server)
        .await;
    let store = Arc::new(RecordingStore {
        inner: MemoryTokenStore::new(),
        cleared: cleared.clone(),
    });
    store.inner.save(&stored(600, "refresh-0")).unwrap();
    let client = client(&server, store.clone()).await;
    client.restore().await.unwrap();
    let mut events = client.subscribe();
    client.sign_out().await.expect("sign-out");
    assert!(!client.is_signed_in());
    assert!(store.load().unwrap().is_none());
    assert!(matches!(
        next_event(&mut events).await,
        AuthEvent::SignedOut
    ));
    let sent = form(&requests(&server, "/oauth/revoke").await[0]);
    assert_eq!(sent["token"], "refresh-0");
    assert_eq!(sent["client_id"], APP);

    // A refused revoke is returned; the local session is gone anyway.
    let server = self::server().await;
    Mock::given(method("POST"))
        .and(path("/oauth/revoke"))
        .respond_with(auth_error(400, "invalid_request", "auth/invalid_request"))
        .mount(&server)
        .await;
    let store = store_with(stored(600, "refresh-0"));
    let client = self::client(&server, store.clone()).await;
    client.restore().await.unwrap();
    assert_eq!(
        api(client.sign_out().await.unwrap_err()).code,
        ErrorCode::InvalidRequest
    );
    assert!(!client.is_signed_in());
    assert!(store.load().unwrap().is_none());
    assert_no_origin(&server).await;
}

#[tokio::test]
async fn player_assertions() {
    let server = server().await;
    Mock::given(method("POST"))
        .and(path("/v1/auth/player-assertions"))
        .and(query_param("client_id", APP))
        .and(query_param("environment", "development"))
        .and(header("authorization", "Bearer access-0"))
        .and(body_json(
            json!({ "audience": "game_server", "challenge": CHALLENGE }),
        ))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "playerAssertion": "assertion-secret", "tokenType": "t", "expiresIn": 60
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/auth/player-assertions"))
        .respond_with(auth_error(
            503,
            "temporarily_unavailable",
            "service/not_ready",
        ))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/auth/player-assertions"))
        .respond_with(auth_error(401, "login_required", "auth/login_required"))
        .mount(&server)
        .await;

    let signed_out = client(&server, Arc::new(MemoryTokenStore::new())).await;
    assert!(matches!(
        signed_out.player_assertion(CHALLENGE).await,
        Err(Error::SignInRequired)
    ));

    let client = client(&server, store_with(stored(600, "refresh-0"))).await;
    client.restore().await.unwrap();
    let assertion = client.player_assertion(CHALLENGE).await.unwrap();
    assert_eq!(assertion.as_str(), "assertion-secret");
    assert_eq!(assertion.expires_in(), Duration::from_secs(60));
    assert!(!format!("{assertion:?}").contains("secret"));
    assert_eq!(assertion.into_string(), "assertion-secret");

    let error = client.player_assertion(CHALLENGE).await.unwrap_err();
    assert_eq!(api(error).code, ErrorCode::ServiceNotReady);
    assert_eq!(
        requests(&server, "/v1/auth/player-assertions").await.len(),
        2,
        "never retried"
    );
    let error = client.player_assertion(CHALLENGE).await.unwrap_err();
    assert_eq!(api(error).code, ErrorCode::LoginRequired);
    assert!(client.is_signed_in());
    assert_no_origin(&server).await;
}

// ---- sessions ---------------------------------------------------------------

fn attach_body(expires_in: u64) -> Value {
    json!({
        "sessionId": "sess-1",
        "participant": {
            "participantId": "gsp1.p",
            "identityKind": "server_verified_guest",
            "subject": "guest-sub",
            "displayName": "Wanderer"
        },
        "modules": ["voice"],
        "grants": {
            "voice": { "accessToken": "grant-1", "tokenType": "Bearer", "expiresIn": expires_in, "scope": "voice.join voice.position" }
        }
    })
}

fn grant_body(token: &str, expires_in: u64) -> Value {
    json!({ "accessToken": token, "tokenType": "Bearer", "expiresIn": expires_in, "scope": "voice.join voice.position" })
}

async fn mount_attach(server: &MockServer, expires_in: u64) {
    Mock::given(method("POST"))
        .and(path("/v1/sessions/attach"))
        .and(query_param("appId", APP))
        .and(query_param("environment", "development"))
        .and(body_json(json!({ "participantTicket": TICKET })))
        .respond_with(ResponseTemplate::new(200).set_body_json(attach_body(expires_in)))
        .mount(server)
        .await;
}

const RENEW: &str = "/v1/sessions/sess-1/grants/voice/renew";

#[tokio::test]
async fn attach_and_renew() {
    let server = server().await;
    mount_attach(&server, 300).await;
    Mock::given(method("POST"))
        .and(path(RENEW))
        .and(query_param("appId", APP))
        .and(query_param("environment", "development"))
        .and(header("authorization", "Bearer grant-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(grant_body("grant-2", 300)))
        .expect(1)
        .mount(&server)
        .await;
    let client = client(&server, Arc::new(MemoryTokenStore::new())).await;
    let session = client.attach(TICKET).await.expect("attach");
    assert_eq!(session.id(), "sess-1");
    let participant = session.participant();
    assert_eq!(participant.id, "gsp1.p");
    assert_eq!(participant.identity_kind, IdentityKind::ServerVerifiedGuest);
    assert_eq!(participant.display_name.as_deref(), Some("Wanderer"));
    assert_eq!(session.capabilities(), [Module::Voice]);
    let attach = &requests(&server, "/v1/sessions/attach").await[0];
    assert!(attach.headers.get("authorization").is_none());

    let grant = session.grant(Module::Voice).await.unwrap();
    assert_eq!(grant.as_str(), "grant-1");
    assert_eq!(grant.scope(), "voice.join voice.position");
    assert!(grant.remaining() > Duration::from_secs(290));
    assert!(!format!("{grant:?}").contains("grant-1"));
    assert!(
        requests(&server, RENEW).await.is_empty(),
        "no renewal with 300 s left"
    );

    let mut events = session.subscribe();
    let renewed = session.renew(Module::Voice).await.unwrap();
    assert_eq!(renewed.as_str(), "grant-2");
    assert_eq!(
        session.grant(Module::Voice).await.unwrap().as_str(),
        "grant-2"
    );
    assert!(matches!(
        next_event(&mut events).await,
        GrantEvent::Renewed(Module::Voice)
    ));
    assert!(matches!(
        session.grant(Module::Presence).await,
        Err(Error::InvalidInput {
            field: "module",
            ..
        })
    ));
    assert_no_origin(&server).await;
}

#[tokio::test]
async fn attach_errors_and_validation() {
    let server = server().await;
    let mut body_without_grant = attach_body(300);
    body_without_grant["grants"] = json!({});
    let mut wrong_type = attach_body(300);
    wrong_type["grants"]["voice"]["tokenType"] = json!("mac");
    let mut nameless_guest = attach_body(300);
    nameless_guest["participant"]
        .as_object_mut()
        .unwrap()
        .remove("displayName");
    let mut unknown_module = attach_body(300);
    unknown_module["modules"] = json!(["voice", "maps"]);
    let mut unknown_kind = attach_body(300);
    unknown_kind["participant"]["identityKind"] = json!("robot");
    for body in [
        body_without_grant,
        wrong_type,
        nameless_guest,
        unknown_module,
        unknown_kind,
    ] {
        Mock::given(method("POST"))
            .and(path("/v1/sessions/attach"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .up_to_n_times(1)
            .mount(&server)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/v1/sessions/attach"))
        .respond_with(edge_error(503, "service/not_ready"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/sessions/attach"))
        .respond_with(edge_error(401, "session/ticket_invalid"))
        .mount(&server)
        .await;
    let client = client(&server, Arc::new(MemoryTokenStore::new())).await;
    for _ in 0..5 {
        let error = client.attach(TICKET).await.unwrap_err();
        assert!(matches!(error, Error::InvalidResponse(_)), "{error:?}");
    }
    let error = api(client.attach(TICKET).await.unwrap_err());
    assert_eq!(error.code, ErrorCode::ServiceNotReady);
    assert_eq!(
        requests(&server, "/v1/sessions/attach").await.len(),
        6,
        "never retried"
    );
    let error = api(client.attach(TICKET).await.unwrap_err());
    assert_eq!(error.service, Service::Edge);
    assert_eq!(error.status, 401);
    assert_eq!(error.code, ErrorCode::TicketInvalid);
    assert_eq!(error.request_id.as_deref(), Some("req-edge"));
}

#[tokio::test]
async fn grants_renew_lazily_once() {
    let server = server().await;
    mount_attach(&server, 30).await;
    Mock::given(method("POST"))
        .and(path(RENEW))
        .and(header("authorization", "Bearer grant-1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(grant_body("grant-2", 300))
                .set_delay(Duration::from_millis(200)),
        )
        .expect(1)
        .mount(&server)
        .await;
    let client = client(&server, Arc::new(MemoryTokenStore::new())).await;
    let session = client.attach(TICKET).await.unwrap();
    let calls: Vec<_> = (0..3)
        .map(|_| {
            let session = session.clone();
            tokio::spawn(async move { session.grant(Module::Voice).await })
        })
        .collect();
    for call in calls {
        assert_eq!(call.await.unwrap().unwrap().as_str(), "grant-2");
    }
    assert_eq!(requests(&server, RENEW).await.len(), 1);
}

#[tokio::test]
async fn renewer_renews_at_60_seconds_left_and_retries() {
    let server = server().await;
    // 61 s grants: due one second after attach.
    mount_attach(&server, 61).await;
    Mock::given(method("POST"))
        .and(path(RENEW))
        .respond_with(edge_error(503, "service/not_ready"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(RENEW))
        .respond_with(ResponseTemplate::new(200).set_body_json(grant_body("grant-2", 300)))
        .mount(&server)
        .await;
    let client = client(&server, Arc::new(MemoryTokenStore::new())).await;
    let session = client.attach(TICKET).await.unwrap();
    let mut events = session.subscribe();
    let started = std::time::Instant::now();
    let renewer = session.spawn_renewer();
    assert!(matches!(
        next_event(&mut events).await,
        GrantEvent::Renewed(Module::Voice)
    ));
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(1400),
        "{elapsed:?}: due at 1 s, then a 0.5 s backoff"
    );
    assert_eq!(requests(&server, RENEW).await.len(), 2);
    assert!(renewer.last_error().is_none());
    assert_eq!(
        session.grant(Module::Voice).await.unwrap().as_str(),
        "grant-2"
    );
    // The next renewal is 240 s away.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(requests(&server, RENEW).await.len(), 2);
    assert!(!renewer.is_finished());
    renewer.stop();
}

#[tokio::test]
async fn renewer_reports_expiry() {
    for (status, code, expected) in [
        (401, "session/grant_invalid", ErrorCode::GrantInvalid),
        (404, "session/not_found", ErrorCode::SessionNotFound),
        (
            403,
            "session/capability_denied",
            ErrorCode::CapabilityDenied,
        ),
    ] {
        let server = server().await;
        mount_attach(&server, 61).await;
        Mock::given(method("POST"))
            .and(path(RENEW))
            .respond_with(edge_error(status, code))
            .expect(1)
            .mount(&server)
            .await;
        let client = client(&server, Arc::new(MemoryTokenStore::new())).await;
        let session = client.attach(TICKET).await.unwrap();
        let mut events = session.subscribe();
        let renewer = session.spawn_renewer();
        match next_event(&mut events).await {
            GrantEvent::Expired { module, code } => {
                assert_eq!(module, Module::Voice);
                assert_eq!(code, expected);
            }
            other => panic!("{other:?}"),
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(renewer.is_finished(), "stops with no module left");
        assert_eq!(api(renewer.last_error().unwrap()).code, expected);
        assert!(events.try_recv().is_err(), "one event only");
    }
}

#[tokio::test]
async fn bootstrap_is_checked() {
    for (patch, expected) in [
        (
            json!({ "application": { "appId": "other-game", "displayName": "X" } }),
            "bootstrap",
        ),
        (json!({ "environment": "production" }), "bootstrap"),
        (json!({ "protocol": { "selected": 2 } }), "bootstrap"),
        (json!({ "ok": false }), "bootstrap"),
        (
            json!({ "endpoints": { "api": "https://sdk.gridspawn.net/v1", "oauth": "http://sdk.example/oauth" } }),
            "bootstrap",
        ),
    ] {
        let server = MockServer::start().await;
        let mut body = bootstrap_body(&server.uri(), &format!("{}/oauth", server.uri()));
        for (key, value) in patch.as_object().unwrap() {
            body[key] = value.clone();
        }
        Mock::given(method("GET"))
            .and(path("/v1/bootstrap"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        let error = Client::builder(APP, Environment::Development)
            .sdk_base_url(server.uri().parse().unwrap())
            .token_store(Arc::new(MemoryTokenStore::new()))
            .connect()
            .await
            .unwrap_err();
        assert_eq!(error.code(), expected, "{patch}: {error:?}");
    }
    let error = Client::builder("x", Environment::Development)
        .connect()
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        Error::InvalidInput {
            field: "app id",
            ..
        }
    ));
    let error = Client::builder(APP, Environment::Development)
        .sdk_base_url("http://sdk.example".parse().unwrap())
        .connect()
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        Error::InvalidInput {
            field: "SDK base URL",
            ..
        }
    ));
}
