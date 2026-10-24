//! Game data against a stand-in edge: start-up seeding from the listing,
//! sequence numbers, skipped entries and the one resend, retries, events
//! with their Idempotency-Key, typed errors, local checks, the publishers
//! (per-key results, flush and shutdown, keep-alive, the quota split) and both
//! write answer shapes (all or nothing, and partial). No network.

use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use gridspawn_server::{
    Client, Credential, DataBatch, DataOutcome, Environment, Error, ErrorCode, RetryPolicy,
};
use serde_json::{json, Value};
use url::Url;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, Request, ResponseTemplate,
};

const CRED: &str = "gsc1.gsc_test.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const DATA: &str = "/v1/server/game-data";
const EVENTS: &str = "/v1/server/game-events";

fn quota() -> Value {
    json!({"keys": 1, "maxKeys": 256, "bytes": 10, "maxBytes": 262144})
}

async fn edge() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/bootstrap"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true, "requestId": "r",
            "application": {"appId": "test-app", "displayName": "Test"},
            "environment": "development",
            "protocol": {"selected": 1, "minimum": 1, "maximum": 1},
            "modules": ["game_data"]
        })))
        .mount(&server)
        .await;
    server
}

async fn client(server: &MockServer) -> Client {
    let mut retry = RetryPolicy::default();
    retry.initial_backoff = Duration::from_millis(1);
    retry.max_backoff = Duration::from_millis(5);
    Client::builder(
        "test-app",
        Environment::Development,
        Credential::parse(CRED).unwrap(),
    )
    .sdk_base_url(Url::parse(&server.uri()).unwrap())
    .retry_policy(retry)
    .connect()
    .await
    .unwrap()
}

/// The listing: one live key at seq 41 and a tombstone at seq 57.
async fn listing(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path(DATA))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "keys": [
                {"key": "match.score", "seq": 41, "bytes": 21, "ttlRemaining": null,
                 "updatedAt": "2026-10-23T07:32:00Z"},
                {"key": "lobby.open", "seq": 57, "bytes": 0, "ttlRemaining": null,
                 "updatedAt": "2026-10-23T07:31:00Z", "removed": true}
            ],
            "quota": quota()
        })))
        .mount(server)
        .await;
}

/// Accepts every entry of a write.
fn accept_all(request: &Request) -> ResponseTemplate {
    let body: Value = serde_json::from_slice(&request.body).unwrap();
    let mut keys: Vec<String> = body["set"]
        .as_object()
        .map(|set| set.keys().cloned().collect())
        .unwrap_or_default();
    for entry in body["remove"].as_array().into_iter().flatten() {
        keys.push(entry["key"].as_str().unwrap().to_owned());
    }
    ResponseTemplate::new(200).set_body_json(json!({
        "ok": true, "accepted": keys, "skipped": [], "quota": quota()
    }))
}

fn refused(status: u16, code: &str, field: Option<&str>, reason: Option<&str>) -> ResponseTemplate {
    let mut error = json!({"code": code, "message": "m"});
    if let Some(field) = field {
        error["field"] = json!(field);
    }
    if let Some(reason) = reason {
        error["reason"] = json!(reason);
    }
    ResponseTemplate::new(status)
        .set_body_json(json!({"ok": false, "error": error, "requestId": "req"}))
}

async fn requests(server: &MockServer, verb: &str, p: &str) -> Vec<Request> {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.method.as_str() == verb && r.url.path() == p)
        .collect()
}

async fn bodies(server: &MockServer, verb: &str, p: &str) -> Vec<Value> {
    requests(server, verb, p)
        .await
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

fn api(error: Error) -> gridspawn_server::ApiError {
    match error {
        Error::Api(api) => *api,
        other => panic!("expected an API error, got {other:?}"),
    }
}

#[tokio::test]
async fn the_first_write_seeds_above_every_listed_seq_once() {
    let server = edge().await;
    listing(&server).await;
    Mock::given(method("PUT"))
        .and(path(DATA))
        .respond_with(accept_all)
        .mount(&server)
        .await;
    let client = client(&server).await;
    let data = client.game().data();
    let ack = data
        .set("match.score", json!({"red": 3, "blue": 1}))
        .await
        .unwrap();
    assert_eq!(ack.accepted, ["match.score"]);
    assert!(ack.skipped.is_empty());
    assert_eq!(ack.quota.max_keys, 256);
    data.set_with_ttl("lobby.banner", json!("Overtime"), Duration::from_secs(60))
        .await
        .unwrap();
    // Another Game of the same client is the same writer.
    client.game().data().remove("lobby.banner").await.unwrap();
    client
        .game()
        .data()
        .set_many(
            &DataBatch::new()
                .set("match.round", json!(4))
                .remove("match.score"),
        )
        .await
        .unwrap();

    assert_eq!(requests(&server, "GET", DATA).await.len(), 1, "one listing");
    let sent = bodies(&server, "PUT", DATA).await;
    assert_eq!(
        sent[0],
        json!({"set": {"match.score": {"value": {"red": 3, "blue": 1}, "seq": 58}}}),
        "above the tombstone's 57"
    );
    assert_eq!(
        sent[1],
        json!({"set": {"lobby.banner": {"value": "Overtime", "seq": 59, "ttlSeconds": 60}}})
    );
    assert_eq!(
        sent[2],
        json!({"remove": [{"key": "lobby.banner", "seq": 60}]})
    );
    assert_eq!(
        sent[3],
        json!({"set": {"match.round": {"value": 4, "seq": 61}},
               "remove": [{"key": "match.score", "seq": 62}]})
    );
    // The routes carry the server credential and never an Origin header.
    for request in server.received_requests().await.unwrap() {
        assert!(request.headers.get("origin").is_none());
        if request.url.path() != "/v1/bootstrap" {
            let auth = request
                .headers
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap();
            assert!(auth.starts_with("GridspawnServer "), "{auth}");
        }
    }
}

#[tokio::test]
async fn concurrent_first_writes_share_one_listing() {
    let server = edge().await;
    Mock::given(method("GET"))
        .and(path(DATA))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"keys": [], "quota": quota()}))
                .set_delay(Duration::from_millis(150)),
        )
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path(DATA))
        .respond_with(accept_all)
        .mount(&server)
        .await;
    let client = client(&server).await;
    let (a, b) = (client.game().data(), client.game().data());
    let (ra, rb) = tokio::join!(a.set("a", json!(1)), b.set("b", json!(2)));
    ra.unwrap();
    rb.unwrap();
    assert_eq!(requests(&server, "GET", DATA).await.len(), 1);
    let mut seqs: Vec<u64> = bodies(&server, "PUT", DATA)
        .await
        .iter()
        .map(|b| {
            let set = b["set"].as_object().unwrap();
            set.values().next().unwrap()["seq"].as_u64().unwrap()
        })
        .collect();
    seqs.sort();
    assert_eq!(seqs, [1, 2], "an empty listing starts at 1");
}

#[tokio::test]
async fn a_failed_listing_sends_nothing_and_the_next_write_tries_again() {
    let server = edge().await;
    Mock::given(method("GET"))
        .and(path(DATA))
        .respond_with(refused(403, "server/scope_denied", None, None))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    listing(&server).await;
    Mock::given(method("PUT"))
        .and(path(DATA))
        .respond_with(accept_all)
        .mount(&server)
        .await;
    let client = client(&server).await;
    let error = client.game().data().set("a", json!(1)).await.unwrap_err();
    assert_eq!(api(error).code, ErrorCode::ScopeDenied);
    assert!(
        requests(&server, "PUT", DATA).await.is_empty(),
        "nothing unseeded"
    );
    client.game().data().set("a", json!(1)).await.unwrap();
    assert_eq!(requests(&server, "GET", DATA).await.len(), 2);
    assert_eq!(bodies(&server, "PUT", DATA).await[0]["set"]["a"]["seq"], 58);
}

#[tokio::test]
async fn a_skipped_entry_is_resent_once_above_the_stored_seq() {
    let server = edge().await;
    Mock::given(method("GET"))
        .and(path(DATA))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"keys": [], "quota": quota()})),
        )
        .mount(&server)
        .await;
    // "mine" moved on elsewhere (stored 100) and comes back on the resend;
    // "theirs" stays ahead (another writer owns it).
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    Mock::given(method("PUT"))
        .and(path(DATA))
        .respond_with(move |request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let call = counter.fetch_add(1, Ordering::SeqCst);
            let set = body["set"].as_object().unwrap();
            if call == 0 {
                assert_eq!(set.len(), 3);
                ResponseTemplate::new(200).set_body_json(json!({
                    "ok": true, "accepted": ["kept"],
                    "skipped": [{"key": "mine", "seq": 100}, {"key": "theirs", "seq": 500}],
                    "quota": quota()
                }))
            } else {
                // The resend: only the two skipped keys, above 100.
                assert_eq!(set.len(), 2, "{body}");
                assert!(set.get("kept").is_none());
                let mine = set["mine"]["seq"].as_u64().unwrap();
                assert!(mine > 100, "{mine}");
                ResponseTemplate::new(200).set_body_json(json!({
                    "ok": true, "accepted": ["mine"],
                    "skipped": [{"key": "theirs", "seq": 900}],
                    "quota": quota()
                }))
            }
        })
        .mount(&server)
        .await;
    let client = client(&server).await;
    let ack = client
        .game()
        .data()
        .set_many(
            &DataBatch::new()
                .set("kept", json!(1))
                .set("mine", json!(2))
                .set("theirs", json!(3)),
        )
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2, "one resend, not more");
    let mut accepted = ack.accepted.clone();
    accepted.sort();
    assert_eq!(accepted, ["kept", "mine"]);
    assert_eq!(ack.skipped.len(), 1);
    assert_eq!(
        (ack.skipped[0].key.as_str(), ack.skipped[0].seq),
        ("theirs", 900)
    );
    // The counter now stays above everything it saw.
    Mock::given(method("PUT"))
        .and(path(DATA))
        .respond_with(accept_all)
        .with_priority(1)
        .mount(&server)
        .await;
    client.game().data().set("next", json!(0)).await.unwrap();
    let last = bodies(&server, "PUT", DATA).await.pop().unwrap();
    assert!(last["set"]["next"]["seq"].as_u64().unwrap() > 900, "{last}");
}

#[tokio::test]
async fn a_retried_write_that_already_landed_counts_as_accepted() {
    let server = edge().await;
    Mock::given(method("GET"))
        .and(path(DATA))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"keys": [], "quota": quota()})),
        )
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path(DATA))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    // The retry: the first attempt was stored after all, so the key is
    // skipped with the very seq that was sent.
    Mock::given(method("PUT"))
        .and(path(DATA))
        .respond_with(|request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let seq = body["set"]["a"]["seq"].as_u64().unwrap();
            ResponseTemplate::new(200).set_body_json(json!({
                "ok": true, "accepted": [], "skipped": [{"key": "a", "seq": seq}], "quota": quota()
            }))
        })
        .mount(&server)
        .await;
    let client = client(&server).await;
    let ack = client.game().data().set("a", json!(1)).await.unwrap();
    assert_eq!(ack.accepted, ["a"]);
    assert!(ack.skipped.is_empty());
    let sent = bodies(&server, "PUT", DATA).await;
    assert_eq!(sent.len(), 2, "one retry, no resend");
    assert_eq!(sent[0], sent[1], "the retry repeats the body");
}

#[tokio::test]
async fn list_reports_keys_tombstones_and_quota() {
    let server = edge().await;
    listing(&server).await;
    let client = client(&server).await;
    let listing = client.game().data().list().await.unwrap();
    assert_eq!(listing.keys.len(), 2);
    assert_eq!(listing.keys[0].key, "match.score");
    assert_eq!(listing.keys[0].seq, 41);
    assert!(!listing.keys[0].removed);
    assert!(listing.keys[1].removed);
    assert_eq!(listing.keys[1].ttl_remaining, None);
    assert_eq!(listing.keys[1].updated_at, "2026-10-23T07:31:00Z");
    assert_eq!(listing.quota.max_bytes, 262_144);
}

#[tokio::test]
async fn events_always_carry_an_idempotency_key_and_retries_keep_it() {
    let server = edge().await;
    Mock::given(method("POST"))
        .and(path(EVENTS))
        .respond_with(ResponseTemplate::new(502))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(EVENTS))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "ok": true, "id": "118", "duplicate": false
        })))
        .mount(&server)
        .await;
    let client = client(&server).await;
    let events = client.game().events();
    let receipt = events
        .emit("match.ended", json!({"winner": "Red team"}))
        .await
        .unwrap();
    assert_eq!((receipt.id.as_str(), receipt.duplicate), ("118", false));
    let sent = requests(&server, "POST", EVENTS).await;
    assert_eq!(sent.len(), 2, "one retry");
    let keys: Vec<String> = sent
        .iter()
        .map(|r| {
            r.headers
                .get("idempotency-key")
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(keys[0], keys[1], "the retry keeps the key");
    assert_eq!(keys[0].len(), 22);
    assert!(keys[0]
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'));
    assert_eq!(
        serde_json::from_slice::<Value>(&sent[0].body).unwrap(),
        json!({"name": "match.ended", "payload": {"winner": "Red team"}})
    );

    events
        .emit_with_id("round.won", json!({"round": 4}), "round-4-of-match-81")
        .await
        .unwrap();
    let last = requests(&server, "POST", EVENTS).await.pop().unwrap();
    assert_eq!(
        last.headers.get("idempotency-key").unwrap(),
        "round-4-of-match-81"
    );
}

#[tokio::test]
async fn errors_are_typed_with_field_and_reason() {
    let server = edge().await;
    let client = client(&server).await;
    let data = client.game().data();
    let cases = [
        (
            refused(
                400,
                "data/invalid_value",
                Some("set.board.value"),
                Some("personal_data"),
            ),
            ErrorCode::GameDataInvalidValue,
        ),
        (
            refused(409, "data/quota_exceeded", Some("keys"), None),
            ErrorCode::GameDataQuotaExceeded,
        ),
        (
            refused(403, "data/module_disabled", None, None),
            ErrorCode::GameDataModuleDisabled,
        ),
        (
            refused(403, "data/browser_forbidden", None, None),
            ErrorCode::BrowserForbidden,
        ),
        (
            refused(429, "data/rate_limited", None, None).insert_header("Retry-After", "3"),
            ErrorCode::RateLimited,
        ),
        (
            refused(503, "data/unavailable", None, None),
            ErrorCode::GameDataUnavailable,
        ),
    ];
    for (response, code) in cases {
        server.reset().await;
        // The listing is needed only for the first write's seeding.
        Mock::given(method("GET"))
            .and(path(DATA))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"keys": [], "quota": quota()})),
            )
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path(DATA))
            .respond_with(response)
            .mount(&server)
            .await;
        let error = api(data.set("board", json!({"top": "Ana"})).await.unwrap_err());
        assert_eq!(error.code, code);
        match code {
            ErrorCode::GameDataInvalidValue => {
                assert!(error.is_personal_data());
                assert_eq!(error.field.as_deref(), Some("set.board.value"));
            }
            ErrorCode::GameDataQuotaExceeded => {
                assert_eq!(error.field.as_deref(), Some("keys"));
                assert!(!error.is_personal_data());
            }
            ErrorCode::RateLimited => {
                assert_eq!(error.retry_after, Some(Duration::from_secs(3)));
            }
            _ => {}
        }
    }
}

#[tokio::test]
async fn invalid_input_is_refused_before_sending() {
    let server = edge().await;
    let client = client(&server).await;
    let data = client.game().data();
    let events = client.game().events();
    let mut big = DataBatch::new();
    for i in 0..41 {
        big = big.set(format!("k{i}"), json!(i));
    }
    let checks: Vec<Result<(), Error>> = vec![
        data.set("Bad Key", json!(1)).await.map(|_| ()),
        data.set("a", json!("x".repeat(5000))).await.map(|_| ()),
        data.set_with_ttl("a", json!(1), Duration::from_secs(5))
            .await
            .map(|_| ()),
        data.set_many(&big).await.map(|_| ()),
        data.set_many(&DataBatch::new()).await.map(|_| ()),
        events.emit("Bad Name", json!({})).await.map(|_| ()),
        events.emit("ok", json!([1, 2])).await.map(|_| ()),
        events
            .emit_with_id("ok", json!({}), "short")
            .await
            .map(|_| ()),
        events
            .emit_with_id("ok", json!({}), "has.a.dot.in.it")
            .await
            .map(|_| ()),
    ];
    for result in checks {
        assert!(
            matches!(result, Err(Error::InvalidInput { .. })),
            "{result:?}"
        );
    }
    let sent: Vec<String> = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.url.path().to_owned())
        .filter(|p| p != "/v1/bootstrap")
        .collect();
    assert!(sent.is_empty(), "{sent:?}");
}

#[tokio::test]
async fn the_publisher_sends_the_latest_value_of_each_changed_key() {
    let server = edge().await;
    Mock::given(method("GET"))
        .and(path(DATA))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"keys": [], "quota": quota()})),
        )
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path(DATA))
        .respond_with(accept_all)
        .mount(&server)
        .await;
    let client = client(&server).await;
    let live = client.game().spawn_data_publisher(5);
    for clock in 0..60 {
        live.set("match.clock", json!(clock)).unwrap();
    }
    live.set("match.round", json!(4)).unwrap();
    assert!(live.set("Bad", json!(1)).is_err(), "checked at once");
    tokio::time::sleep(Duration::from_millis(450)).await;
    live.remove("match.round");
    tokio::time::sleep(Duration::from_millis(450)).await;
    assert!(live.last_error().is_none());
    assert!(live.skipped().is_empty());
    drop(live);

    let sent = bodies(&server, "PUT", DATA).await;
    assert!(!sent.is_empty());
    assert_eq!(
        sent[0]["set"]["match.clock"]["value"], 59,
        "the latest value"
    );
    assert_eq!(sent[0]["set"]["match.round"]["value"], 4);
    let removal = sent
        .iter()
        .find(|b| b.get("remove").is_some())
        .expect("a removal was sent");
    assert_eq!(removal["remove"][0]["key"], "match.round");
    // Nothing changed in between: no empty or repeated batches.
    assert_eq!(sent.len(), 2, "{sent:?}");
}

#[tokio::test]
async fn the_publisher_keeps_its_last_error_and_waits_after_429() {
    let server = edge().await;
    Mock::given(method("GET"))
        .and(path(DATA))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"keys": [], "quota": quota()})),
        )
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path(DATA))
        .respond_with(
            refused(429, "data/rate_limited", None, None).insert_header("Retry-After", "1"),
        )
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path(DATA))
        .respond_with(accept_all)
        .mount(&server)
        .await;
    let client = client(&server).await;
    let live = client.game().spawn_data_publisher(5);
    live.set("a", json!(1)).unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    let error = live.last_error().expect("the 429 is kept");
    assert_eq!(api(error).code, ErrorCode::RateLimited);
    live.set("a", json!(2)).unwrap();
    // It waits the Retry-After second, then sends the newest value once.
    tokio::time::sleep(Duration::from_millis(1600)).await;
    assert!(live.last_error().is_none());
    let sent = bodies(&server, "PUT", DATA).await;
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert_eq!(sent[1]["set"]["a"]["value"], 2);
}

#[tokio::test]
async fn the_publisher_reports_keys_another_writer_owns() {
    let server = edge().await;
    Mock::given(method("GET"))
        .and(path(DATA))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"keys": [], "quota": quota()})),
        )
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path(DATA))
        .respond_with(|_: &Request| {
            ResponseTemplate::new(200).set_body_json(json!({
                "ok": true, "accepted": [], "skipped": [{"key": "shared", "seq": 1000}],
                "quota": quota()
            }))
        })
        .mount(&server)
        .await;
    let client = client(&server).await;
    let live = client.game().spawn_data_publisher(5);
    live.set("shared", json!(1)).unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(live.skipped(), ["shared"]);
    assert_eq!(
        bodies(&server, "PUT", DATA).await.len(),
        2,
        "the write and its one resend"
    );
}

#[tokio::test]
async fn the_publisher_sends_a_markers_set() {
    let server = edge().await;
    Mock::given(method("GET"))
        .and(path(DATA))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"keys": [], "quota": quota()})),
        )
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path(DATA))
        .respond_with(accept_all)
        .mount(&server)
        .await;
    let client = client(&server).await;
    let live = client.game().spawn_data_publisher(5);
    let markers = gridspawn_server::Markers::new().push(
        gridspawn_server::Marker::new("flag-red", "Red flag", 120.5, -40.0).kind("objective"),
    );
    live.set_markers("map.live", &markers).unwrap();
    let too_many = (0..65).fold(gridspawn_server::Markers::new(), |set, i| {
        set.push(gridspawn_server::Marker::new(
            format!("m{i}"),
            "P",
            0.0,
            0.0,
        ))
    });
    assert!(
        live.set_markers("map.live", &too_many).is_err(),
        "checked at once"
    );
    tokio::time::sleep(Duration::from_millis(400)).await;
    drop(live);
    let sent = bodies(&server, "PUT", DATA).await;
    assert_eq!(
        sent[0]["set"]["map.live"]["value"],
        json!({"markers": [{"id": "flag-red", "label": "Red flag", "x": 120.5, "y": -40.0, "kind": "objective"}]})
    );
}

// ---- 0.5.0: per-entry outcomes, flush/shutdown, keep-alive, events -------

async fn empty_listing(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path(DATA))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"keys": [], "quota": quota()})),
        )
        .mount(server)
        .await;
}

/// Today's all-or-nothing edge: refuses the first entry whose key starts
/// with "bad" (personal data in its value), otherwise accepts everything.
fn all_or_nothing(request: &Request) -> ResponseTemplate {
    let body: Value = serde_json::from_slice(&request.body).unwrap();
    if let Some(set) = body["set"].as_object() {
        if let Some(key) = set.keys().find(|k| k.starts_with("bad")) {
            return refused(
                400,
                "data/invalid_value",
                Some(&format!("set.{key}.value.name")),
                Some("personal_data"),
            );
        }
    }
    accept_all(request)
}

fn set_keys(body: &Value) -> Vec<String> {
    let mut keys: Vec<String> = body["set"]
        .as_object()
        .map(|set| set.keys().cloned().collect())
        .unwrap_or_default();
    keys.sort();
    keys
}

#[tokio::test]
async fn a_refused_entry_no_longer_holds_back_the_others_on_an_all_or_nothing_edge() {
    let server = edge().await;
    empty_listing(&server).await;
    Mock::given(method("PUT"))
        .and(path(DATA))
        .respond_with(all_or_nothing)
        .mount(&server)
        .await;
    let client = client(&server).await;
    let data = client.game().data();
    let ack = data
        .set_many(
            &DataBatch::new()
                .set("score", json!(1))
                .set("bad.board", json!({"name": "a@b.c"}))
                .set("round", json!(4))
                .remove("old"),
        )
        .await
        .unwrap();
    let mut accepted = ack.accepted.clone();
    accepted.sort();
    assert_eq!(accepted, ["old", "round", "score"]);
    assert_eq!(ack.rejected.len(), 1);
    assert_eq!(ack.rejected[0].key, "bad.board");
    assert_eq!(
        ack.rejected[0].field.as_deref(),
        Some("set.bad.board.value.name")
    );
    assert_eq!(ack.rejected[0].reason.as_deref(), Some("personal_data"));
    let sent = bodies(&server, "PUT", DATA).await;
    assert_eq!(sent.len(), 2, "one round per refused entry");
    assert_eq!(set_keys(&sent[1]), ["round", "score"]);
    assert_eq!(
        sent[0]["set"]["score"]["seq"], sent[1]["set"]["score"]["seq"],
        "nothing was stored: the same numbers again"
    );
    for request in requests(&server, "PUT", DATA).await {
        assert_eq!(request.headers.get("gridspawn-partial").unwrap(), "1");
    }

    // Several refused entries: one round each, the good ones still land.
    let mut batch = DataBatch::new().set("good.a", json!(1));
    for i in 0..4 {
        batch = batch.set(format!("bad{i}"), json!({"name": "x"}));
    }
    let ack = data.set_many(&batch.set("good.b", json!(2))).await.unwrap();
    let mut accepted = ack.accepted.clone();
    accepted.sort();
    assert_eq!(accepted, ["good.a", "good.b"]);
    assert_eq!(ack.rejected.len(), 4);
    assert_eq!(bodies(&server, "PUT", DATA).await.len(), 2 + 5);

    // Every entry refused: the first refusal is the error.
    let error = api(data
        .set("bad.only", json!({"name": "x"}))
        .await
        .unwrap_err());
    assert!(error.is_personal_data());
    assert_eq!(error.field.as_deref(), Some("set.bad.only.value.name"));
}

#[tokio::test]
async fn partial_answers_are_read_entry_by_entry() {
    let server = edge().await;
    empty_listing(&server).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    Mock::given(method("PUT"))
        .and(path(DATA))
        .respond_with(move |request: &Request| {
            assert_eq!(request.headers.get("gridspawn-partial").unwrap(), "1");
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            match counter.fetch_add(1, Ordering::SeqCst) {
                0 => {
                    assert_eq!(set_keys(&body), ["mine", "score", "secret"]);
                    ResponseTemplate::new(200).set_body_json(json!({
                        "ok": true, "accepted": 1,
                        "skipped": [{"key": "mine", "storedSeq": 1000}],
                        "rejected": [{"key": "secret", "field": "set.secret.value", "reason": "personal_data"}],
                        "quota": quota(), "requestId": "req-p"
                    }))
                }
                1 => {
                    // The resend above the stored 1000.
                    assert_eq!(set_keys(&body), ["mine"]);
                    assert!(body["set"]["mine"]["seq"].as_u64().unwrap() > 1000);
                    ResponseTemplate::new(200).set_body_json(json!({
                        "ok": true, "accepted": 1, "skipped": [], "rejected": [],
                        "quota": quota(), "requestId": "req-q"
                    }))
                }
                _ => ResponseTemplate::new(400).set_body_json(json!({
                    "ok": false,
                    "error": {"code": "data/invalid_value", "message": "m",
                              "field": "set.a.value", "reason": "size"},
                    "rejected": [
                        {"key": "a", "field": "set.a.value", "reason": "size"},
                        {"key": "b", "field": "set.b.value", "reason": "depth"}
                    ],
                    "requestId": "req-r"
                })),
            }
        })
        .mount(&server)
        .await;
    let client = client(&server).await;
    let data = client.game().data();
    let ack = data
        .set_many(
            &DataBatch::new()
                .set("score", json!(3))
                .set("secret", json!("x"))
                .set("mine", json!(1)),
        )
        .await
        .unwrap();
    let mut accepted = ack.accepted.clone();
    accepted.sort();
    assert_eq!(accepted, ["mine", "score"]);
    assert_eq!(ack.rejected.len(), 1);
    assert_eq!(
        (
            ack.rejected[0].key.as_str(),
            ack.rejected[0].reason.as_deref()
        ),
        ("secret", Some("personal_data"))
    );
    assert!(ack.skipped.is_empty());
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    // Every entry refused: 400 with `rejected`, one request, the first is the error.
    let error = api(data
        .set_many(&DataBatch::new().set("a", json!(1)).set("b", json!(2)))
        .await
        .unwrap_err());
    assert_eq!(error.code, ErrorCode::GameDataInvalidValue);
    assert_eq!(error.field.as_deref(), Some("set.a.value"));
    assert_eq!(error.reason.as_deref(), Some("size"));
    assert_eq!(calls.load(Ordering::SeqCst), 3, "no rounds in partial mode");
}

#[tokio::test]
async fn the_publisher_reports_each_key_and_starts_from_the_listing() {
    let server = edge().await;
    listing(&server).await;
    Mock::given(method("PUT"))
        .and(path(DATA))
        .respond_with(all_or_nothing)
        .mount(&server)
        .await;
    let client = client(&server).await;
    let live = client.game().spawn_data_publisher(5);
    live.set("score", json!(1)).unwrap();
    live.set("bad.name", json!({"name": "x"})).unwrap();
    live.remove("lobby.open");
    live.flush(Duration::from_secs(5)).await.unwrap();
    assert_eq!(live.seq_base(), Some(57), "the tombstone's 57");
    assert_eq!(client.game().data().seq_base(), Some(57));
    let results = live.results();
    assert_eq!(results.len(), 3, "{results:?}");
    let find = |key: &str| {
        results
            .iter()
            .find(|r| r.key == key)
            .unwrap_or_else(|| panic!("{key}: {results:?}"))
    };
    assert!(
        matches!(find("score").outcome, DataOutcome::Stored { seq } if seq > 57),
        "{results:?}"
    );
    assert!(matches!(
        find("lobby.open").outcome,
        DataOutcome::Removed { .. }
    ));
    match &find("bad.name").outcome {
        DataOutcome::Refused(error) => assert!(error.api().unwrap().is_personal_data()),
        other => panic!("{other:?}"),
    }
    assert!(live.results().is_empty(), "drained");
    assert_eq!(live.dropped_results(), 0);
    let error = live.last_error().expect("the refusal is the last error");
    assert!(error.api().unwrap().is_personal_data());
    // The next clean batch clears it.
    live.set("score", json!(2)).unwrap();
    live.flush(Duration::from_secs(5)).await.unwrap();
    assert!(live.last_error().is_none());
}

#[tokio::test]
async fn shutdown_sends_pending_removals_and_flush_reports_what_is_left() {
    let server = edge().await;
    empty_listing(&server).await;
    Mock::given(method("PUT"))
        .and(path(DATA))
        .respond_with(accept_all)
        .mount(&server)
        .await;
    let client = client(&server).await;
    let live = client.game().spawn_data_publisher(1);
    live.set("match.score", json!(1)).unwrap();
    live.remove("lobby.banner");
    live.shutdown(Duration::from_secs(5)).await.unwrap();
    let sent = bodies(&server, "PUT", DATA).await;
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0]["remove"][0]["key"], "lobby.banner");

    // A slow edge: the flush times out and names the key; it is sent later.
    server.reset().await;
    empty_listing(&server).await;
    Mock::given(method("PUT"))
        .and(path(DATA))
        .respond_with(|request: &Request| accept_all(request).set_delay(Duration::from_millis(800)))
        .mount(&server)
        .await;
    let live = client.game().spawn_data_publisher(5);
    live.set("slow", json!(1)).unwrap();
    let unsent = live.flush(Duration::from_millis(300)).await.unwrap_err();
    assert_eq!(unsent.pending, ["slow"]);
    live.flush(Duration::from_secs(5)).await.unwrap();
    assert!(matches!(
        live.results()[0].outcome,
        DataOutcome::Stored { .. }
    ));
}

#[tokio::test]
async fn over_quota_the_removals_go_again_alone_and_the_sets_are_refused() {
    let server = edge().await;
    empty_listing(&server).await;
    Mock::given(method("PUT"))
        .and(path(DATA))
        .respond_with(|request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            if body.get("set").is_some() {
                refused(409, "data/quota_exceeded", Some("keys"), None)
            } else {
                accept_all(request)
            }
        })
        .mount(&server)
        .await;
    let client = client(&server).await;
    let live = client.game().spawn_data_publisher(5);
    live.set("new.key", json!(1)).unwrap();
    live.remove("old.key");
    live.flush(Duration::from_secs(5)).await.unwrap();
    let results = live.results();
    assert_eq!(results.len(), 2, "{results:?}");
    for result in &results {
        match (result.key.as_str(), &result.outcome) {
            ("old.key", DataOutcome::Removed { .. }) => {}
            ("new.key", DataOutcome::Refused(error)) => {
                assert_eq!(error.api().unwrap().code, ErrorCode::GameDataQuotaExceeded)
            }
            other => panic!("{other:?}"),
        }
    }
    let sent = bodies(&server, "PUT", DATA).await;
    assert_eq!(sent.len(), 2);
    assert!(sent[1].get("set").is_none(), "{}", sent[1]);
}

#[tokio::test]
async fn keep_alive_needs_a_ttl() {
    let server = edge().await;
    let client = client(&server).await;
    let live = client.game().spawn_data_publisher(5);
    assert!(live.keep_alive("never.set").is_err());
    live.set("plain", json!(1)).unwrap();
    assert!(live.keep_alive("plain").is_err());
    match live.keep_alive("plain") {
        Err(Error::InvalidInput { field, .. }) => assert_eq!(field, "ttl"),
        other => panic!("{other:?}"),
    }
    live.set_with_ttl("lobby", json!(1), Duration::from_secs(10))
        .unwrap();
    live.keep_alive("lobby").unwrap();
    live.stop_keep_alive("lobby");
    live.stop();
}

#[tokio::test]
async fn the_event_publisher_keeps_order_and_retries_with_the_same_key() {
    let server = edge().await;
    // The first event is rate limited once; a refused one does not block the next.
    Mock::given(method("POST"))
        .and(path(EVENTS))
        .respond_with(
            refused(429, "data/rate_limited", None, None).insert_header("Retry-After", "1"),
        )
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(EVENTS))
        .respond_with(|request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            if body["name"] == "bad" {
                refused(
                    400,
                    "data/invalid_value",
                    Some("payload"),
                    Some("personal_data"),
                )
            } else {
                ResponseTemplate::new(201)
                    .set_body_json(json!({"ok": true, "id": "9", "duplicate": false}))
            }
        })
        .mount(&server)
        .await;
    let client = client(&server).await;
    let events = client.game().spawn_event_publisher();
    let first = events.emit("round.won", json!({"round": 1})).unwrap();
    let bad = events.emit("bad", json!({"x": 1})).unwrap();
    let mine = events
        .emit_with_id("match.ended", json!({}), "match-81-ended")
        .unwrap();
    assert_eq!(mine, "match-81-ended");
    assert!(
        events.emit("Bad Name", json!({})).is_err(),
        "checked at once"
    );
    assert!(events.emit("ok", json!([1])).is_err());
    assert!(events.emit_with_id("ok", json!({}), "short").is_err());
    let error = events.last_error();
    assert!(error.is_none() || error.unwrap().is_retryable());
    events.shutdown(Duration::from_secs(10)).await.unwrap();

    let keys: Vec<String> = requests(&server, "POST", EVENTS)
        .await
        .iter()
        .map(|r| {
            r.headers
                .get("idempotency-key")
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(
        keys,
        [first.clone(), first, bad, mine],
        "in order; the retry keeps its key"
    );
}

#[tokio::test]
async fn the_event_publisher_reports_each_event() {
    let server = edge().await;
    Mock::given(method("POST"))
        .and(path(EVENTS))
        .respond_with(|request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            if body["name"] == "bad" {
                refused(
                    400,
                    "data/invalid_value",
                    Some("payload"),
                    Some("personal_data"),
                )
            } else {
                ResponseTemplate::new(201)
                    .set_body_json(json!({"ok": true, "id": "7", "duplicate": false}))
            }
        })
        .mount(&server)
        .await;
    let client = client(&server).await;
    let events = client.game().spawn_event_publisher();
    let good = events.emit("round.won", json!({})).unwrap();
    let bad = events.emit("bad", json!({})).unwrap();
    events.flush(Duration::from_secs(5)).await.unwrap();
    assert_eq!(events.queued(), 0);
    let results = events.results();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].id, good);
    assert_eq!(results[0].outcome.as_ref().unwrap().id, "7");
    assert_eq!(results[1].id, bad);
    assert!(results[1]
        .outcome
        .as_ref()
        .unwrap_err()
        .api()
        .unwrap()
        .is_personal_data());
    assert!(events.last_error().is_some());
}
