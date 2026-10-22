//! Live checks against the SDK edge. Skipped unless configured. No browser:
//! `gridspawn-server` creates the session, admits a guest and issues the ticket.
//!
//!   GRIDSPAWN_TEST_APP_ID=… GRIDSPAWN_TEST_CREDENTIAL_FILE=/path/to/0600-file \
//!   cargo test --test live -- --nocapture
//!
//! The application must allow guests and the Voice module. The renewer check
//! waits for one real cycle: grants live 5 minutes and are renewed at 60 s
//! left, so the test takes about 4 minutes.

use std::{sync::Arc, time::Duration};

use gridspawn_client::{
    Client, Environment, Error, ErrorCode, GrantEvent, IdentityKind, MemoryTokenStore, Module,
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

#[tokio::test]
async fn live_attach_renew_and_renewer() {
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

    // The game server's side.
    let server = gridspawn_server::Client::builder(
        app_id.clone(),
        gridspawn_server::Environment::Development,
        gridspawn_server::Credential::from_file(&path).expect("credential"),
    )
    .connect()
    .await
    .expect("server bootstrap");
    let key = gridspawn_server::IdempotencyKey::random();
    let session = server
        .create_session(gridspawn_server::CreateSession::new(
            format!("client-live-{key}"),
            key,
        ))
        .await
        .expect("create session");
    let _lease = session.spawn_renewer();
    let guest_key = gridspawn_server::IdempotencyKey::random();
    let guest = session
        .admit_guest(gridspawn_server::AdmitGuest::new(
            format!("fixture-{guest_key}"),
            "Fixture Guest",
            guest_key,
        ))
        .await
        .expect("admit guest");
    let ticket = guest
        .issue_ticket(&[gridspawn_server::Module::Voice])
        .await
        .expect("ticket");

    // The game client's side.
    let client = Client::builder(app_id, Environment::Development)
        .token_store(Arc::new(MemoryTokenStore::new()))
        .connect()
        .await
        .expect("client bootstrap");
    eprintln!("connected, modules {:?}", client.modules());
    assert!(matches!(
        client
            .player_assertion("live-check-challenge-0123456789-abcdef")
            .await,
        Err(Error::SignInRequired)
    ));
    // 03: bootstrap advertises the presence endpoint; without a signed-in
    // player the client path refuses locally (a guest cannot get a presence
    // grant, so the session path is not reachable from this test).
    let presence_url = client.presence_url().expect("endpoints.presence");
    assert_eq!(presence_url.scheme(), "https");
    eprintln!("presence endpoint path: {}", presence_url.path());
    assert!(matches!(
        client
            .presence()
            .set(gridspawn_client::Presence::new())
            .await,
        Err(Error::SignInRequired)
    ));

    let attached = client.attach(ticket.as_str()).await.expect("attach");
    let participant = attached.participant();
    assert_eq!(participant.id, guest.id.as_str());
    assert_eq!(participant.identity_kind, IdentityKind::ServerVerifiedGuest);
    assert_eq!(participant.display_name.as_deref(), Some("Fixture Guest"));
    assert_eq!(attached.capabilities(), [Module::Voice]);
    eprintln!("attached: {attached:?}");

    assert_eq!(
        code(client.attach(ticket.as_str()).await.unwrap_err()),
        ErrorCode::TicketInvalid,
        "a ticket is single-use"
    );
    let forged = format!("gpt1.{}", "A".repeat(43));
    assert_eq!(
        code(client.attach(&forged).await.unwrap_err()),
        ErrorCode::TicketInvalid
    );

    let first = attached.grant(Module::Voice).await.expect("grant");
    assert!(first.remaining() > Duration::from_secs(200));
    assert!(first.scope().split(' ').any(|scope| scope == "voice.join"));
    let renewed = attached.renew(Module::Voice).await.expect("renew");
    assert_ne!(renewed.as_str(), first.as_str());
    eprintln!("renewed: {renewed:?}");

    let mut events = attached.subscribe();
    let renewer = attached.spawn_renewer();
    eprintln!("waiting for the renewer (about 4 minutes)");
    let event = tokio::time::timeout(Duration::from_secs(320), events.recv())
        .await
        .expect("a renewal within the grant's lifetime")
        .expect("event");
    assert!(
        matches!(event, GrantEvent::Renewed(Module::Voice)),
        "{event:?}"
    );
    assert!(renewer.last_error().is_none());
    let current = attached.grant(Module::Voice).await.expect("grant");
    assert_ne!(current.as_str(), renewed.as_str());
    assert!(current.remaining() > Duration::from_secs(200));
    eprintln!("renewer cycle ok: {current:?}");
}
