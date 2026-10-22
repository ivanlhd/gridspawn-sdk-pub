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
    IdentityKind, Module, Post, Presence,
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
