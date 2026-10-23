//! Live checks against the SDK edge and Core. Skipped unless configured.
//!
//! Gate 1, an application without a game (Gridspawn's test application):
//!   GRIDSPAWN_TEST_APP_ID=… GRIDSPAWN_TEST_CREDENTIAL_FILE=/path/to/0600-file \
//!   cargo test --test live -- --nocapture
//!   (GRIDSPAWN_TEST_SERVER_CREDENTIAL_FILE, the ops smokes' name, works too.)
//! Gate 2, a real game's development key (posts a real test message to its club):
//!   GRIDSPAWN_TEST_GAME_CREDENTIAL_FILE=… GRIDSPAWN_TEST_GAME_APP_ID=game-… GRIDSPAWN_TEST_FEATURE=smoke

use gridspawn_server::{
    AdmitGuest, Client, CreateSession, Credential, Environment, Error, ErrorCode, IdempotencyKey,
    IdentityKind, Module, Post, Presence, Rolloff, VoiceConfig, VoicePlayerState, VoicePosition,
    VoiceRoom,
};

fn env(names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|name| std::env::var(name).ok())
        .filter(|value| !value.is_empty())
}

fn code(error: Error) -> ErrorCode {
    match error {
        Error::Api(api) => api.code,
        other => panic!("expected an API error, got {other:?}"),
    }
}

fn key(prefix: &str) -> IdempotencyKey {
    IdempotencyKey::new(format!("{prefix}-{}", IdempotencyKey::random())).unwrap()
}

#[tokio::test]
async fn live_test_application() {
    let (Some(path), Some(app_id)) = (
        env(&[
            "GRIDSPAWN_TEST_CREDENTIAL_FILE",
            "GRIDSPAWN_TEST_SERVER_CREDENTIAL_FILE",
        ]),
        env(&["GRIDSPAWN_TEST_APP_ID"]),
    ) else {
        eprintln!("skipped: GRIDSPAWN_TEST_CREDENTIAL_FILE / GRIDSPAWN_TEST_APP_ID not set");
        return;
    };
    let client = Client::builder(
        app_id,
        Environment::Development,
        Credential::from_file(&path).expect("credential"),
    )
    .connect()
    .await
    .expect("bootstrap");
    eprintln!("connected: {client:?}, modules {:?}", client.modules());

    // 02 §1-2: create, the same key again, renew.
    let create_key = key("rust-live");
    let request = CreateSession::new(create_key.as_str(), create_key.clone())
        .region("local-test")
        .max_participants(4)
        .metadata("mode", "rust-crate-live");
    let session = client
        .create_session(request.clone())
        .await
        .expect("create");
    let again = client.create_session(request).await.expect("create again");
    assert_eq!(again.id(), session.id(), "same key, same session");
    let lease = session.renew().await.expect("renew");
    assert_eq!(&lease.session_id, session.id());
    eprintln!("session ok, renew_after {:?}", lease.renew_after);

    // 02 §4-5: a guest twice with one key, presence refused, voice ticket.
    let guest_key = key("rust-guest");
    let guest = session
        .admit_guest(AdmitGuest::new(
            format!("fixture-{}", guest_key.as_str()),
            "Fixture Guest",
            guest_key.clone(),
        ))
        .await
        .expect("guest");
    let same = session
        .admit_guest(AdmitGuest::new(
            format!("fixture-{}", guest_key.as_str()),
            "Fixture Guest",
            guest_key,
        ))
        .await
        .expect("guest again");
    assert_eq!(same.id, guest.id);
    assert_eq!(guest.identity_kind, IdentityKind::ServerVerifiedGuest);
    assert_eq!(
        code(guest.issue_ticket(&[Module::Presence]).await.unwrap_err()),
        ErrorCode::CapabilityDenied
    );
    let ticket = guest.issue_ticket(&[Module::Voice]).await.expect("ticket");
    assert!(ticket.as_str().starts_with("gpt1."));
    eprintln!("guest and voice ticket ok ({ticket:?})");

    // 03 server path: guests have no Gridspawn profile, so no presence; the
    // same through a handle made from the id.
    let refused = guest
        .set_presence(&Presence::new().details("Live check"))
        .await
        .unwrap_err();
    eprintln!("guest presence: {refused}");
    assert_eq!(code(refused), ErrorCode::PresenceGuestNotSupported);
    let by_id = session.participant(guest.id.clone());
    assert_eq!(
        code(by_id.clear_presence().await.unwrap_err()),
        ErrorCode::PresenceGuestNotSupported
    );
    eprintln!("presence refused for the guest on both calls, as expected");

    // 05: the grant works and is reused; the test app has no game, so posts
    // answer feature_not_found.
    for _ in 0..2 {
        let error = client
            .post(
                "rust-live-check",
                Post::text("not posted"),
                &key("rust-post"),
            )
            .await
            .unwrap_err();
        assert_eq!(code(error), ErrorCode::FeatureNotFound);
    }
    eprintln!("club grant ok; post answered feature_not_found as expected");
}

/// The voice rules against development (the test application has voice on).
/// Named rooms are off there, so the managed room is `match`. Whether the
/// rules are enforced once a player joins voice is checked by the voice
/// crate's live tests, which run a voice client.
#[tokio::test]
async fn live_voice_rules() {
    let (Some(path), Some(app_id)) = (
        env(&[
            "GRIDSPAWN_TEST_CREDENTIAL_FILE",
            "GRIDSPAWN_TEST_SERVER_CREDENTIAL_FILE",
        ]),
        env(&["GRIDSPAWN_TEST_APP_ID"]),
    ) else {
        eprintln!("skipped: GRIDSPAWN_TEST_CREDENTIAL_FILE / GRIDSPAWN_TEST_APP_ID not set");
        return;
    };
    let credential = Credential::from_file(&path).expect("credential");
    let client = Client::builder(app_id, Environment::Development, credential)
        .connect()
        .await
        .expect("bootstrap");
    let create_key = key("rust-voice");
    let session = client
        .create_session(CreateSession::new(create_key.as_str(), create_key.clone()))
        .await
        .expect("create");
    let _renewer = session.spawn_renewer();
    let guest_key = key("rust-voice-guest");
    let guest = session
        .admit_guest(AdmitGuest::new(
            format!("fixture-{}", guest_key.as_str()),
            "Voice Fixture",
            guest_key,
        ))
        .await
        .expect("guest");
    let voice = session.voice();

    let ack = voice
        .configure(
            &VoiceConfig::new().room(
                "match",
                VoiceRoom::hybrid(2.0, 20.0)
                    .rolloff(Rolloff::Inverse)
                    .hysteresis(2.0),
            ),
        )
        .await
        .expect("configure");
    eprintln!("configure: {ack:?}");
    assert!(
        !ack.enforced,
        "no player has joined voice: no voice server yet"
    );

    let ack = voice
        .set_player_state(&guest.id, &VoicePlayerState::new().channels(["team:red"]))
        .await
        .expect("state");
    assert!(!ack.overtaken);
    let ack = voice.mute(&guest.id, true).await.expect("mute");
    eprintln!("state and mute: {ack:?}");
    voice.mute(&guest.id, false).await.expect("unmute");
    voice
        .publish_positions(&[VoicePosition::planar(&guest.id, 1.0, 2.0).yaw(0.5)])
        .await
        .expect("positions");

    // The publisher, for half a second.
    let publisher = voice.spawn_position_publisher(10);
    publisher
        .update(&guest.id, [3.0, 0.0, 4.0], Some(0.0))
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    assert!(
        publisher.last_error().is_none(),
        "{:?}",
        publisher.last_error()
    );
    drop(publisher);
    eprintln!("positions and publisher ok");

    // Refusals: an unknown participant, and an older sequence number.
    let stranger = gridspawn_server::ParticipantId::new("gsp1.notadmittedhere");
    assert_eq!(
        code(
            voice
                .set_player_state(&stranger, &VoicePlayerState::new())
                .await
                .unwrap_err()
        ),
        ErrorCode::VoiceParticipantNotFound
    );
    let stale = raw_state(
        &client_parts(&path),
        session.id().as_str(),
        guest.id.as_str(),
        1,
    )
    .await;
    assert_eq!(stale, (409, "voice/stale_sequence".to_owned()));
    let unknown_field = raw_state(
        &client_parts(&path),
        session.id().as_str(),
        guest.id.as_str(),
        0,
    )
    .await;
    assert_eq!(unknown_field, (400, "voice/invalid_request".to_owned()));
    eprintln!("voice refusals as expected");
}

/// The server credential's header value and the edge, for the raw requests
/// that the crate never sends (an old sequence number, an unknown field).
fn client_parts(path: &str) -> (String, String) {
    let credential = std::fs::read_to_string(path).expect("credential");
    (
        format!("GridspawnServer {}", credential.trim()),
        "https://sdk.gridspawn.net".to_owned(),
    )
}

/// PUT a player state by hand: `seq` 1 (older than anything the crate
/// sends), or with `seq` 0 an unknown field. Returns (status, error code).
async fn raw_state(
    (authorization, base): &(String, String),
    session: &str,
    participant: &str,
    seq: u64,
) -> (u16, String) {
    let body = if seq == 0 {
        serde_json::json!({"channels": [], "unknownField": true})
    } else {
        serde_json::json!({"seq": seq, "channels": []})
    };
    let response = reqwest::Client::new()
        .put(format!(
            "{base}/v1/server/sessions/{session}/participants/{participant}/voice"
        ))
        .header("Authorization", authorization)
        .json(&body)
        .send()
        .await
        .expect("raw request");
    let status = response.status().as_u16();
    let value: serde_json::Value = response.json().await.expect("JSON answer");
    (
        status,
        value["error"]["code"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
    )
}

/// Game data (protocol 06) on the test application's development data: the
/// listing, set with and without a TTL, the start-up seeding, an event and
/// its duplicate, the personal-data refusal, and removal. Skipped (with the
/// reason) when the key lacks `game_data.write` or game data is off.
#[tokio::test]
async fn live_game_data() {
    use serde_json::json;
    use std::time::Duration;
    let (Some(path), Some(app_id)) = (
        env(&[
            "GRIDSPAWN_TEST_CREDENTIAL_FILE",
            "GRIDSPAWN_TEST_SERVER_CREDENTIAL_FILE",
        ]),
        env(&["GRIDSPAWN_TEST_APP_ID"]),
    ) else {
        eprintln!("skipped: GRIDSPAWN_TEST_CREDENTIAL_FILE / GRIDSPAWN_TEST_APP_ID not set");
        return;
    };
    let client = Client::builder(
        app_id,
        Environment::Development,
        Credential::from_file(&path).expect("credential"),
    )
    .connect()
    .await
    .expect("bootstrap");
    let data = client.game().data();
    let before = match data.list().await {
        Ok(listing) => listing,
        Err(Error::Api(api))
            if matches!(
                api.code,
                ErrorCode::ScopeDenied | ErrorCode::GameDataModuleDisabled
            ) =>
        {
            eprintln!("skipped: {} ({})", api.code, api.status);
            return;
        }
        Err(error) => panic!("listing: {error:?}"),
    };
    eprintln!(
        "listing: {} keys, quota {:?}",
        before.keys.len(),
        before.quota
    );
    let ack = data
        .set("sdk-live.rust", json!({"round": 4, "leader": "Red team"}))
        .await
        .expect("set");
    assert_eq!(ack.accepted, ["sdk-live.rust"], "{ack:?}");
    assert!(ack.skipped.is_empty(), "{ack:?}");
    data.set_with_ttl(
        "sdk-live.banner",
        json!("Overtime"),
        Duration::from_secs(60),
    )
    .await
    .expect("set with ttl");
    let after = data.list().await.expect("listing again");
    let mine = after
        .keys
        .iter()
        .find(|k| k.key == "sdk-live.rust")
        .expect("listed");
    let banner = after
        .keys
        .iter()
        .find(|k| k.key == "sdk-live.banner")
        .expect("listed");
    assert!(!mine.removed && mine.ttl_remaining.is_none());
    assert!(banner.ttl_remaining.is_some(), "{banner:?}");
    let seeded_above = before.keys.iter().map(|k| k.seq).max().unwrap_or(0);
    assert!(mine.seq > seeded_above, "{} > {seeded_above}", mine.seq);
    eprintln!(
        "set: sdk-live.rust seq {} > listed max {seeded_above}; banner seq {}, ttl_remaining {:?}",
        mine.seq, banner.seq, banner.ttl_remaining
    );

    let refused = data
        .set(
            "sdk-live.private",
            json!({"contact": "someone@example.com"}),
        )
        .await
        .expect_err("personal data");
    let api = refused.api().expect("an API error");
    assert!(api.is_personal_data(), "{api:?}");
    eprintln!("personal data refused, field {:?}", api.field);

    let id = format!("sdk-live-{}", &IdempotencyKey::random().as_str()[..8]);
    let events = client.game().events();
    let first = events
        .emit_with_id("sdk-live.round-won", json!({"round": 4}), &id)
        .await
        .expect("event");
    let again = events
        .emit_with_id("sdk-live.round-won", json!({"round": 4}), &id)
        .await
        .expect("event again");
    assert!(!first.duplicate && again.duplicate, "{first:?} {again:?}");
    assert_eq!(first.id, again.id);
    let generated = events
        .emit("sdk-live.generated", json!({}))
        .await
        .expect("event with a generated key");
    eprintln!(
        "events: id {} (duplicate {}), repeat id {} (duplicate {}), generated-key id {} (duplicate {})",
        first.id, first.duplicate, again.id, again.duplicate, generated.id, generated.duplicate
    );

    for key in ["sdk-live.rust", "sdk-live.banner"] {
        let ack = data.remove(key).await.expect("remove");
        assert_eq!(ack.accepted, [key]);
    }
    let last = data.list().await.expect("final listing");
    let ours: Vec<_> = last
        .keys
        .iter()
        .filter(|k| k.key.starts_with("sdk-live."))
        .collect();
    assert!(ours.iter().all(|k| k.removed));
    for k in &ours {
        eprintln!(
            "final: {} seq {} removed {} ttl_remaining {:?}",
            k.key, k.seq, k.removed, k.ttl_remaining
        );
    }
    eprintln!("final quota {:?}", last.quota);
    eprintln!("game data ok: set, ttl, seeding, personal data, events, removal");
}

#[tokio::test]
async fn live_game_post() {
    let (Some(path), Some(app_id), Some(feature)) = (
        env(&["GRIDSPAWN_TEST_GAME_CREDENTIAL_FILE"]),
        env(&["GRIDSPAWN_TEST_GAME_APP_ID"]),
        env(&["GRIDSPAWN_TEST_FEATURE"]),
    ) else {
        eprintln!("skipped: GRIDSPAWN_TEST_GAME_CREDENTIAL_FILE / _APP_ID / GRIDSPAWN_TEST_FEATURE not set");
        return;
    };
    let client = Client::builder(
        app_id,
        Environment::Development,
        Credential::from_file(&path).expect("credential"),
    )
    .connect()
    .await
    .expect("bootstrap");
    let post_key = key("rust-smoke");
    let body = Post::titled(
        "Smoke test (Rust crate)",
        "Posted by the gridspawn-server live test.",
    );
    let first = client
        .post(&feature, body.clone(), &post_key)
        .await
        .expect("post");
    assert!(!first.duplicate);
    let second = client
        .post(&feature, body, &post_key)
        .await
        .expect("post again");
    assert!(second.duplicate);
    assert_eq!(second.id, first.id);
    let error = client
        .post("no-such-feature", Post::text("x"), &key("rust-smoke"))
        .await
        .unwrap_err();
    assert_eq!(code(error), ErrorCode::FeatureNotFound);
    eprintln!("game post ok: {} then duplicate", first.id);
}
