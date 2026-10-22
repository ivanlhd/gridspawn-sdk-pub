//! Every error path and the retry rules against a mock server. No network.

use std::time::Duration;

use gridspawn_server::{
    AdmitGuest, Client, CreateSession, Credential, Environment, Error, ErrorCode, IdempotencyKey,
    IdentityKind, Module, Post, RetryPolicy, Service,
};
use serde_json::{json, Value};
use url::Url;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, Request, ResponseTemplate,
};

const CRED: &str = "gsc1.gsc_test.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const POSTS: &str = "/core/v1/game-servers/posts";

fn fast_retry() -> RetryPolicy {
    let mut policy = RetryPolicy::default();
    policy.initial_backoff = Duration::from_millis(1);
    policy.max_backoff = Duration::from_millis(5);
    policy
}

async fn server() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/bootstrap"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true, "requestId": "r",
            "application": {"appId": "test-app", "displayName": "Test"},
            "environment": "development",
            "protocol": {"selected": 1, "minimum": 1, "maximum": 1},
            "modules": ["voice", "presence", "something-new"]
        })))
        .mount(&server)
        .await;
    server
}

async fn client(server: &MockServer) -> Client {
    Client::builder(
        "test-app",
        Environment::Development,
        Credential::parse(CRED).unwrap(),
    )
    .sdk_base_url(Url::parse(&server.uri()).unwrap())
    .club_posts_url(Url::parse(&format!("{}{POSTS}", server.uri())).unwrap())
    .retry_policy(fast_retry())
    .connect()
    .await
    .unwrap()
}

async fn requests(server: &MockServer, p: &str) -> Vec<Request> {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path() == p)
        .collect()
}

/// Every request the crate sent: no Origin header, ever.
async fn assert_no_origin(server: &MockServer) {
    for request in server.received_requests().await.unwrap() {
        assert!(
            !request.headers.contains_key("origin"),
            "Origin sent to {}",
            request.url
        );
    }
}

async fn mount_grant(server: &MockServer, expires_in: u64) {
    Mock::given(method("POST"))
        .and(path("/v1/server/grants/club"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "accessToken": "grant-token", "tokenType": "Bearer",
            "expiresIn": expires_in, "scope": "club.post"
        })))
        .mount(server)
        .await;
}

fn lease(id: &str, renew_after: f64) -> Value {
    json!({"sessionId": id, "leaseExpiresAt": "2026-10-22T12:00:45Z", "renewAfterSeconds": renew_after})
}

fn key(value: &str) -> IdempotencyKey {
    IdempotencyKey::new(value).unwrap()
}

fn api(error: Error) -> gridspawn_server::ApiError {
    match error {
        Error::Api(api) => *api,
        other => panic!("expected an API error, got {other:?}"),
    }
}

#[tokio::test]
async fn bootstrap_is_checked() {
    let server = server().await;
    let client = client(&server).await;
    assert_eq!(client.app_id(), "test-app");
    assert_eq!(client.modules(), &[Module::Voice, Module::Presence]);

    let other = Client::builder(
        "other-app",
        Environment::Development,
        Credential::parse(CRED).unwrap(),
    )
    .sdk_base_url(Url::parse(&server.uri()).unwrap())
    .connect()
    .await;
    assert!(matches!(other, Err(Error::Bootstrap(_))));
    let wrong_env = Client::builder(
        "test-app",
        Environment::Production,
        Credential::parse(CRED).unwrap(),
    )
    .sdk_base_url(Url::parse(&server.uri()).unwrap())
    .connect()
    .await;
    assert!(matches!(wrong_env, Err(Error::Bootstrap(_))));
    assert_no_origin(&server).await;
}

#[tokio::test]
async fn edge_error_codes_are_typed() {
    let cases = [
        (
            401,
            "server/authentication_required",
            ErrorCode::AuthenticationRequired,
        ),
        (403, "server/scope_denied", ErrorCode::ScopeDenied),
        (403, "server/browser_forbidden", ErrorCode::BrowserForbidden),
        (503, "service/not_ready", ErrorCode::ServiceNotReady),
        (
            400,
            "session/invalid_request",
            ErrorCode::SessionInvalidRequest,
        ),
        (404, "session/not_found", ErrorCode::SessionNotFound),
        (
            403,
            "session/capability_denied",
            ErrorCode::CapabilityDenied,
        ),
        (
            401,
            "session/assertion_invalid",
            ErrorCode::AssertionInvalid,
        ),
        (
            409,
            "session/assertion_replayed",
            ErrorCode::AssertionReplayed,
        ),
        (
            400,
            "request/idempotency_required",
            ErrorCode::IdempotencyRequired,
        ),
        (
            409,
            "request/idempotency_conflict",
            ErrorCode::IdempotencyConflict,
        ),
        (
            409,
            "request/idempotency_in_progress",
            ErrorCode::IdempotencyInProgress,
        ),
        (404, "config/app_not_found", ErrorCode::AppNotFound),
        (
            400,
            "config/protocol_incompatible",
            ErrorCode::ProtocolIncompatible,
        ),
    ];
    for (status, code, expected) in cases {
        let server = server().await;
        Mock::given(method("POST"))
            .and(path("/v1/server/sessions"))
            .respond_with(ResponseTemplate::new(status).set_body_json(json!({
                "ok": false, "error": {"code": code, "message": "m"}, "requestId": "req-1"
            })))
            .mount(&server)
            .await;
        let error = client(&server)
            .await
            .create_session(CreateSession::new("match-1", key("match-1-key")))
            .await
            .unwrap_err();
        let error = api(error);
        assert_eq!(error.code, expected, "{code}");
        assert_eq!(error.service, Service::Edge);
        assert_eq!(error.status, status);
        assert_eq!(error.wire_code, code);
        assert_eq!(error.message.as_deref(), Some("m"));
        assert_eq!(error.request_id.as_deref(), Some("req-1"));
        assert_no_origin(&server).await;
    }
}

#[tokio::test]
async fn core_error_codes_are_typed() {
    let cases = [
        (401, "grant_required", ErrorCode::GrantRequired),
        (
            503,
            "grant_keys_unavailable",
            ErrorCode::GrantKeysUnavailable,
        ),
        (
            403,
            "game_server_browser_forbidden",
            ErrorCode::BrowserForbidden,
        ),
        (404, "feature_not_found", ErrorCode::FeatureNotFound),
        (400, "invalid_game_server_json", ErrorCode::InvalidBody),
        (
            400,
            "invalid_idempotency_key",
            ErrorCode::InvalidIdempotencyKey,
        ),
        (
            400,
            "title_too_long",
            ErrorCode::ContentRejected("title_too_long".into()),
        ),
        (409, "announcement_board_full", ErrorCode::BoardFull),
        (409, "announcement_board_busy", ErrorCode::BoardBusy),
        (409, "club_chat_unavailable", ErrorCode::ChatUnavailable),
        (
            503,
            "club_webhook_chat_unavailable",
            ErrorCode::ChatUnavailable,
        ),
        (429, "rate_limited", ErrorCode::RateLimited),
        (
            418,
            "something_new",
            ErrorCode::Other("something_new".into()),
        ),
    ];
    for (status, code, expected) in cases {
        let server = server().await;
        mount_grant(&server, 300).await;
        Mock::given(method("POST"))
            .and(path(POSTS))
            .respond_with(ResponseTemplate::new(status).set_body_json(json!({ "error": code })))
            .mount(&server)
            .await;
        let error = client(&server)
            .await
            .post("smoke", Post::text("hello"), &key("post-key-1"))
            .await
            .unwrap_err();
        let error = api(error);
        assert_eq!(error.code, expected, "{code}");
        assert_eq!(error.service, Service::Core);
        assert_eq!(error.status, status);
        assert!(error.message.is_none());
        assert_no_origin(&server).await;
    }
}

#[tokio::test]
async fn html_429_is_rate_limited_with_retry_after_and_not_retried() {
    let server = server().await;
    mount_grant(&server, 300).await;
    Mock::given(method("POST"))
        .and(path(POSTS))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "30")
                .set_body_string("<html><body>429 Too Many Requests</body></html>"),
        )
        .mount(&server)
        .await;
    let error = client(&server)
        .await
        .post("smoke", Post::text("hello"), &key("post-key-1"))
        .await
        .unwrap_err();
    assert!(error.is_retryable());
    let error = api(error);
    assert_eq!(error.code, ErrorCode::RateLimited);
    assert_eq!(error.retry_after, Some(Duration::from_secs(30)));
    assert_eq!(
        requests(&server, POSTS).await.len(),
        1,
        "429 is not retried"
    );
}

#[tokio::test]
async fn server_errors_are_retried_with_the_same_key_and_body() {
    let server = server().await;
    mount_grant(&server, 300).await;
    Mock::given(method("POST"))
        .and(path(POSTS))
        .respond_with(ResponseTemplate::new(502).set_body_string("bad gateway"))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(POSTS))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "id": "gs-1"})))
        .mount(&server)
        .await;
    let receipt = client(&server)
        .await
        .post("smoke", Post::titled("T", "C"), &key("post-key-1"))
        .await
        .unwrap();
    assert_eq!(receipt.id, "gs-1");
    assert!(!receipt.duplicate);
    let posts = requests(&server, POSTS).await;
    assert_eq!(posts.len(), 3);
    for request in &posts {
        assert_eq!(request.headers["idempotency-key"], "post-key-1");
        assert_eq!(request.headers["authorization"], "Bearer grant-token");
        assert_eq!(request.body, posts[0].body);
    }
    let body: Value = serde_json::from_slice(&posts[0].body).unwrap();
    assert_eq!(
        body,
        json!({"feature": "smoke", "title": "T", "content": "C"})
    );
    assert_no_origin(&server).await;
}

#[tokio::test]
async fn retries_stop_at_max_attempts() {
    let server = server().await;
    Mock::given(method("POST"))
        .and(path("/v1/server/sessions"))
        .respond_with(ResponseTemplate::new(503).set_body_json(
            json!({"ok": false, "error": {"code": "service/not_ready", "message": "m"}}),
        ))
        .mount(&server)
        .await;
    let error = client(&server)
        .await
        .create_session(CreateSession::new("m", key("create-key")))
        .await
        .unwrap_err();
    assert_eq!(api(error).code, ErrorCode::ServiceNotReady);
    assert_eq!(requests(&server, "/v1/server/sessions").await.len(), 3);
}

#[tokio::test]
async fn network_errors_are_retryable() {
    let server = server().await;
    mount_grant(&server, 300).await;
    // A port that was bound and released: connections are refused at once.
    let closed = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let client = Client::builder(
        "test-app",
        Environment::Development,
        Credential::parse(CRED).unwrap(),
    )
    .sdk_base_url(Url::parse(&server.uri()).unwrap())
    .club_posts_url(Url::parse(&format!("http://127.0.0.1:{closed}/posts")).unwrap())
    .retry_policy(fast_retry())
    .connect()
    .await
    .unwrap();
    let error = client
        .post("smoke", Post::text("x"), &key("post-key-1"))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Network(_)), "{error:?}");
    assert!(error.is_retryable());
}

#[tokio::test]
async fn the_grant_is_reused_while_more_than_a_minute_remains() {
    let server = server().await;
    mount_grant(&server, 300).await;
    Mock::given(method("POST"))
        .and(path(POSTS))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "id": "gs-1"})))
        .mount(&server)
        .await;
    let client = client(&server).await;
    client
        .post("smoke", Post::text("1"), &key("post-key-1"))
        .await
        .unwrap();
    client
        .post("smoke", Post::text("2"), &key("post-key-2"))
        .await
        .unwrap();
    assert_eq!(requests(&server, "/v1/server/grants/club").await.len(), 1);
    let grant = &requests(&server, "/v1/server/grants/club").await[0];
    assert_eq!(
        grant.headers["authorization"],
        format!("GridspawnServer {CRED}").as_str()
    );
}

#[tokio::test]
async fn a_short_grant_is_not_reused() {
    let server = server().await;
    mount_grant(&server, 30).await;
    Mock::given(method("POST"))
        .and(path(POSTS))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "id": "gs-1"})))
        .mount(&server)
        .await;
    let client = client(&server).await;
    client
        .post("smoke", Post::text("1"), &key("post-key-1"))
        .await
        .unwrap();
    client
        .post("smoke", Post::text("2"), &key("post-key-2"))
        .await
        .unwrap();
    assert_eq!(requests(&server, "/v1/server/grants/club").await.len(), 2);
}

#[tokio::test]
async fn grant_invalid_refreshes_the_grant_once() {
    let server = server().await;
    mount_grant(&server, 300).await;
    Mock::given(method("POST"))
        .and(path(POSTS))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({"error": "grant_invalid"})))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(POSTS))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"ok": true, "id": "gs-1", "duplicate": true})),
        )
        .mount(&server)
        .await;
    let receipt = client(&server)
        .await
        .post("smoke", Post::text("x"), &key("post-key-1"))
        .await
        .unwrap();
    assert!(receipt.duplicate);
    assert_eq!(requests(&server, "/v1/server/grants/club").await.len(), 2);
    assert_eq!(requests(&server, POSTS).await.len(), 2);
}

#[tokio::test]
async fn grant_invalid_twice_is_returned_without_a_loop() {
    let server = server().await;
    mount_grant(&server, 300).await;
    Mock::given(method("POST"))
        .and(path(POSTS))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({"error": "grant_invalid"})))
        .mount(&server)
        .await;
    let error = client(&server)
        .await
        .post("smoke", Post::text("x"), &key("post-key-1"))
        .await
        .unwrap_err();
    assert_eq!(api(error).code, ErrorCode::GrantInvalid);
    assert_eq!(requests(&server, POSTS).await.len(), 2);
}

#[tokio::test]
async fn invalid_input_never_reaches_the_network() {
    let server = server().await;
    let client = client(&server).await;
    let before = server.received_requests().await.unwrap().len();
    for error in [
        client
            .create_session(CreateSession::new("", key("create-key")))
            .await
            .unwrap_err(),
        client
            .create_session(CreateSession::new("m", key("create-key")).max_participants(0))
            .await
            .unwrap_err(),
        client
            .post("Bad Feature", Post::text("x"), &key("post-key-1"))
            .await
            .unwrap_err(),
        client
            .post("smoke", Post::default(), &key("post-key-1"))
            .await
            .unwrap_err(),
        client
            .session(gridspawn_server::SessionId::new("gssn1.x"))
            .admit_guest(AdmitGuest::new(
                "guest@example.com",
                "Wanderer",
                key("guest-key"),
            ))
            .await
            .unwrap_err(),
    ] {
        assert!(matches!(error, Error::InvalidInput { .. }), "{error:?}");
    }
    assert!(IdempotencyKey::new("short").is_err());
    assert!(IdempotencyKey::new("has space inside").is_err());
    assert_eq!(server.received_requests().await.unwrap().len(), before);
}

#[tokio::test]
async fn sessions_guests_and_tickets() {
    let server = server().await;
    Mock::given(method("POST"))
        .and(path("/v1/server/sessions"))
        .respond_with(ResponseTemplate::new(201).set_body_json(lease("gssn1.abc", 15.0)))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/server/sessions/gssn1.abc/guests"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "participantId": "gsp1.p", "identityKind": "server_verified_guest",
            "subject": "gsg1.s", "displayName": "Wanderer"
        })))
        .mount(&server)
        .await;
    let ticket = format!("gpt1.{}", "t".repeat(43));
    Mock::given(method("POST"))
        .and(path("/v1/server/sessions/gssn1.abc/participants/gsp1.p/tickets"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "participantTicket": ticket, "tokenType": "urn:gridspawn:participant-ticket", "expiresIn": 120
        })))
        .mount(&server)
        .await;
    let client = client(&server).await;
    let session = client
        .create_session(
            CreateSession::new("match-1", key("match-1-key"))
                .region("us-east")
                .max_participants(16)
                .metadata("map", "arena"),
        )
        .await
        .unwrap();
    assert_eq!(session.id().as_str(), "gssn1.abc");
    assert_eq!(
        session.lease().unwrap().renew_after,
        Duration::from_secs(15)
    );
    let create = &requests(&server, "/v1/server/sessions").await[0];
    assert_eq!(create.headers["idempotency-key"], "match-1-key");
    let body: Value = serde_json::from_slice(&create.body).unwrap();
    assert_eq!(
        body,
        json!({"externalSessionId": "match-1", "region": "us-east", "maxParticipants": 16,
               "metadata": {"map": "arena"}})
    );

    let guest = session
        .admit_guest(AdmitGuest::new("player-7f3a", " Wanderer ", key("guest-key-1")).team("red"))
        .await
        .unwrap();
    assert_eq!(guest.identity_kind, IdentityKind::ServerVerifiedGuest);
    assert_eq!(guest.display_name.as_deref(), Some("Wanderer"));
    let admit = &requests(&server, "/v1/server/sessions/gssn1.abc/guests").await[0];
    let body: Value = serde_json::from_slice(&admit.body).unwrap();
    assert_eq!(
        body,
        json!({"externalPlayerId": "player-7f3a", "displayName": "Wanderer", "team": "red"})
    );

    let issued = guest
        .issue_ticket(&[Module::Voice, Module::Voice])
        .await
        .unwrap();
    assert_eq!(issued.as_str(), ticket);
    assert_eq!(issued.expires_in(), Duration::from_secs(120));
    assert!(!format!("{issued:?}").contains(&"t".repeat(43)));
    let request = &requests(
        &server,
        "/v1/server/sessions/gssn1.abc/participants/gsp1.p/tickets",
    )
    .await[0];
    let body: Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(body, json!({"modules": ["voice"]}));
    assert!(guest.issue_ticket(&[]).await.is_err());
    assert_no_origin(&server).await;
}

#[tokio::test]
async fn ticket_issue_is_never_retried() {
    let server = server().await;
    Mock::given(method("POST"))
        .and(path("/v1/server/sessions/gssn1.abc/guests"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "participantId": "gsp1.p", "identityKind": "server_verified_guest",
            "subject": "gsg1.s", "displayName": "Wanderer"
        })))
        .mount(&server)
        .await;
    let tickets = "/v1/server/sessions/gssn1.abc/participants/gsp1.p/tickets";
    Mock::given(method("POST"))
        .and(path(tickets))
        .respond_with(ResponseTemplate::new(503).set_body_json(
            json!({"ok": false, "error": {"code": "service/not_ready", "message": "m"}}),
        ))
        .mount(&server)
        .await;
    let client = client(&server).await;
    let guest = client
        .session(gridspawn_server::SessionId::new("gssn1.abc"))
        .admit_guest(AdmitGuest::new(
            "player-7f3a",
            "Wanderer",
            key("guest-key-1"),
        ))
        .await
        .unwrap();
    let error = guest.issue_ticket(&[Module::Voice]).await.unwrap_err();
    assert_eq!(api(error).code, ErrorCode::ServiceNotReady);
    assert_eq!(requests(&server, tickets).await.len(), 1);
}

#[tokio::test]
async fn the_renewer_renews_and_stops_when_dropped() {
    let server = server().await;
    Mock::given(method("POST"))
        .and(path("/v1/server/sessions"))
        .respond_with(ResponseTemplate::new(201).set_body_json(lease("gssn1.abc", 0.2)))
        .mount(&server)
        .await;
    let renew = "/v1/server/sessions/gssn1.abc/renew";
    Mock::given(method("POST"))
        .and(path(renew))
        .respond_with(ResponseTemplate::new(200).set_body_json(lease("gssn1.abc", 0.2)))
        .mount(&server)
        .await;
    let session = client(&server)
        .await
        .create_session(CreateSession::new("match-1", key("match-1-key")))
        .await
        .unwrap();
    let renewer = session.spawn_renewer();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let renewed = requests(&server, renew).await.len();
    assert!(renewed >= 3, "renewed {renewed} times");
    assert!(renewer.last_error().is_none());
    drop(renewer);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let stopped = requests(&server, renew).await.len();
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(requests(&server, renew).await.len(), stopped);
    assert_no_origin(&server).await;
}

#[test]
fn credentials_are_parsed_and_never_printed() {
    let credential = Credential::parse(format!("  {CRED}\n")).unwrap();
    assert_eq!(credential.id(), "gsc_test");
    for shown in [format!("{credential:?}"), format!("{credential}")] {
        assert!(!shown.contains("AAAA"), "{shown}");
        assert!(shown.contains("gsc1.gsc_test.****"), "{shown}");
    }
    for bad in [
        "",
        "gsc1.ab.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "gsc2.gsc_test.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "gsc1.gsc_test.short",
        "gsc1.gsc_test.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA.A",
    ] {
        assert!(Credential::parse(bad).is_err(), "{bad}");
    }
}

#[test]
fn credentials_load_from_env() {
    std::env::set_var("GRIDSPAWN_SERVER_CREDENTIAL_TEST_ONLY", CRED);
    assert_eq!(
        Credential::from_env("GRIDSPAWN_SERVER_CREDENTIAL_TEST_ONLY")
            .unwrap()
            .id(),
        "gsc_test"
    );
    assert!(Credential::from_env("GRIDSPAWN_SERVER_CREDENTIAL_NOT_SET").is_err());
}

#[cfg(unix)]
#[test]
fn credential_files_must_be_private() {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::temp_dir().join(format!("gridspawn-cred-{}", uuid_like()));
    std::fs::write(&path, format!("{CRED}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let error = Credential::from_file(&path).unwrap_err();
    let message = error.to_string();
    assert!(matches!(error, Error::CredentialFileTooOpen { .. }));
    assert!(
        message.contains(&format!("chmod 600 {}", path.display())),
        "{message}"
    );
    assert!(message.contains("from_file_unchecked"), "{message}");
    assert_eq!(
        Credential::from_file_unchecked(&path).unwrap().id(),
        "gsc_test"
    );
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(Credential::from_file(&path).unwrap().id(), "gsc_test");
    std::fs::remove_file(&path).unwrap();
    assert!(matches!(
        Credential::from_file(&path),
        Err(Error::CredentialUnreadable(_))
    ));
}

fn uuid_like() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}
