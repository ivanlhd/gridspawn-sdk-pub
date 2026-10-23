//! Voice rules against a mock edge: paths, bodies, sequence numbers, answers,
//! errors and the position publisher. No network.

use std::time::Duration;

use gridspawn_server::{
    Client, CreateSession, Credential, Environment, Error, ErrorCode, IdempotencyKey,
    ParticipantId, RetryPolicy, Session, VoiceConfig, VoicePlayerState, VoicePosition, VoiceRoom,
};
use serde_json::{json, Value};
use url::Url;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, Request, ResponseTemplate,
};

const CRED: &str = "gsc1.gsc_test.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const SESSION: &str = "/v1/server/sessions/gssn1.s";
const PLAYER: &str = "/v1/server/sessions/gssn1.s/participants/gsp1.a/voice";

async fn mock_edge() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/bootstrap"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true, "requestId": "r",
            "application": {"appId": "test-app", "displayName": "Test"},
            "environment": "development",
            "protocol": {"selected": 1, "minimum": 1, "maximum": 1},
            "modules": ["voice"]
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/server/sessions"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "sessionId": "gssn1.s", "leaseExpiresAt": "2026-10-23T12:00:45Z", "renewAfterSeconds": 15
        })))
        .mount(&server)
        .await;
    server
}

async fn session(server: &MockServer) -> Session {
    let mut retry = RetryPolicy::default();
    retry.initial_backoff = Duration::from_millis(1);
    retry.max_backoff = Duration::from_millis(5);
    let client = Client::builder(
        "test-app",
        Environment::Development,
        Credential::parse(CRED).unwrap(),
    )
    .sdk_base_url(Url::parse(&server.uri()).unwrap())
    .retry_policy(retry)
    .connect()
    .await
    .unwrap();
    client
        .create_session(CreateSession::new(
            "m",
            IdempotencyKey::new("match-key-1").unwrap(),
        ))
        .await
        .unwrap()
}

fn ok(enforced: bool) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({"ok": true, "enforced": enforced}))
}

fn refused(status: u16, code: &str) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(json!({
        "ok": false, "error": {"code": code, "message": "m"}, "requestId": "req"
    }))
}

async fn bodies(server: &MockServer, p: &str) -> Vec<Value> {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r: &Request| r.url.path() == p)
        .map(|r| {
            assert!(!r.headers.contains_key("origin"), "Origin sent");
            serde_json::from_slice(&r.body).unwrap()
        })
        .collect()
}

fn api(error: Error) -> gridspawn_server::ApiError {
    match error {
        Error::Api(api) => *api,
        other => panic!("expected an API error, got {other:?}"),
    }
}

#[tokio::test]
async fn every_route_has_its_method_path_and_body() {
    let server = mock_edge().await;
    Mock::given(method("PUT"))
        .and(path(format!("{SESSION}/voice")))
        .respond_with(ok(false))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path(PLAYER))
        .respond_with(ok(true))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("{PLAYER}/mute")))
        .respond_with(ok(true))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("{SESSION}/voice/positions")))
        .respond_with(ok(true))
        .mount(&server)
        .await;
    let voice = session(&server).await.voice();
    let a = ParticipantId::new("gsp1.a");

    let ack = voice
        .configure(&VoiceConfig::new().room("match", VoiceRoom::channels()))
        .await
        .unwrap();
    assert!(!ack.enforced, "no player in voice yet");
    assert_eq!(
        bodies(&server, &format!("{SESSION}/voice")).await[0]["rooms"]["match"]["mode"],
        "channels"
    );

    let ack = voice
        .set_player_state(&a, &VoicePlayerState::new().channels(["team:red"]))
        .await
        .unwrap();
    assert!(ack.enforced && !ack.overtaken);
    voice
        .set_player_state(&a, &VoicePlayerState::new().alive(false))
        .await
        .unwrap();
    let states = bodies(&server, PLAYER).await;
    assert_eq!(states[0]["channels"], json!(["team:red"]));
    assert_eq!(states[1]["alive"], false);
    assert!(
        states[1]["seq"].as_u64().unwrap() > states[0]["seq"].as_u64().unwrap(),
        "increasing seq"
    );

    voice.mute(&a, true).await.unwrap();
    assert_eq!(
        bodies(&server, &format!("{PLAYER}/mute")).await,
        [json!({"muted": true})]
    );

    voice
        .publish_positions(&[VoicePosition::planar(&a, 1.0, 2.0).yaw(0.25)])
        .await
        .unwrap();
    let batch = &bodies(&server, &format!("{SESSION}/voice/positions")).await[0];
    assert_eq!(
        batch["positions"],
        json!([{"participantId": "gsp1.a", "p": [1.0, 0.0, 2.0], "yaw": 0.25}])
    );
    assert!(batch["seq"].as_u64().unwrap() > states[1]["seq"].as_u64().unwrap());
}

#[tokio::test]
async fn voice_errors_are_typed_with_field_and_reason() {
    let cases = [
        (403, "voice/browser_forbidden", ErrorCode::BrowserForbidden),
        (404, "voice/session_not_found", ErrorCode::SessionNotFound),
        (
            404,
            "voice/participant_not_found",
            ErrorCode::VoiceParticipantNotFound,
        ),
        (403, "voice/module_disabled", ErrorCode::VoiceModuleDisabled),
        (409, "voice/stale_sequence", ErrorCode::VoiceStaleSequence),
        (429, "voice/rate_limited", ErrorCode::RateLimited),
    ];
    for (status, code, expected) in cases {
        let server = mock_edge().await;
        Mock::given(method("PUT"))
            .and(path(PLAYER))
            .respond_with(refused(status, code))
            .mount(&server)
            .await;
        let error = session(&server)
            .await
            .voice()
            .set_player_state(&ParticipantId::new("gsp1.a"), &VoicePlayerState::new())
            .await
            .unwrap_err();
        let error = api(error);
        assert_eq!(error.code, expected, "{code}");
        assert_eq!(error.wire_code, code);
        // A first-attempt 409 is surfaced, and nothing is retried but 5xx.
        assert_eq!(bodies(&server, PLAYER).await.len(), 1, "{code}");
    }

    let server = mock_edge().await;
    Mock::given(method("PUT"))
        .and(path(format!("{SESSION}/voice")))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "ok": false,
            "error": {"code": "voice/invalid_request", "message": "m", "field": "proximity", "reason": "hysteresis"},
            "requestId": "req"
        })))
        .mount(&server)
        .await;
    let error = api(session(&server)
        .await
        .voice()
        .configure(&VoiceConfig::new().room("match", VoiceRoom::open()))
        .await
        .unwrap_err());
    assert_eq!(error.code, ErrorCode::VoiceInvalidRequest);
    assert_eq!(error.field.as_deref(), Some("proximity"));
    assert_eq!(error.reason.as_deref(), Some("hysteresis"));
}

#[tokio::test]
async fn a_retried_update_that_meets_stale_sequence_is_overtaken_not_an_error() {
    let server = mock_edge().await;
    // The first attempt "fails" with 503 (it may have landed), the retry is stale.
    Mock::given(method("PUT"))
        .and(path(PLAYER))
        .respond_with(refused(503, "voice/unavailable"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path(PLAYER))
        .respond_with(refused(409, "voice/stale_sequence"))
        .mount(&server)
        .await;
    let ack = session(&server)
        .await
        .voice()
        .set_player_state(&ParticipantId::new("gsp1.a"), &VoicePlayerState::new())
        .await
        .unwrap();
    assert!(ack.overtaken && !ack.enforced);
    let sent = bodies(&server, PLAYER).await;
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0], sent[1], "the same body and seq on the retry");

    // 503 every time stays an error, retryable.
    let server = mock_edge().await;
    Mock::given(method("POST"))
        .and(path(format!("{PLAYER}/mute")))
        .respond_with(refused(503, "voice/unavailable"))
        .mount(&server)
        .await;
    let error = session(&server)
        .await
        .voice()
        .mute(&ParticipantId::new("gsp1.a"), true)
        .await
        .unwrap_err();
    assert!(error.is_retryable());
    assert_eq!(api(error).code, ErrorCode::VoiceUnavailable);
}

#[tokio::test]
async fn invalid_input_is_refused_before_sending() {
    let server = mock_edge().await;
    let voice = session(&server).await.voice();
    let a = ParticipantId::new("gsp1.a");
    for error in [
        voice.configure(&VoiceConfig::new()).await.unwrap_err(),
        voice
            .set_player_state(&a, &VoicePlayerState::new().channels(["Bad Key"]))
            .await
            .unwrap_err(),
        voice.publish_positions(&[]).await.unwrap_err(),
        voice
            .publish_positions(&[VoicePosition::new(&a, [f64::NAN, 0.0, 0.0])])
            .await
            .unwrap_err(),
    ] {
        assert!(matches!(error, Error::InvalidInput { .. }), "{error:?}");
    }
    let sent = server.received_requests().await.unwrap();
    assert!(
        sent.iter().all(|r| !r.url.path().contains("/voice")),
        "nothing voice-related was sent"
    );
}

#[tokio::test]
async fn the_publisher_sends_the_latest_position_of_each_player() {
    let server = mock_edge().await;
    let positions = format!("{SESSION}/voice/positions");
    Mock::given(method("POST"))
        .and(path(&positions))
        .respond_with(ok(true))
        .mount(&server)
        .await;
    let voice = session(&server).await.voice();
    let (a, b) = (ParticipantId::new("gsp1.a"), ParticipantId::new("gsp1.b"));
    let publisher = voice.spawn_position_publisher(10);
    publisher.update(&a, [1.0, 0.0, 0.0], None).unwrap();
    publisher.update(&a, [2.0, 0.0, 0.0], Some(1.0)).unwrap();
    publisher.update_planar(&b, 5.0, 6.0, None).unwrap();
    assert!(publisher.update(&b, [f64::NAN, 0.0, 0.0], None).is_err());
    tokio::time::sleep(Duration::from_millis(350)).await;
    publisher.remove(&b);
    tokio::time::sleep(Duration::from_millis(250)).await;
    drop(publisher);
    // A batch already on the wire when the task stops may still land.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let sent = bodies(&server, &positions).await;
    assert!(sent.len() >= 4, "about 10 per second, got {}", sent.len());
    assert_eq!(
        sent[0]["positions"],
        json!([
            {"participantId": "gsp1.a", "p": [2.0, 0.0, 0.0], "yaw": 1.0},
            {"participantId": "gsp1.b", "p": [5.0, 0.0, 6.0]}
        ]),
        "coalesced to the latest"
    );
    assert_eq!(
        sent.last().unwrap()["positions"],
        json!([{"participantId": "gsp1.a", "p": [2.0, 0.0, 0.0], "yaw": 1.0}]),
        "b removed"
    );
    let seqs: Vec<u64> = sent.iter().map(|b| b["seq"].as_u64().unwrap()).collect();
    assert!(seqs.windows(2).all(|w| w[1] > w[0]), "{seqs:?}");
    // Stopped: nothing more after the drop.
    let count = sent.len();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(bodies(&server, &positions).await.len(), count);
}

#[tokio::test]
async fn the_publisher_keeps_its_last_error_and_waits_after_429() {
    let server = mock_edge().await;
    let positions = format!("{SESSION}/voice/positions");
    Mock::given(method("POST"))
        .and(path(&positions))
        .respond_with(refused(429, "voice/rate_limited").insert_header("Retry-After", "1"))
        .mount(&server)
        .await;
    let voice = session(&server).await.voice();
    let publisher = voice.spawn_position_publisher(10);
    publisher
        .update(&ParticipantId::new("gsp1.a"), [0.0; 3], None)
        .unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;
    let error = publisher.last_error().expect("an error");
    assert_eq!(api(error).code, ErrorCode::RateLimited);
    // One batch, then the Retry-After pause (1 s) holds the next one.
    assert_eq!(bodies(&server, &positions).await.len(), 1);
}
